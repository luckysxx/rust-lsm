//! 被测对象。基准驱动只通过 `KvTarget` 操作它，换一个实现就能在相同负载下对照。

use std::collections::BTreeMap;
use std::hint::black_box;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use lsm::compaction::CompactionOptions;
use lsm::db::Db;
use lsm::iterators::StorageIterator;
use lsm::lsm_storage::{LsmStorageOptions, WriteOptions};
use lsm::manifest::{MANIFEST_FILE_NAME, Manifest};
use lsm::mem_table::{MemTable, MemTableIterator};
use lsm::stats::StatsSnapshot;

/// 多个线程会同时调用同一个被测对象，所以要求实现者是 `Sync`。
pub trait KvTarget: Sync {
    /// 写进报告里的名字。
    fn name(&self) -> &'static str;

    fn put(&self, key: &[u8], value: &[u8]);

    /// 返回 value 的副本；key 不存在时返回 `None`。
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;

    /// 从 `start`（包含）起按 key 的顺序读最多 `limit` 条记录，返回实际读到的条数。
    fn scan(&self, start: &[u8], limit: usize) -> usize;

    /// 写进报告里的补充说明，例如被测对象的配置与结束时的内部状态。
    fn details(&self) -> Option<String> {
        None
    }

    /// 引擎内部的计数器。不写磁盘的对象没有，返回 `None`。
    fn stats(&self) -> Option<StatsSnapshot> {
        None
    }

    /// 等后台工作做完再返回。没有后台工作的对象直接返回。
    fn wait_for_background(&self) {}

    /// 把磁盘上的全部 SST 合并成一层。没有 compaction 的对象什么也不做。
    fn compact(&self) {}

    /// 此刻磁盘上的 SST：L0、L1 各有几个文件，一共多少字节。不写磁盘的对象没有，返回 `None`。
    fn sst_layout(&self) -> Option<SstLayout> {
        None
    }
}

/// 某一时刻磁盘上的 SST 怎样分布。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SstLayout {
    pub l0_files: usize,
    /// L1 往下各层的文件数，第 0 项是 L1。
    pub level_files: Vec<usize>,
    /// 目录里全部 SST 文件的长度之和。
    pub total_bytes: u64,
}

/// 内存基线：一把互斥锁保护的 `BTreeMap`。
///
/// 没有 WAL、没有文件，重启即丢。它给出"只在内存里维护一张有序表"的
/// 吞吐与延迟，引擎的数字与它相比，差出来的部分就是持久化与分层的代价。
#[derive(Default)]
pub struct BTreeMapTarget {
    map: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl BTreeMapTarget {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvTarget for BTreeMapTarget {
    fn name(&self) -> &'static str {
        "btreemap"
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        let mut map = self.map.lock().unwrap();
        map.insert(key.to_vec(), value.to_vec());
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let map = self.map.lock().unwrap();
        map.get(key).cloned()
    }

    fn scan(&self, start: &[u8], limit: usize) -> usize {
        let map = self.map.lock().unwrap();
        let range = (Bound::Included(start), Bound::Unbounded);
        let mut count = 0;
        for (key, value) in map.range::<[u8], _>(range).take(limit) {
            // 让编译器无法把"读出来却没用"的访问优化掉。
            black_box(key.len() + value.len());
            count += 1;
        }
        count
    }
}

/// 引擎的 memtable 单独拿出来：一张并发跳表，没有 WAL、没有文件。
///
/// `MemTable` 的 `put` 与 `get` 只需要 `&self`，本身可以被多个线程同时调用，
/// 所以这里不加锁。与 `BTreeMapTarget` 相比，差别就是"一把锁加平衡树"和
/// "无锁跳表"。
pub struct MemTableTarget {
    mem_table: Arc<MemTable>,
}

impl MemTableTarget {
    pub fn new() -> Self {
        Self {
            mem_table: Arc::new(MemTable::new(0)),
        }
    }
}

impl KvTarget for MemTableTarget {
    fn name(&self) -> &'static str {
        "memtable"
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.mem_table.put(key, value).expect("写入失败");
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.mem_table.get(key).map(|value| value.to_vec())
    }

    fn scan(&self, start: &[u8], limit: usize) -> usize {
        let table = Arc::clone(&self.mem_table);
        let mut iter = MemTableIterator::new(table, Bound::Included(start), Bound::Unbounded);
        read_up_to(&mut iter, limit)
    }
}

