//! 内存表：写入先落在这里，写满后冻结并刷成 SST。
//!
//! 一张可以被多个线程同时读写的有序表。删除不移除记录，而是写入一条
//! value 为空的记录（墓碑），所以表里的记录只增不减。

use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use crossbeam_skiplist::SkipMap;

use crate::iterators::StorageIterator;

#[derive(Default)]
pub struct MemTable {
    /// 并发有序表，key 与 value 都由表自己持有。
    map: SkipMap<Bytes, Bytes>,
    /// 写入过的 key 与 value 的长度之和，覆盖写重复计入。
    approximate_size: AtomicUsize,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入一条记录，覆盖同一个 key 原有的记录。
    ///
    /// key 和 value 各拷贝一次，之后表里的数据与调用方的缓冲区无关。
    /// value 为空表示这个 key 被删除。
    pub fn put(&self, key: &[u8], value: &[u8]) {
        self.map
            .insert(Bytes::copy_from_slice(key), Bytes::copy_from_slice(value));
        self.approximate_size
            .fetch_add(key.len() + value.len(), Ordering::Relaxed);
    }

    /// 查找一个 key。
    ///
    /// - `None`：这张表里没有这个 key 的记录，调用方应当继续查更旧的数据。
    /// - `Some(空)`：这个 key 已被删除，调用方不应再往更旧的数据里找。
    /// - `Some(非空)`：找到了值。
    ///
    /// 返回的 `Bytes` 与表共用同一块内存，不拷贝数据。
    pub fn get(&self, key: &[u8]) -> Option<Bytes> {
        self.map.get(key).map(|entry| entry.value().clone())
    }

    /// 到目前为止写入的 key 与 value 的长度之和。覆盖写重复计入。
    pub fn approximate_size(&self) -> usize {
        self.approximate_size.load(Ordering::Relaxed)
    }

    /// 范围内 key 最小的那条记录；范围内没有记录时返回 `None`。
    ///
    /// 墓碑也是记录，原样返回。范围为空（例如下界大于上界）时返回 `None`。
    pub fn first_in_range(
        &self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Option<(Bytes, Bytes)> {
        let mut range = self.map.range::<[u8], _>((lower, upper));
        let entry = range.next()?;
        Some((entry.key().clone(), entry.value().clone()))
    }
}

/// 一张 memtable 上的游标：按 key 从小到大给出范围内的记录，墓碑原样给出。
///
/// 游标不保存"指向表里某个节点"的引用，只保存当前这条记录本身；每次前进时
/// 在表里重新查找"比当前 key 大的第一条"。这样游标里没有任何借用，可以带着
/// 一个 `Arc` 让表活着、被返回给调用方慢慢使用。代价是每前进一步是一次查找。
///
/// 表在扫描期间仍可能被写入：排在游标之后的新 key 会被扫到，排在之前的不会。
pub struct MemTableIterator {
    /// 让表在游标用完之前不被释放。
    table: Arc<MemTable>,
    /// 范围的上界。下界只在创建时用一次，不用保存。
    upper: Bound<Bytes>,
    /// 当前这条记录（key，value）；`None` 表示已经走到头。
    current: Option<(Bytes, Bytes)>,
}

impl MemTableIterator {
    /// 创建游标并让它指着范围内的第一条记录。
    pub fn new(table: Arc<MemTable>, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Self {
        let current = table.first_in_range(lower, upper);
        Self {
            table,
            upper: bound_to_owned(upper),
            current,
        }
    }
}

impl StorageIterator for MemTableIterator {
    fn key(&self) -> &[u8] {
        let (key, _value) = self.current.as_ref().expect("游标无效时不能读取 key");
        key
    }

    fn value(&self) -> &[u8] {
        let (_key, value) = self.current.as_ref().expect("游标无效时不能读取 value");
        value
    }

    fn is_valid(&self) -> bool {
        self.current.is_some()
    }

