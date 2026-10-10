//! 引擎层：把多张 memtable 组织起来，对外提供 `put`、`delete`、`get`。
//!
//! 同一时刻有一张可写的 memtable 和若干张冻结的 memtable。写入只进可写的那张；
//! 它写满后被冻结，换上一张新的。读取从新到旧依次查找。
//! 目前数据只在内存里，冻结的表还不会被写成 SST。

use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use bytes::Bytes;

use crate::iterators::merge_iterator::MergeIterator;
use crate::lsm_iterator::LsmIterator;
use crate::mem_table::{MemTable, MemTableIterator};

/// `scan` 返回的游标类型：每张 memtable 一个游标，归并，再去掉墓碑。
/// 以后加入 SST，里面两层会换，对外仍然是这个名字。
pub type ScanIterator = LsmIterator<MergeIterator<MemTableIterator>>;

/// 某一时刻引擎由哪些表组成。
///
/// 一份状态创建之后不再修改。要改变结构，就复制一份、改副本、再把副本整份换上去。
/// 读者手里的旧状态因此永远不会变。
#[derive(Clone)]
pub struct LsmStorageState {
    /// 可写的 memtable，恰好一张。
    pub memtable: Arc<MemTable>,
    /// 冻结的 memtable，新的在前。
    pub imm_memtables: Vec<Arc<MemTable>>,
}

#[derive(Debug, Clone)]
pub struct LsmStorageOptions {
    /// 可写的 memtable 估算大小达到这个字节数就冻结。这是一条软线，允许略微超出。
    pub memtable_size_limit: usize,
}

pub struct LsmStorage {
    /// 指向当前状态的指针。读写锁保护的是"当前状态是哪一份"，不是表里的内容。
    state: RwLock<Arc<LsmStorageState>>,
    /// 让改变结构的操作（现在只有冻结）排队。
    state_lock: Mutex<()>,
    options: LsmStorageOptions,
}

impl LsmStorage {
    pub fn open(options: LsmStorageOptions) -> Self {
        let initial_state = LsmStorageState {
            memtable: Arc::new(MemTable::new()),
            imm_memtables: Vec::new(),
        };
        Self {
            state: RwLock::new(Arc::new(initial_state)),
            state_lock: Mutex::new(()),
            options,
        }
    }

    /// 当前有几张冻结的 memtable。它们还没有落盘，点查与扫描都要逐张去看。
    pub fn imm_memtable_count(&self) -> usize {
        self.snapshot().imm_memtables.len()
    }

    /// 取走当前状态的一份快照。
    ///
    /// 只在复制指针的一瞬间持有读锁；之后调用方在快照上慢慢查，不挡住别人。
    fn snapshot(&self) -> Arc<LsmStorageState> {
        let guard = self.state.read().unwrap();
        Arc::clone(&guard)
    }

    /// 查找一个 key；不存在或已被删除时返回 `None`。
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        let snapshot = self.snapshot();

        // 从新到旧：先查可写的表。
        if let Some(value) = snapshot.memtable.get(key) {
            return visible_value(value);
        }

        // 再查冻结的表，列表里新的在前。
        for table in &snapshot.imm_memtables {
            if let Some(value) = table.get(key) {
                return visible_value(value);
            }
        }

