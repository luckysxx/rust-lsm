//! 被测对象。基准驱动只通过 `KvTarget` 操作它，换一个实现就能在相同负载下对照。

use std::collections::BTreeMap;
use std::hint::black_box;
use std::ops::Bound;
use std::sync::{Arc, Mutex};

use lsm::iterators::StorageIterator;
use lsm::lsm_storage::{LsmStorage, LsmStorageOptions};
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

/// 引擎层：一张可写的 memtable 加若干张冻结的，写满即冻结；没有 WAL、没有文件。
///
/// 冻结的表目前不会被写成 SST，只会越积越多，点查与扫描都要逐张去看。
/// 与 `MemTableTarget` 相比，差别是多了状态快照、冻结，以及跨多张表的查找与归并。
pub struct EngineTarget {
    storage: LsmStorage,
    memtable_size_limit: usize,
}

impl EngineTarget {
    pub fn new(memtable_size_limit: usize) -> Self {
        let options = LsmStorageOptions {
            memtable_size_limit,
        };
        Self {
            storage: LsmStorage::open(options),
            memtable_size_limit,
        }
    }
}

impl KvTarget for EngineTarget {
    fn name(&self) -> &'static str {
        "engine"
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.storage.put(key, value);
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.storage.get(key).map(|value| value.to_vec())
    }

    fn scan(&self, start: &[u8], limit: usize) -> usize {
        let mut iter = self.storage.scan(Bound::Included(start), Bound::Unbounded);
        read_up_to(&mut iter, limit)
    }

    fn details(&self) -> Option<String> {
        let megabytes = self.memtable_size_limit / (1024 * 1024);
        let frozen = self.storage.imm_memtable_count();
        Some(format!(
            "memtable 上限 {megabytes} MB；结束时有 {frozen} 张冻结的 memtable（不落盘，只增不减）"
        ))
    }
}

/// 从游标当前的位置起读最多 `limit` 条，返回读到的条数。
fn read_up_to(iter: &mut impl StorageIterator, limit: usize) -> usize {
    let mut count = 0;
    while count < limit && iter.is_valid() {
        black_box(iter.key().len() + iter.value().len());
        count += 1;
        iter.next();
    }
    count
}

/// 可以在命令行上选择的被测对象。
pub const TARGET_NAMES: [&str; 3] = ["btreemap", "memtable", "engine"];

/// 按名字创建被测对象；名字不认识时返回 `None`。
/// `memtable_size_limit` 只对 `engine` 有意义。
pub fn create_target(name: &str, memtable_size_limit: usize) -> Option<Box<dyn KvTarget>> {
    match name {
        "btreemap" => Some(Box::new(BTreeMapTarget::new())),
        "memtable" => Some(Box::new(MemTableTarget::new())),
        "engine" => Some(Box::new(EngineTarget::new(memtable_size_limit))),
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
            let target = create_target(name, 1024).unwrap();
            assert_eq!(target.name(), name);
        }
        assert!(create_target("nosuch", 1024).is_none());
    }

    // 三种被测对象的扫描结果一致：从起点（包含）开始，最多读指定的条数，到尾部为止。
    #[test]
    fn every_target_scans_from_start_up_to_limit() {
        for name in TARGET_NAMES {
            // 上限很小，让引擎在写入途中冻结几次，扫描要跨多张表。
            let target = create_target(name, 8).unwrap();
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
        let target = EngineTarget::new(4 * 1024 * 1024);

        assert_eq!(target.get(b"k"), None);

        target.put(b"k", b"v1");
        target.put(b"k", b"v2");

        assert_eq!(target.get(b"k"), Some(b"v2".to_vec()));
        assert!(target.details().unwrap().contains("4 MB"));
    }
}
