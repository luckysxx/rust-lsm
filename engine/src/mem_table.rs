//! 内存表：写入先落在这里，写满后冻结并刷成 SST。
//!
//! 一张可以被多个线程同时读写的有序表。删除不移除记录，而是写入一条
//! value 为空的记录（墓碑），所以表里的记录只增不减。
//!
//! 开了 WAL 时，每张表有自己的 WAL 文件：写入先追加到 WAL，再插入表。崩溃之后重放这个文件，
//! 就能重建这张表。

use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use crossbeam_skiplist::SkipMap;

use crate::error::Result;
use crate::iterators::StorageIterator;
use crate::wal::Wal;

pub struct MemTable {
    /// 表的编号，与 SST 共用一个计数器，所以不会与任何文件重名。WAL 的文件名就是它。
    id: usize,
    /// 并发有序表，key 与 value 都由表自己持有。
    map: SkipMap<Bytes, Bytes>,
    /// 写入过的 key 与 value 的长度之和，覆盖写重复计入。
    approximate_size: AtomicUsize,
    /// 这张表的 WAL。没开 WAL 时是 `None`，写入只进内存。
    wal: Option<Wal>,
    /// 有 WAL 时，"追加 WAL、插入表"两步一起在这把锁里做：WAL 里的先后就是插入的先后，
    /// 重放得到的表与崩溃前的一样。没有 WAL 时只有多对的批次用它，让一批批整批先后生效；
    /// 单条写入之间不排队。
    write_lock: Mutex<()>,
}

impl MemTable {
    /// 一张没有 WAL 的空表，写入只进内存。
    pub fn new(id: usize) -> Self {
        Self {
            id,
            map: SkipMap::new(),
            approximate_size: AtomicUsize::new(0),
            wal: None,
            write_lock: Mutex::new(()),
        }
    }

    /// 一张带 WAL 的空表：在 `path` 新建 WAL 文件并同步所在的目录。文件已经存在时返回错误。
    pub fn create_with_wal(id: usize, path: &Path) -> Result<Self> {
        let wal = Wal::create(path)?;
        let mut table = Self::new(id);
        table.wal = Some(wal);
        Ok(table)
    }

    /// 从 `path` 的 WAL 重建一张表：按写入的顺序把一批批记录插回去，之后的写入接着追加到这个文件。
    ///
    /// 末尾写了一半的记录被截掉（见 [`Wal::recover`]）；中间损坏时返回
    /// [`crate::error::Error::Corruption`]。
    pub fn recover_from_wal(id: usize, path: &Path) -> Result<Self> {
        let (wal, batches) = Wal::recover(path)?;
        let mut table = Self::new(id);
        for batch in batches {
            for (key, value) in batch {
                table.insert(&key, &value);
            }
        }
        table.wal = Some(wal);
        Ok(table)
    }

    /// 表的编号。
    pub fn id(&self) -> usize {
        self.id
    }

    /// 这张表有没有 WAL。
    pub fn has_wal(&self) -> bool {
        self.wal.is_some()
    }