    fn next(&mut self) {
        // 不保存位置：在表里重新查找比当前 key 大、且不越过上界的第一条。
        // 找不到时得到 None，游标随之变为无效。
        self.current = self
            .table
            .first_in_range(Bound::Excluded(self.key()), bound_as_slice(&self.upper));
    }
}

/// 把借来的边界复制成自己拥有的，好存进游标里。
fn bound_to_owned(bound: Bound<&[u8]>) -> Bound<Bytes> {
    match bound {
        Bound::Included(key) => Bound::Included(Bytes::copy_from_slice(key)),
        Bound::Excluded(key) => Bound::Excluded(Bytes::copy_from_slice(key)),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// 把自己拥有的边界借出去，交给只需要看一眼的函数。
fn bound_as_slice(bound: &Bound<Bytes>) -> Bound<&[u8]> {
    match bound {
        Bound::Included(key) => Bound::Included(&key[..]),
        Bound::Excluded(key) => Bound::Excluded(&key[..]),
        Bound::Unbounded => Bound::Unbounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use std::thread;

    /// 建一张表并按给定的顺序写入。
    fn table_with(pairs: &[(&str, &str)]) -> Arc<MemTable> {
        let table = MemTable::new();
        for (key, value) in pairs {
            table.put(key.as_bytes(), value.as_bytes());
        }
        Arc::new(table)
    }

    /// 在整张表上扫描 `[lower, upper)` 之类的范围，收集结果。
    fn scan(
        table: &Arc<MemTable>,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Vec<(String, String)> {
        let mut iter = MemTableIterator::new(Arc::clone(table), lower, upper);
        collect_all(&mut iter)
    }

    // 没写过的 key 查不到，返回 None。
    #[test]
    fn get_missing_key_returns_none() {
        let table = MemTable::new();

        assert_eq!(table.get(b"a"), None);
    }

    // 写入之后能读到同样的字节，没写过的 key 不受影响。
    #[test]
    fn put_then_get_returns_value() {
        let table = MemTable::new();

        table.put(b"a", b"1");
        table.put(b"b", b"22");

        assert_eq!(table.get(b"a"), Some(Bytes::from_static(b"1")));
        assert_eq!(table.get(b"b"), Some(Bytes::from_static(b"22")));
        assert_eq!(table.get(b"c"), None);
    }

    // 同一个 key 写两次，读到的是后写的值。
    #[test]
    fn put_overwrites_existing_key() {
        let table = MemTable::new();

        table.put(b"a", b"old");
        table.put(b"a", b"new");

        assert_eq!(table.get(b"a"), Some(Bytes::from_static(b"new")));
    }

    // 墓碑（空 value）是一条真实的记录：读到的是 Some(空)，不是 None。
    // 两者含义不同：None 表示"这张表不知道"，Some(空) 表示"已删除"。
    #[test]
    fn empty_value_is_kept_as_tombstone() {
        let table = MemTable::new();

        table.put(b"a", b"1");
        table.put(b"a", b"");

        assert_eq!(table.get(b"a"), Some(Bytes::new()));
    }

    // put 拷贝了数据：调用方之后改动自己的缓冲区，表里的内容不变。
    #[test]
    fn put_copies_callers_buffers() {
        let table = MemTable::new();
        let mut key = b"key".to_vec();
        let mut value = b"value".to_vec();

        table.put(&key, &value);
        key[0] = b'X';
        value[0] = b'X';

        assert_eq!(table.get(b"key"), Some(Bytes::from_static(b"value")));
        assert_eq!(table.get(b"Xey"), None);
    }

    // get 交出的值不依赖表：表被释放之后，手里的值仍然可读。
    #[test]
    fn value_outlives_table() {
        let table = MemTable::new();
        table.put(b"a", b"still here");

        let value = table.get(b"a").unwrap();
        drop(table);

        assert_eq!(&value[..], b"still here");
    }

    // 估算的大小是写入的 key 长度与 value 长度之和，空表为 0。
    #[test]
    fn approximate_size_adds_key_and_value_lengths() {
        let table = MemTable::new();
        assert_eq!(table.approximate_size(), 0);

        table.put(b"a", b"1");
        table.put(b"bb", b"222");

        assert_eq!(table.approximate_size(), 2 + 5);
    }

    // 同一个 key 写两次，估算的大小把两次都算进去，不扣掉被覆盖的那一次。
    #[test]
    fn approximate_size_adds_same_key_lengths() {
        let table = MemTable::new();
        assert_eq!(table.approximate_size(), 0);

        table.put(b"a", b"1");
        table.put(b"a", b"222");

        assert_eq!(table.approximate_size(), 2 + 4);
    }

    // 多个线程同时写入各自的 key，一条都不丢，大小也没有少算。
    #[test]
    fn concurrent_puts_are_all_visible() {
        let table = MemTable::new();
        let threads = 8;
        let puts_per_thread = 1000;

        thread::scope(|scope| {
            for thread_id in 0..threads {
                let table = &table;
                scope.spawn(move || {
                    for i in 0..puts_per_thread {
                        let key = format!("t{thread_id}-{i:04}");
                        table.put(key.as_bytes(), b"v");
                    }
                });
            }
        });

        for thread_id in 0..threads {
            for i in 0..puts_per_thread {
                let key = format!("t{thread_id}-{i:04}");
                assert_eq!(table.get(key.as_bytes()), Some(Bytes::from_static(b"v")));
            }
        }
        // 每个 key 形如 "t0-0000"，7 字节；value 1 字节。
        assert_eq!(table.approximate_size(), threads * puts_per_thread * 8);
    }

    // ---------- 游标 ----------

    // 不管写入的顺序如何，游标按 key 从小到大给出全部记录。
    #[test]
    fn iterator_yields_all_records_in_key_order() {
        let table = table_with(&[("d", "4"), ("a", "1"), ("c", "3"), ("b", "2")]);

        let collected = scan(&table, Bound::Unbounded, Bound::Unbounded);

        assert_eq!(
            collected,
            expected(&[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")])
        );
    }

    // 空表上的游标一开始就是无效的。
    #[test]
    fn iterator_on_empty_table_is_invalid() {
        let table = table_with(&[]);

        let iter = MemTableIterator::new(table, Bound::Unbounded, Bound::Unbounded);

        assert!(!iter.is_valid());
    }

    // 下界：包含时从这个 key 开始，不包含时从它之后开始；下界不在表里时从比它大的第一条开始。
    #[test]
    fn iterator_respects_lower_bound() {
        let table = table_with(&[("a", "1"), ("c", "3"), ("e", "5")]);

        let from_c = scan(&table, Bound::Included(b"c"), Bound::Unbounded);
        let after_c = scan(&table, Bound::Excluded(b"c"), Bound::Unbounded);
        let from_b = scan(&table, Bound::Included(b"b"), Bound::Unbounded);

        assert_eq!(from_c, expected(&[("c", "3"), ("e", "5")]));
        assert_eq!(after_c, expected(&[("e", "5")]));
        assert_eq!(from_b, expected(&[("c", "3"), ("e", "5")]));
    }

    // 上界：不包含时在这个 key 之前停下，包含时把它也给出；前进时不能越过上界。
    #[test]
    fn iterator_respects_upper_bound() {
        let table = table_with(&[("a", "1"), ("c", "3"), ("e", "5")]);

        let before_c = scan(&table, Bound::Unbounded, Bound::Excluded(b"c"));
        let up_to_c = scan(&table, Bound::Unbounded, Bound::Included(b"c"));
        let before_d = scan(&table, Bound::Unbounded, Bound::Excluded(b"d"));

        assert_eq!(before_c, expected(&[("a", "1")]));
        assert_eq!(up_to_c, expected(&[("a", "1"), ("c", "3")]));
        assert_eq!(before_d, expected(&[("a", "1"), ("c", "3")]));
    }

    // 范围内没有记录时，游标一开始就是无效的。
    #[test]
    fn iterator_over_empty_range_is_invalid() {
        let table = table_with(&[("a", "1"), ("e", "5")]);

        let between = scan(&table, Bound::Included(b"b"), Bound::Excluded(b"d"));
        let inverted = scan(&table, Bound::Included(b"e"), Bound::Excluded(b"a"));

        assert_eq!(between, expected(&[]));
        assert_eq!(inverted, expected(&[]));
    }

    // 墓碑原样给出，这一层不过滤；被覆盖的 key 只出现一次，是最后写入的值。
    #[test]
    fn iterator_passes_tombstones_and_latest_values() {
        let table = table_with(&[
            ("a", "1"),
            ("b", "2"),
            ("b", ""),
            ("c", "old"),
            ("c", "new"),
        ]);

        let collected = scan(&table, Bound::Unbounded, Bound::Unbounded);

        assert_eq!(collected, expected(&[("a", "1"), ("b", ""), ("c", "new")]));
    }

    // 游标自己让表活着：创建之后调用方不再持有这张表，游标照样走完。
    #[test]
    fn iterator_keeps_table_alive() {
        let table = table_with(&[("a", "1"), ("b", "2")]);
        let mut iter =
            MemTableIterator::new(Arc::clone(&table), Bound::Unbounded, Bound::Unbounded);
        drop(table);

        let collected = collect_all(&mut iter);

        assert_eq!(collected, expected(&[("a", "1"), ("b", "2")]));
    }

    // 扫描期间表被写入：排在游标之后的新 key 会被扫到，排在之前的不会。
    #[test]
    fn iterator_sees_later_writes_ahead_of_cursor() {
        let table = table_with(&[("b", "2"), ("d", "4")]);
        let mut iter =
            MemTableIterator::new(Arc::clone(&table), Bound::Unbounded, Bound::Unbounded);
        iter.next();
        assert_eq!(iter.key(), b"d");

        table.put(b"a", b"1");
        table.put(b"e", b"5");

        let rest = collect_all(&mut iter);

        assert_eq!(rest, expected(&[("d", "4"), ("e", "5")]));
    }
}
