//! 被测对象。基准驱动只通过 `KvTarget` 操作它，换一个实现就能在相同负载下对照。

use std::collections::BTreeMap;
use std::hint::black_box;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use lsm::db::Db;
use lsm::iterators::StorageIterator;
use lsm::lsm_storage::LsmStorageOptions;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SstLayout {
    pub l0_files: usize,
    pub l1_files: usize,
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
#[derive(Default)]
pub struct MemTableTarget {
    mem_table: Arc<MemTable>,
}

impl MemTableTarget {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvTarget for MemTableTarget {
    fn name(&self) -> &'static str {
        "memtable"
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.mem_table.put(key, value);
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

/// 完整的引擎：memtable 写满即冻结，后台线程把冻结的表写成 L0 的 SST；没有 WAL。
///
/// 没有自动的 compaction：L0 的文件只增不减，只有执行 `compact` 负载时才全部合并成 L1。
/// 点查与扫描要逐个去看 L0 的文件，靠每个文件的首末 key 与 Bloom filter 排除，
/// L1 只看一个文件；读块先查块缓存。
/// 写入太快时冻结的表会在内存里排队，还没有让写入等一等的机制；
/// 每段负载结束后基准驱动等它们全部落盘，再开始下一段。
/// 与 `MemTableTarget` 相比，差别是多了状态快照、冻结、落盘，以及跨表和文件的查找与归并。
pub struct EngineTarget {
    db: Db,
    memtable_size_limit: usize,
    block_cache_size: usize,
    /// 引擎所在的临时目录，随被测对象一起清理。
    dir: tempfile::TempDir,
}

impl EngineTarget {
    /// `block_cache_size` 为 0 时不用块缓存。
    pub fn new(memtable_size_limit: usize, block_cache_size: usize) -> Self {
        let options = LsmStorageOptions {
            memtable_size_limit,
            block_size: 4096,
            block_cache_size,
            // compaction 写出的文件与 flush 写出的一样大，与 RocksDB 默认的做法相同。
            target_sst_size: memtable_size_limit,
        };
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let db = Db::open(dir.path(), options).expect("打开引擎失败");
        Self {
            db,
            memtable_size_limit,
            block_cache_size,
            dir,
        }
    }
}

impl KvTarget for EngineTarget {
    fn name(&self) -> &'static str {
        "engine"
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.db.put(key, value);
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
        let memtable_megabytes = self.memtable_size_limit / (1024 * 1024);
        let cache_megabytes = self.block_cache_size / (1024 * 1024);
        let frozen = self.db.imm_memtable_count();
        let l0_files = self.db.l0_sstable_count();
        let l1_files = self.db.l1_sstable_count();
        let (cached_blocks, cached_bytes) = self.db.block_cache_usage();
        let cached_megabytes = cached_bytes as f64 / (1024.0 * 1024.0);
        Some(format!(
            "memtable 上限 {memtable_megabytes} MB，块缓存 {cache_megabytes} MB；\
             结束时有 {frozen} 张冻结的 memtable 等待落盘，L0 有 {l0_files} 个 SST，\
             L1 有 {l1_files} 个，块缓存里有 {cached_blocks} 个块（{cached_megabytes:.1} MB）"
        ))
    }

    fn stats(&self) -> Option<StatsSnapshot> {
        Some(self.db.stats())
    }

    /// 等冻结的 memtable 全部写成 SST。
    fn wait_for_background(&self) {
        while self.db.imm_memtable_count() > 0 {
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
            l1_files: self.db.l1_sstable_count(),
            total_bytes,
        })
    }
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
/// `memtable_size_limit` 与 `block_cache_size` 只对 `engine` 有意义。
pub fn create_target(
    name: &str,
    memtable_size_limit: usize,
    block_cache_size: usize,
) -> Option<Box<dyn KvTarget>> {
    match name {
        "btreemap" => Some(Box::new(BTreeMapTarget::new())),
        "memtable" => Some(Box::new(MemTableTarget::new())),
        "engine" => Some(Box::new(EngineTarget::new(
            memtable_size_limit,
            block_cache_size,
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let target = create_target(name, 1024, 1 << 20).unwrap();
            assert_eq!(target.name(), name);
        }
        assert!(create_target("nosuch", 1024, 1 << 20).is_none());
    }

    // 三种被测对象的扫描结果一致：从起点（包含）开始，最多读指定的条数，到尾部为止。
    #[test]
    fn every_target_scans_from_start_up_to_limit() {
        for name in TARGET_NAMES {
            // 上限很小，让引擎在写入途中冻结几次，扫描要跨多张表。
            let target = create_target(name, 8, 1 << 20).unwrap();
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
        let target = EngineTarget::new(4 * 1024 * 1024, 1 << 20);

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
        let target = EngineTarget::new(256, 0);
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
}