    /// 写入一条记录，覆盖同一个 key 原有的记录。value 为空表示这个 key 被删除。
    ///
    /// 就是只有一对的 [`MemTable::put_batch`]。
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.put_batch(&[(key, value)])?;
        Ok(())
    }

    /// 写入一批记录，按顺序插入：同一个 key 出现几次时，后面的覆盖前面的。批次不能为空。
    ///
    /// 有 WAL 时先把整批作为一条记录追加到 WAL、再逐对插入表，两步在 `write_lock` 里一起做；
    /// 追加失败时返回错误，表不变。追加只写进页缓存、不同步，要落到磁盘上，把返回的
    /// "这条记录在 WAL 里的末尾"交给 [`MemTable::sync_wal_to`]。没有 WAL 时返回 0。
    ///
    /// 插入期间别的线程可能读到这批的一部分：这里只保证崩溃之后整批都在或都不在。
    /// key 和 value 各拷贝一次，之后表里的数据与调用方的缓冲区无关。
    pub fn put_batch(&self, batch: &[(&[u8], &[u8])]) -> Result<u64> {
        match &self.wal {
            Some(wal) => {
                let _guard = self.write_lock.lock().unwrap();
                let wal_end = wal.append(batch)?;
                for &(key, value) in batch {
                    self.insert(key, value);
                }
                Ok(wal_end)
            }
            None => {
                // 多对的批次也要排队：两批同时写同几个 key 时，不排队就会一对一对交错，
                // 写完之后 a 是这一批的、b 是那一批的，哪种先后次序都得不到这个结果。
                let _guard = (batch.len() > 1).then(|| self.write_lock.lock().unwrap());
                for &(key, value) in batch {
                    self.insert(key, value);
                }
                Ok(0)
            }
        }
    }

    /// 把这张表的 WAL 同步到磁盘；没有 WAL 时什么都不做。返回这次有没有真的做 fsync。
    ///
    /// 不拿 `write_lock`：同步期间别的线程照常写入。返回之后，调用之前完成的写入都已落盘。
    pub fn sync_wal(&self) -> Result<bool> {
        match &self.wal {
            Some(wal) => wal.sync(),
            None => Ok(false),
        }
    }

    /// 让这张表的 WAL 里末尾不超过 `wal_end`（[`MemTable::put_batch`] 的返回值）的记录落盘，
    /// 别的线程那次 fsync 已经带下去时不再做。返回这次有没有真的做 fsync。
    pub fn sync_wal_to(&self, wal_end: u64) -> Result<bool> {
        match &self.wal {
            Some(wal) => wal.sync_to(wal_end),
            None => Ok(false),
        }
    }

    /// 只在测试里用：之后这张表的 WAL 每次 fsync 都变慢，见 [`Wal::make_syncs_slow`]。
    #[cfg(test)]
    pub(crate) fn make_wal_syncs_slow(&self) {
        self.wal.as_ref().unwrap().make_syncs_slow();
    }

    /// 只在测试里用：这张表的 WAL 开始过几次 fsync。
    #[cfg(test)]
    pub(crate) fn wal_fsyncs_started(&self) -> usize {
        self.wal.as_ref().unwrap().fsyncs_started()
    }

    /// 只插入表，不碰 WAL。
    fn insert(&self, key: &[u8], value: &[u8]) {
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

    fn next(&mut self) -> Result<()> {
        // 不保存位置：在表里重新查找比当前 key 大、且不越过上界的第一条。
        // 找不到时得到 None，游标随之变为无效。
        self.current = self
            .table
            .first_in_range(Bound::Excluded(self.key()), bound_as_slice(&self.upper));
        Ok(())
    }
}