        None
    }

    /// 按 key 从小到大扫描范围内的记录，已删除的 key 不出现。
    ///
    /// 游标建立在调用这一刻的状态快照上：之后发生的冻结不影响它，冻结之后
    /// 写入新表的 key 它看不到。但它看到的不是数据的快照：快照里那张可写的表
    /// 在被冻结之前仍在接收写入，排在游标之后的新 key 会被扫到。
    pub fn scan(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> ScanIterator {
        let snapshot = self.snapshot();

        // 来源从新到旧排列，归并时排在前面的胜出：
        // 可写的表最新，放在最前；冻结的表在列表里本来就是新的在前，照原样的顺序放。
        let mut iters = Vec::new();
        iters.push(MemTableIterator::new(
            Arc::clone(&snapshot.memtable),
            lower,
            upper,
        ));
        for imm_memtable in &snapshot.imm_memtables {
            iters.push(MemTableIterator::new(
                Arc::clone(imm_memtable),
                lower,
                upper,
            ));
        }

        ScanIterator::new(MergeIterator::new(iters))
    }

    /// 写入一条记录。key 与 value 都不能为空：空 value 留给墓碑使用。
    pub fn put(&self, key: &[u8], value: &[u8]) {
        assert!(!key.is_empty(), "key 不能为空");
        assert!(!value.is_empty(), "value 不能为空，空 value 表示删除");
        self.write(key, value);
    }

    /// 删除一个 key：写入一条墓碑。
    pub fn delete(&self, key: &[u8]) {
        assert!(!key.is_empty(), "key 不能为空");
        self.write(key, b"");
    }

    /// `put` 与 `delete` 共用的写入步骤。
    ///
    /// 写入期间持有 `state` 的读锁：冻结要拿写锁，所以不会有表在写入途中被冻结。
    /// 表本身允许多个线程同时写入，读锁也不会让写入之间互相等待。
    fn write(&self, key: &[u8], value: &[u8]) {
        let size_after_write;
        {
            let guard = self.state.read().unwrap();
            guard.memtable.put(key, value);
            size_after_write = guard.memtable.approximate_size();
        }
        // 读锁已经放开：冻结要拿写锁，持着读锁去冻结会等自己，造成死锁。
        self.try_freeze(size_after_write);
    }

    /// 可写的表到了上限就冻结它。
    ///
    /// 先在锁外用写入时看到的大小快速判断；到了上限再拿 `state_lock` 排队。
    /// 排队期间别的线程可能已经冻结过了，所以拿到锁之后要重新检查当前的表。
    fn try_freeze(&self, size_after_write: usize) {
        let limit = self.options.memtable_size_limit;
        if size_after_write < limit {
            return;
        }

        let state_lock = self.state_lock.lock().unwrap();
        let current_size = self.snapshot().memtable.approximate_size();
        if current_size >= limit {
            self.force_freeze_memtable(&state_lock);
        }
    }

    /// 把可写的表移进冻结列表，换上一张新的空表。
    ///
    /// 调用方必须持有 `state_lock`，参数就是持有它的凭证。
    ///
    /// 当前状态可能正被读者的快照引用，不能原地修改：复制一份，在副本上换表，
    /// 再用写锁把指针换成副本。新表在拿写锁之前就准备好，写锁只持有一瞬间。
    pub fn force_freeze_memtable(&self, _state_lock: &MutexGuard<'_, ()>) {
        let new_memtable = Arc::new(MemTable::new());

        let mut guard = self.state.write().unwrap();
        let mut new_state = guard.as_ref().clone();
        let old_memtable = Arc::clone(&new_state.memtable);
        // 冻结列表新的在前：刚冻结的这张最新。
        new_state.imm_memtables.insert(0, old_memtable);
        new_state.memtable = new_memtable;
        *guard = Arc::new(new_state);
    }
}