/// 完整的引擎：memtable 写满即冻结，后台线程把冻结的表写成 L0 的 SST；`config.enable_wal`
/// 时写入先追加到 WAL，`config.sync_writes` 时每次写入还要同步。
///
/// `config.compaction` 为 `None` 时没有自动的 compaction：L0 的文件只增不减，只有执行
/// `compact` 负载时才全部合并到最底层。给了参数时，后台线程按简单分层策略反复合并。
/// 点查与扫描要逐个去看 L0 的文件，靠每个文件的首末 key 与 Bloom filter 排除，
/// L1 往下每层只看一个文件；读块先查块缓存。
/// 写入太快时冻结的表会在内存里排队，L0 的文件也会积压，还没有让写入等一等的机制；
/// 每段负载结束后基准驱动等后台的 flush 与合并都做完，再开始下一段。
/// 与 `MemTableTarget` 相比，差别是多了状态快照、冻结、落盘，以及跨表和文件的查找与归并。
pub struct EngineTarget {
    db: Db,
    config: EngineConfig,
    /// 引擎所在的临时目录，随被测对象一起清理。
    dir: tempfile::TempDir,
}

/// 引擎作为被测对象时的配置。其余选项（块大小 4 KB、SST 目标大小与 memtable 上限相同）固定。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// memtable 的上限，字节。compaction 写出的每个 SST 也是这么大。
    pub memtable_size_limit: usize,
    /// 块缓存的容量，字节；0 表示不用块缓存。
    pub block_cache_size: usize,
    /// L0 之下有几层。tiered 不用它。
    pub max_levels: usize,
    /// 后台自动合并的策略与参数。
    pub compaction: CompactionOptions,
    /// 写入是否先追加到 WAL。
    pub enable_wal: bool,
    /// 每次写入是否同步 WAL（`WriteOptions::sync`）。只在 `enable_wal` 时有意义。
    pub sync_writes: bool,
}

#[cfg(test)]
impl EngineConfig {
    /// 给定 memtable 上限与块缓存容量，L0 之下 4 层，不自动合并。测试用它。
    pub fn without_compaction(memtable_size_limit: usize, block_cache_size: usize) -> Self {
        Self {
            memtable_size_limit,
            block_cache_size,
            max_levels: 4,
            compaction: CompactionOptions::NoCompaction,
            enable_wal: false,
            sync_writes: false,
        }
    }
}

impl EngineTarget {
    pub fn new(config: EngineConfig) -> Self {
        let options = LsmStorageOptions {
            memtable_size_limit: config.memtable_size_limit,
            block_size: 4096,
            block_cache_size: config.block_cache_size,
            // compaction 写出的文件与 flush 写出的一样大，与 RocksDB 默认的做法相同。
            target_sst_size: config.memtable_size_limit,
            max_levels: config.max_levels,
            compaction: config.compaction.clone(),
            enable_wal: config.enable_wal,
        };
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db = Db::open(dir.path(), options).expect("打开引擎失败");
        Self { db, config, dir }
    }

    /// MANIFEST 里有几条记录、文件多少字节。每次 flush 或合并换状态追加一条，
    /// 第一条记着形状。
    fn manifest_usage(&self) -> (usize, u64) {
        let path = self.dir.path().join(MANIFEST_FILE_NAME);
        let bytes = std::fs::metadata(&path)
            .expect("读 MANIFEST 的信息失败")
            .len();
        let (_, records) = Manifest::recover(&path).expect("读 MANIFEST 失败");
        (records.len(), bytes)
    }

    /// 目录里现有几个 SST 文件。
    fn sst_file_count(&self) -> usize {
        let entries = std::fs::read_dir(self.dir.path()).expect("读目录失败");
        let mut count = 0;
        for entry in entries {
            let entry = entry.expect("读目录失败");
            if entry.file_name().to_string_lossy().ends_with(".sst") {
                count += 1;
            }
        }
        count
    }
}