/// 把借来的边界复制成自己拥有的，好存进游标里。
pub(crate) fn bound_to_owned(bound: Bound<&[u8]>) -> Bound<Bytes> {
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
        let table = MemTable::new(0);
        for (key, value) in pairs {
            table.put(key.as_bytes(), value.as_bytes()).unwrap();
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
        let table = MemTable::new(0);

        assert_eq!(table.get(b"a"), None);
    }

    // 写入之后能读到同样的字节，没写过的 key 不受影响。
    #[test]
    fn put_then_get_returns_value() {
        let table = MemTable::new(0);

        table.put(b"a", b"1").unwrap();
        table.put(b"b", b"22").unwrap();

        assert_eq!(table.get(b"a"), Some(Bytes::from_static(b"1")));
        assert_eq!(table.get(b"b"), Some(Bytes::from_static(b"22")));
        assert_eq!(table.get(b"c"), None);
    }

    // 同一个 key 写两次，读到的是后写的值。
    #[test]
    fn put_overwrites_existing_key() {
        let table = MemTable::new(0);

        table.put(b"a", b"old").unwrap();
        table.put(b"a", b"new").unwrap();

        assert_eq!(table.get(b"a"), Some(Bytes::from_static(b"new")));
    }

    // 墓碑（空 value）是一条真实的记录：读到的是 Some(空)，不是 None。
    // 两者含义不同：None 表示"这张表不知道"，Some(空) 表示"已删除"。
    #[test]
    fn empty_value_is_kept_as_tombstone() {
        let table = MemTable::new(0);

        table.put(b"a", b"1").unwrap();
        table.put(b"a", b"").unwrap();

        assert_eq!(table.get(b"a"), Some(Bytes::new()));
    }

    // put 拷贝了数据：调用方之后改动自己的缓冲区，表里的内容不变。
    #[test]
    fn put_copies_callers_buffers() {
        let table = MemTable::new(0);
        let mut key = b"key".to_vec();
        let mut value = b"value".to_vec();

        table.put(&key, &value).unwrap();
        key[0] = b'X';
        value[0] = b'X';

        assert_eq!(table.get(b"key"), Some(Bytes::from_static(b"value")));
        assert_eq!(table.get(b"Xey"), None);
    }

    // get 交出的值不依赖表：表被释放之后，手里的值仍然可读。
    #[test]
    fn value_outlives_table() {
        let table = MemTable::new(0);
        table.put(b"a", b"still here").unwrap();

        let value = table.get(b"a").unwrap();
        drop(table);

        assert_eq!(&value[..], b"still here");
    }

    // 估算的大小是写入的 key 长度与 value 长度之和，空表为 0。
    #[test]
    fn approximate_size_adds_key_and_value_lengths() {
        let table = MemTable::new(0);
        assert_eq!(table.approximate_size(), 0);

        table.put(b"a", b"1").unwrap();
        table.put(b"bb", b"222").unwrap();

        assert_eq!(table.approximate_size(), 2 + 5);
    }

    // 同一个 key 写两次，估算的大小把两次都算进去，不扣掉被覆盖的那一次。
    #[test]
    fn approximate_size_adds_same_key_lengths() {
        let table = MemTable::new(0);
        assert_eq!(table.approximate_size(), 0);

        table.put(b"a", b"1").unwrap();
        table.put(b"a", b"222").unwrap();

        assert_eq!(table.approximate_size(), 2 + 4);
    }

    // 多个线程同时写入各自的 key，一条都不丢，大小也没有少算。
    #[test]
    fn concurrent_puts_are_all_visible() {
        let table = MemTable::new(0);
        let threads = 8;
        let puts_per_thread = 1000;

        thread::scope(|scope| {
            for thread_id in 0..threads {
                let table = &table;
                scope.spawn(move || {
                    for i in 0..puts_per_thread {
                        let key = format!("t{thread_id}-{i:04}");
                        table.put(key.as_bytes(), b"v").unwrap();
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
        iter.next().unwrap();
        assert_eq!(iter.key(), b"d");

        table.put(b"a", b"1").unwrap();
        table.put(b"e", b"5").unwrap();

        let rest = collect_all(&mut iter);

        assert_eq!(rest, expected(&[("d", "4"), ("e", "5")]));
    }

    // ---------- WAL ----------

    /// 读出 WAL 文件里的全部批次，每批是一个列表，变成字符串。
    fn wal_batches(path: &Path) -> Vec<Vec<(String, String)>> {
        let (_wal, batches) = Wal::recover(path).unwrap();
        batches
            .iter()
            .map(|batch| {
                batch
                    .iter()
                    .map(|(key, value)| {
                        let key = String::from_utf8(key.to_vec()).unwrap();
                        let value = String::from_utf8(value.to_vec()).unwrap();
                        (key, value)
                    })
                    .collect()
            })
            .collect()
    }

    /// 读出 WAL 文件里的全部记录，不分批次，变成字符串。
    fn wal_entries(path: &Path) -> Vec<(String, String)> {
        let (_wal, batches) = Wal::recover(path).unwrap();
        batches
            .iter()
            .flatten()
            .map(|(key, value)| {
                let key = String::from_utf8(key.to_vec()).unwrap();
                let value = String::from_utf8(value.to_vec()).unwrap();
                (key, value)
            })
            .collect()
    }

    // 带 WAL 的表：每次写入按顺序追加到 WAL，墓碑也在其中；表里照常读得到。
    #[test]
    fn wal_records_every_put_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00007.wal");
        let table = MemTable::create_with_wal(7, &path).unwrap();

        table.put(b"a", b"1").unwrap();
        table.put(b"b", b"2").unwrap();
        table.put(b"a", b"").unwrap();
        table.sync_wal().unwrap();

        assert_eq!(table.id(), 7);
        assert_eq!(table.get(b"a"), Some(Bytes::new()));
        assert_eq!(table.get(b"b"), Some(Bytes::from_static(b"2")));
        let expected = vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
            ("a".to_string(), String::new()),
        ];
        assert_eq!(wal_entries(&path), expected);
    }

    // WAL 文件已经存在时建表失败，不覆盖原来的文件。
    #[test]
    fn create_with_wal_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00007.wal");
        let table = MemTable::create_with_wal(7, &path).unwrap();
        table.put(b"a", b"1").unwrap();

        assert!(MemTable::create_with_wal(7, &path).is_err());
        assert_eq!(wal_entries(&path).len(), 1);
    }

    // 没有 WAL 的表不建文件；sync_wal 什么都不做。
    #[test]
    fn table_without_wal_writes_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let table = MemTable::new(3);

        table.put(b"a", b"1").unwrap();
        table.sync_wal().unwrap();

        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert_eq!(table.get(b"a"), Some(Bytes::from_static(b"1")));
    }

    // 几个线程在同一时刻写同一个 key：WAL 里最后一条的值就是表里的值，重放得到的与崩溃前一样。
    // 每个 key 都先用 Barrier 让线程对齐再一起写；追加之后停一小段长短不一的时间，
    // 两步之间没有锁时插入的先后就会与追加的先后不同。
    #[test]
    fn wal_order_matches_table_under_concurrent_puts() {
        crate::test_util::run_with_slow_deadline("并发写同一个 key", || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("00001.wal");
            let table = MemTable::create_with_wal(1, &path).unwrap();
            table.wal.as_ref().unwrap().make_appends_slow();
            let threads = 4;
            let keys = 50;
            let barrier = std::sync::Barrier::new(threads);
            thread::scope(|scope| {
                for thread_id in 0..threads {
                    let table = &table;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        for i in 0..keys {
                            let key = format!("k{i:02}");
                            let value = format!("t{thread_id}");
                            barrier.wait();
                            table.put(key.as_bytes(), value.as_bytes()).unwrap();
                        }
                    });
                }
            });

            let entries = wal_entries(&path);
            assert_eq!(entries.len(), threads * keys);
            let mut last_in_wal = std::collections::BTreeMap::new();
            for (key, value) in entries {
                last_in_wal.insert(key, value);
            }
            for (key, value) in &last_in_wal {
                let in_table = table.get(key.as_bytes()).unwrap();
                assert_eq!(value.as_bytes(), &in_table[..], "{key}");
            }
        });
    }

    // 一批写入：按顺序插入，同一个 key 后面的覆盖前面的；WAL 里整批是一条记录。
    #[test]
    fn put_batch_inserts_in_order_as_one_wal_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00007.wal");
        let table = MemTable::create_with_wal(7, &path).unwrap();

        table.put(b"z", b"0").unwrap();
        table
            .put_batch(&[(b"a", b"1"), (b"b", b"2"), (b"a", b"3"), (b"b", b"")])
            .unwrap();

        assert_eq!(table.get(b"a"), Some(Bytes::from_static(b"3")));
        assert_eq!(table.get(b"b"), Some(Bytes::new()));
        assert_eq!(table.get(b"z"), Some(Bytes::from_static(b"0")));
        let expected_batches = vec![
            vec![("z".to_string(), "0".to_string())],
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("a".to_string(), "3".to_string()),
                ("b".to_string(), String::new()),
            ],
        ];
        assert_eq!(wal_batches(&path), expected_batches);
    }

    // 没有 WAL 的表也能整批写入，同一个 key 后面的覆盖前面的。
    #[test]
    fn put_batch_without_wal() {
        let table = MemTable::new(3);

        table.put_batch(&[(b"a", b"1"), (b"a", b"2")]).unwrap();

        assert_eq!(table.get(b"a"), Some(Bytes::from_static(b"2")));
        assert_eq!(table.approximate_size(), 4);
    }

    // 没有 WAL 时，几个线程同时写同几个 key 的批次：每一轮写完，各个 key 的值都来自同一批，
    // 不会一对来自这一批、另一对来自那一批。每轮先用 Barrier 让线程对齐再一起写。
    #[test]
    fn concurrent_batches_without_wal_do_not_interleave() {
        crate::test_util::run_with_slow_deadline("并发写批次", || {
            let table = MemTable::new(1);
            let threads = 4;
            let rounds = 2000;
            let keys: Vec<String> = (0..8).map(|i| format!("k{i}")).collect();
            let barrier = std::sync::Barrier::new(threads);
            thread::scope(|scope| {
                for thread_id in 0..threads {
                    let table = &table;
                    let barrier = &barrier;
                    let keys = &keys;
                    scope.spawn(move || {
                        for round in 0..rounds {
                            let value = format!("t{thread_id}-r{round}");
                            let batch: Vec<(&[u8], &[u8])> = keys
                                .iter()
                                .map(|key| (key.as_bytes(), value.as_bytes()))
                                .collect();
                            barrier.wait();
                            table.put_batch(&batch).unwrap();
                            barrier.wait();
                            let first = table.get(keys[0].as_bytes()).unwrap();
                            for key in keys {
                                assert_eq!(
                                    table.get(key.as_bytes()).unwrap(),
                                    first,
                                    "第 {round} 轮"
                                );
                            }
                            barrier.wait();
                        }
                    });
                }
            });
        });
    }

    // 从 WAL 重建的表与写入时的一样：每一批的每一对都插回去，顺序不变。
    #[test]
    fn recover_from_wal_replays_every_pair_of_every_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00007.wal");
        let table = MemTable::create_with_wal(7, &path).unwrap();
        table.put_batch(&[(b"a", b"1"), (b"b", b"2")]).unwrap();
        table.put(b"c", b"3").unwrap();
        table.put_batch(&[(b"a", b""), (b"c", b"4")]).unwrap();
        drop(table);

        let table = MemTable::recover_from_wal(7, &path).unwrap();

        let mut iter = MemTableIterator::new(Arc::new(table), Bound::Unbounded, Bound::Unbounded);
        let all = collect_all(&mut iter);
        assert_eq!(all, expected(&[("a", ""), ("b", "2"), ("c", "4")]));
    }

    // 带 WAL 时 put_batch 返回这批记录在 WAL 里的末尾，同步到那里就够了；没有 WAL 时返回 0。
    #[test]
    fn put_batch_returns_wal_end_for_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00007.wal");
        let table = MemTable::create_with_wal(7, &path).unwrap();

        let first = table.put_batch(&[(b"a", b"1")]).unwrap();
        let second = table.put_batch(&[(b"b", b"2"), (b"c", b"3")]).unwrap();

        assert_eq!(second, std::fs::metadata(&path).unwrap().len());
        assert!(first < second);
        assert!(table.sync_wal_to(first).unwrap());
        assert!(!table.sync_wal_to(second).unwrap());
        assert!(!table.sync_wal().unwrap());
        assert_eq!(MemTable::new(3).put_batch(&[(b"a", b"1")]).unwrap(), 0);
    }
}