/// memtable 里读到的记录怎样呈现给调用方：空 value 是墓碑，表现为不存在。
fn visible_value(value: Bytes) -> Option<Bytes> {
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use std::collections::BTreeMap;
    use std::thread;

    /// 扫描整个引擎，收集结果。
    fn scan_all(storage: &LsmStorage) -> Vec<(String, String)> {
        let mut iter = storage.scan(Bound::Unbounded, Bound::Unbounded);
        collect_all(&mut iter)
    }

    fn open_with_limit(memtable_size_limit: usize) -> LsmStorage {
        LsmStorage::open(LsmStorageOptions {
            memtable_size_limit,
        })
    }

    /// 上限大到测试里不会触发自动冻结。
    fn open_without_auto_freeze() -> LsmStorage {
        open_with_limit(usize::MAX)
    }

    fn freeze(storage: &LsmStorage) {
        let state_lock = storage.state_lock.lock().unwrap();
        storage.force_freeze_memtable(&state_lock);
    }

    // ---------- 写入、删除与读取 ----------

    // 空的引擎里什么都查不到。
    #[test]
    fn write_nothing_then_get_returns_none() {
        let storage = open_without_auto_freeze();

        assert_eq!(storage.get(b"a"), None);
    }

    // 写入之后能读到；没写过的 key 读不到。
    #[test]
    fn write_put_then_get_returns_value() {
        let storage = open_without_auto_freeze();

        storage.put(b"a", b"1");

        assert_eq!(storage.get(b"a"), Some(Bytes::from_static(b"1")));
        assert_eq!(storage.get(b"b"), None);
    }

    // 删除之后读不到：引擎把墓碑解释成"不存在"，而不是返回空值。
    #[test]
    fn write_delete_hides_value() {
        let storage = open_without_auto_freeze();

        storage.put(b"a", b"1");
        storage.delete(b"a");

        assert_eq!(storage.get(b"a"), None);
    }

    // 删除之后再写入，读到的是新值。
    #[test]
    fn write_put_after_delete_is_visible() {
        let storage = open_without_auto_freeze();

        storage.put(b"a", b"1");
        storage.delete(b"a");
        storage.put(b"a", b"2");

        assert_eq!(storage.get(b"a"), Some(Bytes::from_static(b"2")));
    }

    // 空 value 留给墓碑，put 拒绝它。
    #[test]
    #[should_panic(expected = "value 不能为空")]
    fn write_put_rejects_empty_value() {
        let storage = open_without_auto_freeze();

        storage.put(b"a", b"");
    }

    // ---------- 冻结 ----------

    // 冻结后：原来的表进了冻结列表，可写的是一张新的空表。
    #[test]
    fn freeze_moves_memtable_to_frozen_list() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");

        freeze(&storage);

        let state = storage.snapshot();
        assert_eq!(state.imm_memtables.len(), 1);
        assert_eq!(storage.imm_memtable_count(), 1);
        assert_eq!(state.memtable.approximate_size(), 0);
        assert_eq!(
            state.imm_memtables[0].get(b"a"),
            Some(Bytes::from_static(b"1"))
        );
    }

    // 冻结不丢数据：冻结前写入的 key 之后照样读得到。
    #[test]
    fn freeze_keeps_data_readable() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");

        freeze(&storage);
        storage.put(b"b", b"2");

        assert_eq!(storage.get(b"a"), Some(Bytes::from_static(b"1")));
        assert_eq!(storage.get(b"b"), Some(Bytes::from_static(b"2")));
    }

    // 可写的表里的值盖住冻结表里的旧值。
    #[test]
    fn freeze_newer_value_wins_over_frozen_value() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"old");

        freeze(&storage);
        storage.put(b"a", b"new");

        assert_eq!(storage.get(b"a"), Some(Bytes::from_static(b"new")));
    }

    // 两张冻结表里都有同一个 key 时，读到较新那张里的值：列表必须新的在前。
    #[test]
    fn freeze_newer_frozen_table_wins_over_older_one() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        freeze(&storage);
        storage.put(b"a", b"2");
        freeze(&storage);

        assert_eq!(storage.snapshot().imm_memtables.len(), 2);
        assert_eq!(storage.get(b"a"), Some(Bytes::from_static(b"2")));
    }

    // 较新的表里的墓碑挡住较旧的表里的值：读到墓碑就停，不能继续往旧表里找。
    #[test]
    fn freeze_tombstone_hides_value_in_older_table() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        freeze(&storage);
        storage.delete(b"a");

        assert_eq!(storage.get(b"a"), None);

        freeze(&storage);

        assert_eq!(storage.get(b"a"), None);
    }

    // 冻结之前取走的快照不受冻结影响：它看到的仍是当时的结构。
    #[test]
    fn freeze_does_not_change_earlier_snapshot() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        let before = storage.snapshot();

        freeze(&storage);
        storage.put(b"b", b"2");

        assert_eq!(before.imm_memtables.len(), 0);
        assert_eq!(before.memtable.get(b"a"), Some(Bytes::from_static(b"1")));
        assert_eq!(before.memtable.get(b"b"), None);
    }

    // ---------- 写满自动冻结 ----------

    // 没到上限不冻结。
    #[test]
    fn auto_freeze_does_not_happen_below_limit() {
        let storage = open_with_limit(1000);

        storage.put(b"a", b"1");
        storage.put(b"b", b"2");

        assert_eq!(storage.snapshot().imm_memtables.len(), 0);
    }

    // 写到上限后自动冻结，之后的写入进新表，所有数据都读得到。
    #[test]
    fn auto_freeze_happens_at_limit() {
        // 每条记录 2 字节，上限 4 字节：每写两条冻结一次。
        let storage = open_with_limit(4);

        storage.put(b"a", b"1");
        assert_eq!(storage.snapshot().imm_memtables.len(), 0);
        storage.put(b"b", b"2");
        assert_eq!(storage.snapshot().imm_memtables.len(), 1);
        storage.put(b"c", b"3");
        storage.put(b"d", b"4");
        assert_eq!(storage.snapshot().imm_memtables.len(), 2);

        for (key, value) in [(b"a", b"1"), (b"b", b"2"), (b"c", b"3"), (b"d", b"4")] {
            assert_eq!(storage.get(key), Some(Bytes::copy_from_slice(value)));
        }
    }

    // 多个线程同时写并触发冻结：数据一条不丢；每张冻结的表都确实到过上限，
    // 没有哪张几乎空的表因为重复冻结而被冻结。
    #[test]
    fn auto_freeze_under_concurrent_writes() {
        let limit = 256;
        let storage = open_with_limit(limit);
        let threads = 8;
        let puts_per_thread = 2000;

        thread::scope(|scope| {
            for thread_id in 0..threads {
                let storage = &storage;
                scope.spawn(move || {
                    for i in 0..puts_per_thread {
                        let key = format!("t{thread_id}-{i:05}");
                        storage.put(key.as_bytes(), b"v");
                    }
                });
            }
        });

        for thread_id in 0..threads {
            for i in 0..puts_per_thread {
                let key = format!("t{thread_id}-{i:05}");
                assert_eq!(storage.get(key.as_bytes()), Some(Bytes::from_static(b"v")));
            }
        }

        let state = storage.snapshot();
        assert!(!state.imm_memtables.is_empty());
        for table in &state.imm_memtables {
            assert!(table.approximate_size() >= limit);
        }
    }

    // ---------- 范围扫描 ----------

    // 空的引擎扫不出任何东西。
    #[test]
    fn scan_on_empty_storage_yields_nothing() {
        let storage = open_without_auto_freeze();

        assert_eq!(scan_all(&storage), expected(&[]));
    }

    // 记录分散在可写的表和两张冻结的表里，扫描把它们合成一个按 key 有序的结果。
    #[test]
    fn scan_merges_all_memtables_in_key_order() {
        let storage = open_without_auto_freeze();
        storage.put(b"e", b"5");
        storage.put(b"b", b"2");
        freeze(&storage);
        storage.put(b"d", b"4");
        storage.put(b"a", b"1");
        freeze(&storage);
        storage.put(b"c", b"3");

        assert_eq!(
            scan_all(&storage),
            expected(&[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4"), ("e", "5")])
        );
    }

    // 同一个 key 在几张表里都有：扫描给出最新那张表里的值，而且只给一次。
    #[test]
    fn scan_returns_newest_value_for_each_key() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"oldest");
        storage.put(b"b", b"oldest");
        freeze(&storage);
        storage.put(b"a", b"middle");
        freeze(&storage);
        storage.put(b"a", b"newest");

        assert_eq!(
            scan_all(&storage),
            expected(&[("a", "newest"), ("b", "oldest")])
        );
    }

    // 已删除的 key 不出现，即使更旧的表里还留着它的值。
    #[test]
    fn scan_hides_deleted_keys() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        storage.put(b"b", b"2");
        storage.put(b"c", b"3");
        freeze(&storage);
        storage.delete(b"b");

        assert_eq!(scan_all(&storage), expected(&[("a", "1"), ("c", "3")]));

        freeze(&storage);

        assert_eq!(scan_all(&storage), expected(&[("a", "1"), ("c", "3")]));
    }

    // 删除之后又写入的 key 重新出现，值是新写入的。
    #[test]
    fn scan_shows_key_written_again_after_delete() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        freeze(&storage);
        storage.delete(b"a");
        freeze(&storage);
        storage.put(b"a", b"2");

        assert_eq!(scan_all(&storage), expected(&[("a", "2")]));
    }

    // 范围的上下界对每一张表都生效。
    #[test]
    fn scan_respects_bounds_across_memtables() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        storage.put(b"c", b"3");
        freeze(&storage);
        storage.put(b"b", b"2");
        storage.put(b"d", b"4");

        let mut iter = storage.scan(Bound::Included(b"b"), Bound::Excluded(b"d"));
        let b_to_d = collect_all(&mut iter);
        let mut iter = storage.scan(Bound::Excluded(b"b"), Bound::Included(b"d"));
        let after_b_through_d = collect_all(&mut iter);

        assert_eq!(b_to_d, expected(&[("b", "2"), ("c", "3")]));
        assert_eq!(after_b_through_d, expected(&[("c", "3"), ("d", "4")]));
    }

    // 游标建立在创建时的状态快照上：之后冻结、再写进新表的 key，这个游标看不到；
    // 快照里原有的记录照样扫得出来。
    #[test]
    fn scan_is_not_affected_by_later_freeze() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        storage.put(b"c", b"3");
        let mut iter = storage.scan(Bound::Unbounded, Bound::Unbounded);

        freeze(&storage);
        storage.put(b"b", b"2");
        storage.put(b"z", b"26");

        assert_eq!(collect_all(&mut iter), expected(&[("a", "1"), ("c", "3")]));
        assert_eq!(
            scan_all(&storage),
            expected(&[("a", "1"), ("b", "2"), ("c", "3"), ("z", "26")])
        );
    }

    // 随机执行写入、删除、冻结，每一步之后引擎的全表扫描与点查都和一张普通的有序表一致。
    #[test]
    fn scan_and_get_match_reference_model_under_random_operations() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut random = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };

        for _round in 0..20 {
            let storage = open_without_auto_freeze();
            let mut model: BTreeMap<String, String> = BTreeMap::new();

            for step in 0..200 {
                let key = format!("k{:02}", random(20));
                match random(10) {
                    0 => freeze(&storage),
                    1..=3 => {
                        storage.delete(key.as_bytes());
                        model.remove(&key);
                    }
                    _ => {
                        let value = format!("v{step}");
                        storage.put(key.as_bytes(), value.as_bytes());
                        model.insert(key.clone(), value);
                    }
                }

                let from_model: Vec<(String, String)> =
                    model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(scan_all(&storage), from_model);

                let got = storage.get(key.as_bytes());
                let got = got.map(|value| String::from_utf8_lossy(&value).into_owned());
                assert_eq!(got, model.get(&key).cloned());
            }
        }
    }
}