impl KvTarget for EngineTarget {
    fn name(&self) -> &'static str {
        "engine"
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        let options = WriteOptions {
            sync: self.config.sync_writes,
        };
        self.db.put_opt(key, value, options).expect("写入失败");
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db
            .get(key)
            .expect("点查失败")
            .map(|value| value.to_vec())
    }

    fn scan(&self, start: &[u8], limit: usize) -> usize {
        let mut iter = self
            .db
            .scan(Bound::Included(start), Bound::Unbounded)
            .expect("建立扫描的游标失败");
        read_up_to(&mut iter, limit)
    }

    fn details(&self) -> Option<String> {
        let memtable_megabytes = self.config.memtable_size_limit / (1024 * 1024);
        let cache_megabytes = self.config.block_cache_size / (1024 * 1024);
        let max_levels = self.config.max_levels;
        // 策略与形状：tiered 没有 L0、tier 的个数是变的，分开描述。
        let (strategy, shape) = match &self.config.compaction {
            CompactionOptions::NoCompaction => (
                format!("L0 之下 {max_levels} 层，不自动合并"),
                format!("L0 有 {} 个 SST", self.db.l0_sstable_count()),
            ),
            CompactionOptions::SimpleLeveled(options) => (
                format!(
                    "L0 之下 {max_levels} 层，后台按简单分层合并（L0 触发值 {}，层间比例 {}%）",
                    options.level0_file_num_compaction_trigger, options.size_ratio_percent
                ),
                format!("L0 有 {} 个 SST", self.db.l0_sstable_count()),
            ),
            CompactionOptions::Tiered(options) => (
                format!(
                    "后台按 tiered 合并（num_tiers {}，空间放大 {}%，比例 {}%，最少合并 {} 个，最多 {}）",
                    options.num_tiers,
                    options.max_size_amplification_percent,
                    options.size_ratio,
                    options.min_merge_width,
                    match options.max_merge_width {
                        Some(width) => format!("{width} 个"),
                        None => "不限".to_string(),
                    }
                ),
                "不用 L0".to_string(),
            ),
            CompactionOptions::Leveled(options) => (
                format!(
                    "L0 之下 {max_levels} 层，后台按 leveled 合并（L0 触发值 {}，层间比例 {}，基准大小 {} MB）",
                    options.level0_file_num_compaction_trigger,
                    options.level_size_multiplier,
                    options.base_level_size_bytes as f64 / (1024.0 * 1024.0)
                ),
                format!("L0 有 {} 个 SST", self.db.l0_sstable_count()),
            ),
        };
        let level_files = join_counts(&self.db.level_sstable_counts());
        let levels_name = if self.config.compaction.flush_to_l0() {
            "L1 往下各层"
        } else {
            "各 tier（新的在前）"
        };
        let frozen = self.db.imm_memtable_count();
        let (cached_blocks, cached_bytes) = self.db.block_cache_usage();
        let cached_megabytes = cached_bytes as f64 / (1024.0 * 1024.0);
        let (manifest_records, manifest_bytes) = self.manifest_usage();
        let manifest_kilobytes = manifest_bytes as f64 / 1024.0;
        let wal = match (self.config.enable_wal, self.config.sync_writes) {
            (false, _) => "不写 WAL",
            (true, false) => "写入追加到 WAL、不同步",
            (true, true) => "写入追加到 WAL 并同步",
        };
        Some(format!(
            "memtable 上限 {memtable_megabytes} MB，块缓存 {cache_megabytes} MB，{wal}，{strategy}；\
             结束时有 {frozen} 张冻结的 memtable 等待落盘，{shape}，\
             {levels_name}依次有 {level_files} 个，块缓存里有 {cached_blocks} 个块（{cached_megabytes:.1} MB），\
             MANIFEST 有 {manifest_records} 条记录（{manifest_kilobytes:.1} KB）"
        ))
    }

    fn stats(&self) -> Option<StatsSnapshot> {
        Some(self.db.stats())
    }

    /// 等冻结的 memtable 全部写成 SST，后台没有待做的合并，并且合并换下的旧文件都已删掉。
    ///
    /// 最后一项要单独等：合并先换状态、再删旧文件，状态里已经没有待做的任务时，
    /// 旧文件可能还在目录里。目录里的 SST 个数与状态里的相同，才说明删完了。
    fn wait_for_background(&self) {
        loop {
            let flushing = self.db.imm_memtable_count() > 0;
            let compacting = self.db.has_pending_compaction();
            let files_in_state =
                self.db.l0_sstable_count() + self.db.level_sstable_counts().iter().sum::<usize>();
            let deleting = self.sst_file_count() != files_in_state;
            if !flushing && !compacting && !deleting {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn compact(&self) {
        self.db.force_full_compaction().expect("compaction 失败");
    }

    fn sst_layout(&self) -> Option<SstLayout> {
        let mut total_bytes = 0;
        for entry in std::fs::read_dir(self.dir.path()).expect("读目录失败") {
            let entry = entry.expect("读目录失败");
            let is_sst = entry.file_name().to_string_lossy().ends_with(".sst");
            if is_sst {
                total_bytes += entry.metadata().expect("读文件信息失败").len();
            }
        }
        Some(SstLayout {
            l0_files: self.db.l0_sstable_count(),
            level_files: self.db.level_sstable_counts(),
            total_bytes,
        })
    }
}

/// 把各层的文件数写成 `0 / 2 / 5` 的样子。
pub fn join_counts(counts: &[usize]) -> String {
    let texts: Vec<String> = counts.iter().map(|count| count.to_string()).collect();
    texts.join(" / ")
}

/// 从游标当前的位置起读最多 `limit` 条，返回读到的条数。
fn read_up_to(iter: &mut impl StorageIterator, limit: usize) -> usize {
    let mut count = 0;
    while count < limit && iter.is_valid() {
        black_box(iter.key().len() + iter.value().len());
        count += 1;
        iter.next().expect("游标前进时出错");
    }
    count
}

/// 可以在命令行上选择的被测对象。
pub const TARGET_NAMES: [&str; 3] = ["btreemap", "memtable", "engine"];

/// 按名字创建被测对象；名字不认识时返回 `None`。
/// `engine_config` 只对 `engine` 有意义。
pub fn create_target(name: &str, engine_config: &EngineConfig) -> Option<Box<dyn KvTarget>> {
    match name {
        "btreemap" => Some(Box::new(BTreeMapTarget::new())),
        "memtable" => Some(Box::new(MemTableTarget::new())),
        "engine" => Some(Box::new(EngineTarget::new(engine_config.clone()))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsm::compaction::simple_leveled::SimpleLeveledOptions;
    use lsm::compaction::tiered::TieredOptions;

    // 内存基线读到的是最后一次写入的值；它没有引擎计数器。
    #[test]
    fn btreemap_target_reads_back_latest_value() {
        let target = BTreeMapTarget::new();

        assert_eq!(target.get(b"k"), None);

        target.put(b"k", b"v1");
        target.put(b"k", b"v2");

        assert_eq!(target.get(b"k"), Some(b"v2".to_vec()));
        assert_eq!(target.stats(), None);
    }

    // memtable 作为被测对象：读到最后一次写入的值，没有引擎计数器。
    #[test]
    fn memtable_target_reads_back_latest_value() {
        let target = MemTableTarget::new();

        assert_eq!(target.get(b"k"), None);

        target.put(b"k", b"v1");
        target.put(b"k", b"v2");

        assert_eq!(target.get(b"k"), Some(b"v2".to_vec()));
        assert_eq!(target.stats(), None);
    }

    // 列出的每个名字都能创建出同名的被测对象，不认识的名字创建失败。
    #[test]
    fn every_listed_target_can_be_created() {
        for name in TARGET_NAMES {
            let config = EngineConfig::without_compaction(1024, 1 << 20);
            let target = create_target(name, &config).unwrap();
            assert_eq!(target.name(), name);
        }
        let config = EngineConfig::without_compaction(1024, 1 << 20);
        assert!(create_target("nosuch", &config).is_none());
    }

    // 三种被测对象的扫描结果一致：从起点（包含）开始，最多读指定的条数，到尾部为止。
    #[test]
    fn every_target_scans_from_start_up_to_limit() {
        for name in TARGET_NAMES {
            // 上限很小，让引擎在写入途中冻结几次，扫描要跨多张表。
            let config = EngineConfig::without_compaction(8, 1 << 20);
            let target = create_target(name, &config).unwrap();
            for key in ["e", "a", "c", "b", "d"] {
                target.put(key.as_bytes(), b"value");
            }

            assert_eq!(target.scan(b"a", 3), 3, "{name}");
            assert_eq!(target.scan(b"d", 3), 2, "{name}");
            assert_eq!(target.scan(b"bb", 10), 3, "{name}");
            assert_eq!(target.scan(b"z", 10), 0, "{name}");
        }
    }

    // 引擎作为被测对象：读到最后一次写入的值；说明里带着 memtable 的上限。
    #[test]
    fn engine_target_reads_back_latest_value() {
        let target = EngineTarget::new(EngineConfig::without_compaction(4 * 1024 * 1024, 1 << 20));

        assert_eq!(target.get(b"k"), None);

        target.put(b"k", b"v1");
        target.put(b"k", b"v2");

        assert_eq!(target.get(b"k"), Some(b"v2".to_vec()));
        assert!(target.details().unwrap().contains("4 MB"));
    }

    // 引擎作为被测对象有计数器；等后台工作做完之后，冻结的表全部落了盘。
    #[test]
    fn engine_target_reports_stats_and_drains_frozen_memtables() {
        // 上限很小，写入途中会冻结很多次。
        let target = EngineTarget::new(EngineConfig::without_compaction(256, 0));
        for i in 0..200 {
            target.put(format!("key-{i:03}").as_bytes(), b"value");
        }

        target.wait_for_background();

        assert_eq!(target.db.imm_memtable_count(), 0);
        assert!(target.db.l0_sstable_count() > 1);
        let stats = target.stats().unwrap();
        assert_eq!(stats.user_bytes_written, 200 * (7 + 5));
        assert!(stats.flush_bytes_written > 0);
    }

    // 开了 WAL：写入追加到 WAL，计数器记下 WAL 的字节，报告里写明；sync 模式照常写入。
    #[test]
    fn engine_target_with_wal_counts_wal_bytes() {
        for sync_writes in [false, true] {
            let config = EngineConfig {
                enable_wal: true,
                sync_writes,
                ..EngineConfig::without_compaction(1 << 20, 0)
            };
            let target = EngineTarget::new(config);
            for i in 0..20 {
                target.put(format!("key-{i:03}").as_bytes(), b"value");
            }

            let stats = target.stats().unwrap();
            // 每条：4 字节长度、2 + 7 字节 key、2 + 5 字节 value、4 字节校验和。
            assert_eq!(stats.wal_bytes_written, 20 * (4 + 2 + 7 + 2 + 5 + 4));
            assert_eq!(target.get(b"key-019"), Some(b"value".to_vec()));
            assert!(target.details().unwrap().contains("WAL"));
        }
    }

    // 开了自动合并：等后台工作做完之后，冻结的表全部落了盘，也没有待做的合并；
    // 有文件被合并进了 L1 往下的层。
    #[test]
    fn engine_target_waits_for_background_compaction() {
        let config = EngineConfig {
            memtable_size_limit: 256,
            block_cache_size: 0,
            max_levels: 3,
            compaction: CompactionOptions::SimpleLeveled(SimpleLeveledOptions {
                level0_file_num_compaction_trigger: 2,
                size_ratio_percent: 200,
            }),
            enable_wal: false,
            sync_writes: false,
        };
        let target = EngineTarget::new(config);
        for i in 0..200 {
            target.put(format!("key-{i:03}").as_bytes(), b"value");
        }

        target.wait_for_background();

        assert_eq!(target.db.imm_memtable_count(), 0);
        assert!(!target.db.has_pending_compaction());
        assert!(target.db.level_sstable_counts().iter().sum::<usize>() > 0);
        assert!(target.stats().unwrap().compaction_bytes_written > 0);
        assert!(target.details().unwrap().contains("简单分层"));
    }

    // tiered：等后台工作做完之后同样没有待做的合并；L0 一直是空的，报告里写的是 tier。
    #[test]
    fn engine_target_waits_for_background_tiered_compaction() {
        let config = EngineConfig {
            memtable_size_limit: 256,
            block_cache_size: 0,
            max_levels: 3,
            compaction: CompactionOptions::Tiered(TieredOptions {
                num_tiers: 4,
                max_size_amplification_percent: 200,
                size_ratio: 1,
                min_merge_width: 2,
                max_merge_width: None,
            }),
            enable_wal: false,
            sync_writes: false,
        };
        let target = EngineTarget::new(config);
        for i in 0..200 {
            target.put(format!("key-{i:03}").as_bytes(), b"value");
        }

        target.wait_for_background();

        assert_eq!(target.db.imm_memtable_count(), 0);
        assert!(!target.db.has_pending_compaction());
        assert_eq!(target.db.l0_sstable_count(), 0);
        assert!(target.db.level_sstable_counts().len() < 4);
        assert!(target.stats().unwrap().compaction_bytes_written > 0);
        let details = target.details().unwrap();
        assert!(details.contains("tiered") && details.contains("各 tier"));
    }
}
