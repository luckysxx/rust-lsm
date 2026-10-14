//! 引擎层：把 memtable 与 SST 组织起来，对外提供 `put`、`delete`、`get`、`scan`。
//!
//! 同一时刻有一张可写的 memtable、若干张冻结的 memtable 和若干个 L0 的 SST。
//! 写入只进可写的那张表；它写满后被冻结，换上一张新的。读取从新到旧依次查找：
//! 可写的表、冻结的表、L0 的 SST。
//! 冻结的表由 flush 写成 SST。
//!
//! 这一层自己不起线程：flush 由调用方触发。平时触发它的是 `db::Db` 的后台线程。

use std::fs::File;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use bytes::Bytes;

use crate::error::Result;
use crate::iterators::StorageIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::lsm_iterator::LsmIterator;
use crate::mem_table::{MemTable, MemTableIterator, bound_to_owned};
use crate::stats::{Stats, StatsSnapshot};
use crate::table::{BlockCache, SsTable, SsTableBuilder, SsTableIterator, new_block_cache};

/// `scan` 返回的游标类型，从里往外读：
/// memtable 那一组归并成一个，SST 那一组归并成一个，两路归并把它们合起来，
/// 最外面去掉墓碑并在上界处停下。
pub type ScanIterator =
    LsmIterator<TwoMergeIterator<MergeIterator<MemTableIterator>, MergeIterator<SsTableIterator>>>;

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
    /// L0 的 SST，新的在前。它们之间的 key 范围可以重叠，所以读的时候都要看。
    pub l0_sstables: Vec<Arc<SsTable>>,
}

#[derive(Debug, Clone)]
pub struct LsmStorageOptions {
    /// 可写的 memtable 估算大小达到这个字节数就冻结。这是一条软线，允许略微超出。
    pub memtable_size_limit: usize,
    /// SST 里每个数据块的目标大小，字节。
    pub block_size: usize,
    /// 块缓存的容量，字节。为 0 时不用块缓存，每次读块都从文件读。
    pub block_cache_size: usize,
}

pub struct LsmStorage {
    /// 指向当前状态的指针。读写锁保护的是"当前状态是哪一份"，不是表里的内容。
    state: RwLock<Arc<LsmStorageState>>,
    /// 让改变结构的操作（冻结、flush 最后换状态的那一步）排队。
    state_lock: Mutex<()>,
    /// 同一时刻只做一次 flush。写文件比较慢，这期间不占着 `state_lock`，
    /// 冻结和写入照常进行。
    flush_lock: Mutex<()>,
    /// 数据库所在的目录，SST 文件都放在这里。
    path: PathBuf,
    /// 下一个 SST 文件的编号。从 1 开始，只增不减，编号不重复使用。
    next_sst_id: AtomicUsize,
    /// 所有 SST 共用的块缓存。容量设为 0 时没有缓存。
    block_cache: Option<Arc<BlockCache>>,
    /// 统计计数器：写入的字节、flush 写出的字节、点查进入的 SST、从文件读出的块。
    stats: Arc<Stats>,
    options: LsmStorageOptions,
}

impl LsmStorage {
    /// 在 `path` 这个目录里打开引擎；目录不存在就创建。
    ///
    /// 目前不会读取目录里已有的文件：每次打开都是空的，重启后数据不保留。
    pub fn open(path: &Path, options: LsmStorageOptions) -> Result<Self> {
        std::fs::create_dir_all(path)?;

        let initial_state = LsmStorageState {
            memtable: Arc::new(MemTable::new()),
            imm_memtables: Vec::new(),
            l0_sstables: Vec::new(),
        };
        let block_cache = if options.block_cache_size == 0 {
            None
        } else {
            let capacity_bytes = options.block_cache_size as u64;
            Some(Arc::new(new_block_cache(capacity_bytes)))
        };
        Ok(Self {
            state: RwLock::new(Arc::new(initial_state)),
            state_lock: Mutex::new(()),
            flush_lock: Mutex::new(()),
            path: path.to_path_buf(),
            next_sst_id: AtomicUsize::new(1),
            block_cache,
            stats: Arc::new(Stats::new()),
            options,
        })
    }

    /// 当前有几张冻结的 memtable。它们还没有落盘，点查与扫描都要逐张去看。
    pub fn imm_memtable_count(&self) -> usize {
        self.snapshot().imm_memtables.len()
    }

    /// 当前 L0 有几个 SST。
    pub fn l0_sstable_count(&self) -> usize {
        self.snapshot().l0_sstables.len()
    }

    /// 块缓存里现在有几个块、一共多少字节。没有缓存时都是 0。
    pub fn block_cache_usage(&self) -> (u64, u64) {
        let Some(block_cache) = &self.block_cache else {
            return (0, 0);
        };
        // 缓存的计数稍有延迟，先让它把积压的工作做完。
        block_cache.run_pending_tasks();
        (block_cache.entry_count(), block_cache.weighted_size())
    }

    /// 统计计数器当前的值。
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// 编号为 `id` 的 SST 文件的路径，例如 `目录/00007.sst`。
    fn path_of_sst(&self, id: usize) -> PathBuf {
        self.path.join(format!("{id:05}.sst"))
    }

    /// 取一个新的 SST 编号。
    fn allocate_sst_id(&self) -> usize {
        self.next_sst_id.fetch_add(1, Ordering::SeqCst)
    }

    /// 同步数据库的目录：让"目录里多了一个文件"这件事落到磁盘上。
    fn sync_dir(&self) -> Result<()> {
        File::open(&self.path)?.sync_all()?;
        Ok(())
    }

    /// 取走当前状态的一份快照。
    ///
    /// 只在复制指针的一瞬间持有读锁；之后调用方在快照上慢慢查，不挡住别人。
    fn snapshot(&self) -> Arc<LsmStorageState> {
        let guard = self.state.read().unwrap();
        Arc::clone(&guard)
    }

    /// 查找一个 key；不存在或已被删除时返回 `None`。
    ///
    /// 从新到旧依次查找，遇到这个 key 的记录（值或墓碑）就停。查到 SST 时要读磁盘，
    /// 所以可能失败。
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.stats.add_get();
        let snapshot = self.snapshot();

        // 从新到旧：先查可写的表。
        if let Some(value) = snapshot.memtable.get(key) {
            return Ok(visible_value(value));
        }

        // 再查冻结的表，列表里新的在前。
        for table in &snapshot.imm_memtables {
            if let Some(value) = table.get(key) {
                return Ok(visible_value(value));
            }
        }

        // 最后查 L0 的 SST，列表里新的在前。在每个 SST 里定位到第一条不小于 key 的
        // 记录；它的 key 恰好相等才算这个 SST 里有，否则接着看更旧的。
        for sst in &snapshot.l0_sstables {
            // 范围之外的文件一定没有它，一个块都不用读。
            if !sst.may_contain_key(key) {
                continue;
            }
            self.stats.add_tables_probed(1);
            let iterator = SsTableIterator::new_and_seek_to_key(Arc::clone(sst), key)?;
            if iterator.is_valid() && iterator.key() == key {
                let value = Bytes::copy_from_slice(iterator.value());
                return Ok(visible_value(value));
            }
        }

        Ok(None)
    }

    /// 按 key 从小到大扫描范围内的记录，已删除的 key 不出现。
    ///
    /// 游标建立在调用这一刻的状态快照上：之后发生的冻结不影响它，冻结之后
    /// 写入新表的 key 它看不到。但它看到的不是数据的快照：快照里那张可写的表
    /// 在被冻结之前仍在接收写入，排在游标之后的新 key 会被扫到。
    ///
    /// 建立游标时要读每个 SST 的第一个相关的块，所以可能失败。
    pub fn scan(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<ScanIterator> {
        let snapshot = self.snapshot();

        // 每一组里来源都从新到旧排列，归并时排在前面的胜出。
        // memtable 这一组：可写的表最新，放在最前；冻结的表在列表里本来就是新的在前。
        let mut memtable_iters = Vec::new();
        memtable_iters.push(MemTableIterator::new(
            Arc::clone(&snapshot.memtable),
            lower,
            upper,
        ));
        for imm_memtable in &snapshot.imm_memtables {
            memtable_iters.push(MemTableIterator::new(
                Arc::clone(imm_memtable),
                lower,
                upper,
            ));
        }

        // SST 这一组：列表里也是新的在前。SST 的游标只管下界，上界由最外层处理。
        let mut sst_iters = Vec::new();
        for sst in &snapshot.l0_sstables {
            // 与范围不相交的文件不建游标：建游标要读它的一个块。
            if !sst.overlaps_range(lower, upper) {
                continue;
            }
            sst_iters.push(sst_iterator_from(Arc::clone(sst), lower)?);
        }

        // memtable 比 SST 新，放在两路归并的前一个位置。
        let two_merge_iterator = TwoMergeIterator::new(
            MergeIterator::new(memtable_iters),
            MergeIterator::new(sst_iters),
        )?;
        LsmIterator::new(two_merge_iterator, bound_to_owned(upper))
    }

    /// 写入一条记录。key 与 value 都不能为空：空 value 留给墓碑使用。
    ///
    /// 返回这次写入有没有冻结一张表。冻结了，就有一张表等着 flush。
    pub fn put(&self, key: &[u8], value: &[u8]) -> bool {
        assert!(!key.is_empty(), "key 不能为空");
        assert!(!value.is_empty(), "value 不能为空，空 value 表示删除");
        self.write(key, value)
    }

    /// 删除一个 key：写入一条墓碑。返回值与 `put` 的含义相同。
    pub fn delete(&self, key: &[u8]) -> bool {
        assert!(!key.is_empty(), "key 不能为空");
        self.write(key, b"")
    }

    /// `put` 与 `delete` 共用的写入步骤。
    ///
    /// 写入期间持有 `state` 的读锁：冻结要拿写锁，所以不会有表在写入途中被冻结。
    /// 表本身允许多个线程同时写入，读锁也不会让写入之间互相等待。
    fn write(&self, key: &[u8], value: &[u8]) -> bool {
        self.stats
            .add_user_bytes_written((key.len() + value.len()) as u64);
        let size_after_write;
        {
            let guard = self.state.read().unwrap();
            guard.memtable.put(key, value);
            size_after_write = guard.memtable.approximate_size();
        }
        // 读锁已经放开：冻结要拿写锁，持着读锁去冻结会等自己，造成死锁。
        self.try_freeze(size_after_write)
    }

    /// 可写的表到了上限就冻结它。返回这一次有没有冻结。
    ///
    /// 先在锁外用写入时看到的大小快速判断；到了上限再拿 `state_lock` 排队。
    /// 排队期间别的线程可能已经冻结过了，所以拿到锁之后要重新检查当前的表。
    fn try_freeze(&self, size_after_write: usize) -> bool {
        let limit = self.options.memtable_size_limit;
        if size_after_write < limit {
            return false;
        }

        let state_lock = self.state_lock.lock().unwrap();
        let current_size = self.snapshot().memtable.approximate_size();
        if current_size < limit {
            return false;
        }
        self.force_freeze_memtable(&state_lock);
        true
    }

    /// 不管可写的表有多大，现在就冻结它。表是空的也照样冻结：flush 遇到空表会直接丢掉。
    pub fn freeze_memtable(&self) {
        let state_lock = self.state_lock.lock().unwrap();
        self.force_freeze_memtable(&state_lock);
    }

    /// 把冻结的 memtable 一张一张写成 SST，直到一张不剩。
    ///
    /// 中途有一张写失败就停下并返回错误，那张表和它之后的表都留在冻结列表里。
    pub fn flush_all_imm_memtables(&self) -> Result<()> {
        // 每一圈都重新看列表：写文件的这段时间里可能又有表被冻结。
        while self.imm_memtable_count() > 0 {
            self.force_flush_next_imm_memtable()?;
        }
        Ok(())
    }

    /// 把最旧的一张冻结的 memtable 写成 SST，放进 L0。没有冻结的表时什么都不做。
    ///
    /// 写文件期间不挡住读写：先在锁外把文件写好、同步，最后只用一瞬间换状态。
    /// 换状态是一步完成的，读者要么看到"表还在、文件不在"，要么看到"表不在了、
    /// 文件在"，不会出现两边都没有的时刻。
    ///
    /// 写文件失败时返回错误，状态不变，那张表仍然留在冻结列表里。
    pub fn force_flush_next_imm_memtable(&self) -> Result<()> {
        // 同一时刻只做一次 flush：下面要先看"最旧的表是哪张"，最后再把它拿掉，
        // 这中间不能有另一次 flush 插进来。冻结不受影响，它只往列表的开头加。
        let _flush_lock = self.flush_lock.lock().unwrap();

        // 最旧的冻结表在列表的末尾。
        let snapshot = self.snapshot();
        let memtable = match snapshot.imm_memtables.last() {
            Some(memtable) => Arc::clone(memtable),
            None => return Ok(()),
        };

        // 写文件。表是空的时候不写，得到 None。
        let sst = self.build_sst_from_memtable(&memtable)?;
        if sst.is_some() {
            self.sync_dir()?;
        }

        // 换状态：复制一份，去掉末尾那张表，把新的 SST 放到 L0 的最前面，换指针。
        // 与冻结一样先拿 `state_lock` 排队，再用写锁换指针。
        let _state_lock = self.state_lock.lock().unwrap();
        let mut guard = self.state.write().unwrap();
        let mut new_state = guard.as_ref().clone();
        let removed = new_state.imm_memtables.pop();
        // 拿掉的必须就是刚才写成文件的那一张。
        assert!(
            removed.is_some_and(|removed| Arc::ptr_eq(&removed, &memtable)),
            "flush 期间冻结列表的末尾变了"
        );
        if let Some(sst) = sst {
            // L0 里新的在前：刚写出的这个最新。
            new_state.l0_sstables.insert(0, sst);
        }
        *guard = Arc::new(new_state);
        Ok(())
    }

    /// 把一张 memtable 里的全部记录按顺序写成一个 SST 文件。
    ///
    /// 墓碑照常写进去：更旧的 SST 里可能还有这个 key 的值，要靠它盖住。
    /// 表里一条记录都没有时不写文件，返回 `None`。
    fn build_sst_from_memtable(&self, memtable: &Arc<MemTable>) -> Result<Option<Arc<SsTable>>> {
        let mut memtable_iter =
            MemTableIterator::new(Arc::clone(memtable), Bound::Unbounded, Bound::Unbounded);

        // 游标一开始就无效，说明表里一条记录都没有。
        if !memtable_iter.is_valid() {
            return Ok(None);
        }

        // 把全部记录按顺序交给构建器。墓碑也照常交。
        let mut sst_builder = SsTableBuilder::new(self.options.block_size);
        while memtable_iter.is_valid() {
            sst_builder.add(memtable_iter.key(), memtable_iter.value());
            memtable_iter.next()?;
        }

        let sst_id = self.allocate_sst_id();
        let block_cache = self.block_cache.clone();
        let sst = sst_builder
            .build(sst_id, block_cache, &self.path_of_sst(sst_id))?
            .with_stats(Arc::clone(&self.stats));
        self.stats.add_flush_bytes_written(sst.file_size());
        Ok(Some(Arc::new(sst)))
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

/// 在一个 SST 上建游标，指着范围下界之后的第一条记录。
fn sst_iterator_from(sst: Arc<SsTable>, lower: Bound<&[u8]>) -> Result<SsTableIterator> {
    // 三种情况各自直接建在该去的位置上。先建在第一条、再定位的话，
    // 每个 SST 会白读一次它的第一个块。
    match lower {
        Bound::Included(key) => SsTableIterator::new_and_seek_to_key(sst, key),
        Bound::Excluded(key) => {
            // 定位只有"大于等于"一种，不包含下界时多走一步跳过它本身。
            let mut iterator = SsTableIterator::new_and_seek_to_key(sst, key)?;
            if iterator.is_valid() && iterator.key() == key {
                iterator.next()?;
            }
            Ok(iterator)
        }
        Bound::Unbounded => SsTableIterator::new_and_seek_to_first(sst),
    }
}

/// 读到的记录怎样呈现给调用方：空 value 是墓碑，表现为不存在。
fn visible_value(value: Bytes) -> Option<Bytes> {
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use crate::test_util::run_with_deadline;
    use std::collections::BTreeMap;
    use std::thread;

    /// 扫描整个引擎，收集结果。
    fn scan_all(storage: &LsmStorage) -> Vec<(String, String)> {
        let mut iter = storage.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
        collect_all(&mut iter)
    }

    /// 测试用的引擎：开在一个临时目录里，目录随它一起清理。
    ///
    /// 实现了 `Deref`，所以可以直接当作 `LsmStorage` 用：`storage.put(…)`、`&storage`。
    struct TestStorage {
        storage: LsmStorage,
        dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestStorage {
        type Target = LsmStorage;

        fn deref(&self) -> &LsmStorage {
            &self.storage
        }
    }

    impl TestStorage {
        /// 引擎所在的目录。
        fn dir(&self) -> &Path {
            self.dir.path()
        }

        /// 目录里现有的 SST 文件名，按名字排序。
        fn sst_file_names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".sst"))
                .collect();
            names.sort();
            names
        }
    }

    fn open_with_limit(memtable_size_limit: usize) -> TestStorage {
        let dir = tempfile::tempdir().unwrap();
        let options = LsmStorageOptions {
            memtable_size_limit,
            // 块给得很小，让测试里的 SST 也有好几个块。
            block_size: 64,
            block_cache_size: 1 << 20,
        };
        let storage = LsmStorage::open(dir.path(), options).unwrap();
        TestStorage { storage, dir }
    }

    /// 上限大到测试里不会触发自动冻结。
    fn open_without_auto_freeze() -> TestStorage {
        open_with_limit(usize::MAX)
    }

    fn freeze(storage: &LsmStorage) {
        storage.freeze_memtable();
    }

    // ---------- 写入、删除与读取 ----------

    // 空的引擎里什么都查不到。
    #[test]
    fn write_nothing_then_get_returns_none() {
        let storage = open_without_auto_freeze();

        assert_eq!(storage.get(b"a").unwrap(), None);
    }

    // 写入之后能读到；没写过的 key 读不到。
    #[test]
    fn write_put_then_get_returns_value() {
        let storage = open_without_auto_freeze();

        storage.put(b"a", b"1");

        assert_eq!(storage.get(b"a").unwrap(), Some(Bytes::from_static(b"1")));
        assert_eq!(storage.get(b"b").unwrap(), None);
    }

    // 删除之后读不到：引擎把墓碑解释成"不存在"，而不是返回空值。
    #[test]
    fn write_delete_hides_value() {
        let storage = open_without_auto_freeze();

        storage.put(b"a", b"1");
        storage.delete(b"a");

        assert_eq!(storage.get(b"a").unwrap(), None);
    }

    // 删除之后再写入，读到的是新值。
    #[test]
    fn write_put_after_delete_is_visible() {
        let storage = open_without_auto_freeze();

        storage.put(b"a", b"1");
        storage.delete(b"a");
        storage.put(b"a", b"2");

        assert_eq!(storage.get(b"a").unwrap(), Some(Bytes::from_static(b"2")));
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

        assert_eq!(storage.get(b"a").unwrap(), Some(Bytes::from_static(b"1")));
        assert_eq!(storage.get(b"b").unwrap(), Some(Bytes::from_static(b"2")));
    }

    // 可写的表里的值盖住冻结表里的旧值。
    #[test]
    fn freeze_newer_value_wins_over_frozen_value() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"old");

        freeze(&storage);
        storage.put(b"a", b"new");

        assert_eq!(storage.get(b"a").unwrap(), Some(Bytes::from_static(b"new")));
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
        assert_eq!(storage.get(b"a").unwrap(), Some(Bytes::from_static(b"2")));
    }

    // 较新的表里的墓碑挡住较旧的表里的值：读到墓碑就停，不能继续往旧表里找。
    #[test]
    fn freeze_tombstone_hides_value_in_older_table() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        freeze(&storage);
        storage.delete(b"a");

        assert_eq!(storage.get(b"a").unwrap(), None);

        freeze(&storage);

        assert_eq!(storage.get(b"a").unwrap(), None);
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
            assert_eq!(
                storage.get(key).unwrap(),
                Some(Bytes::copy_from_slice(value))
            );
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
                assert_eq!(
                    storage.get(key.as_bytes()).unwrap(),
                    Some(Bytes::from_static(b"v"))
                );
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

        let mut iter = storage
            .scan(Bound::Included(b"b"), Bound::Excluded(b"d"))
            .unwrap();
        let b_to_d = collect_all(&mut iter);
        let mut iter = storage
            .scan(Bound::Excluded(b"b"), Bound::Included(b"d"))
            .unwrap();
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
        let mut iter = storage.scan(Bound::Unbounded, Bound::Unbounded).unwrap();

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

                let got = storage.get(key.as_bytes()).unwrap();
                let got = got.map(|value| String::from_utf8_lossy(&value).into_owned());
                assert_eq!(got, model.get(&key).cloned());
            }
        }
    }

    // ---------- 读路径：SST ----------

    /// 把一串记录写成一个 SST 文件，放到 L0 的最前面（它成为最新的 SST）。
    /// 记录必须已经按 key 从小到大排好。空字符串的 value 是墓碑。
    fn add_l0_sst(
        storage: &LsmStorage,
        dir: &tempfile::TempDir,
        id: usize,
        pairs: &[(&str, &str)],
    ) {
        let path = dir.path().join(format!("{id:05}.sst"));
        // 块的目标大小给得很小，让一个文件里有好几个块。
        let mut builder = crate::table::SsTableBuilder::new(32);
        for (key, value) in pairs {
            builder.add(key.as_bytes(), value.as_bytes());
        }
        let block_cache = storage.block_cache.clone();
        let sst = builder.build(id, block_cache, &path).unwrap();
        let sst = Arc::new(sst.with_stats(Arc::clone(&storage.stats)));

        let _state_lock = storage.state_lock.lock().unwrap();
        let mut guard = storage.state.write().unwrap();
        let mut new_state = guard.as_ref().clone();
        new_state.l0_sstables.insert(0, sst);
        *guard = Arc::new(new_state);
    }

    fn get_str(storage: &LsmStorage, key: &str) -> Option<String> {
        storage
            .get(key.as_bytes())
            .unwrap()
            .map(|value| String::from_utf8_lossy(&value).into_owned())
    }

    /// 扫描一个范围，收集结果。
    fn scan_range(
        storage: &LsmStorage,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Vec<(String, String)> {
        let mut iter = storage.scan(lower, upper).unwrap();
        collect_all(&mut iter)
    }

    // 只在 SST 里的 key 查得到；SST 里没有的 key 查不到，包括落在两条记录之间的。
    #[test]
    fn get_finds_keys_in_sst() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "1"), ("c", "3"), ("e", "5")]);

        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
        assert_eq!(get_str(&storage, "c"), Some("3".to_string()));
        assert_eq!(get_str(&storage, "e"), Some("5".to_string()));
        assert_eq!(get_str(&storage, "b"), None);
        assert_eq!(get_str(&storage, "A"), None);
        assert_eq!(get_str(&storage, "z"), None);
    }

    // 同一个 key 在 memtable 和 SST 里都有：memtable 更新，以它为准。
    #[test]
    fn get_prefers_memtable_over_sst() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "old"), ("b", "2")]);
        storage.put(b"a", b"new");

        assert_eq!(get_str(&storage, "a"), Some("new".to_string()));
        assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
    }

    // 冻结的 memtable 也比 SST 新。
    #[test]
    fn get_prefers_frozen_memtable_over_sst() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "old")]);
        storage.put(b"a", b"frozen");
        freeze(&storage);

        assert_eq!(get_str(&storage, "a"), Some("frozen".to_string()));
    }

    // 同一个 key 在两个 SST 里都有：后放进 L0 的那个更新，以它为准。
    #[test]
    fn get_prefers_newer_sst() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "old"), ("b", "only-in-old")]);
        add_l0_sst(&storage, &dir, 2, &[("a", "new"), ("c", "only-in-new")]);

        assert_eq!(get_str(&storage, "a"), Some("new".to_string()));
        assert_eq!(get_str(&storage, "b"), Some("only-in-old".to_string()));
        assert_eq!(get_str(&storage, "c"), Some("only-in-new".to_string()));
    }

    // memtable 里的墓碑盖住 SST 里的值：查不到，不会接着去 SST 里找出旧值。
    #[test]
    fn get_tombstone_in_memtable_hides_sst_value() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "1")]);
        storage.delete(b"a");

        assert_eq!(get_str(&storage, "a"), None);
    }

    // 较新的 SST 里的墓碑盖住较旧的 SST 里的值。
    #[test]
    fn get_tombstone_in_newer_sst_hides_older_value() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "1"), ("b", "2")]);
        add_l0_sst(&storage, &dir, 2, &[("a", "")]);

        assert_eq!(get_str(&storage, "a"), None);
        assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
    }

    // 较旧的 SST 里是墓碑、较新的 SST 里又写了值：查到的是新值。
    #[test]
    fn get_value_in_newer_sst_overrides_older_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "")]);
        add_l0_sst(&storage, &dir, 2, &[("a", "back")]);

        assert_eq!(get_str(&storage, "a"), Some("back".to_string()));
    }

    // 扫描把 memtable、冻结的表和几个 SST 里的记录合成一个有序的结果。
    #[test]
    fn scan_merges_memtables_and_ssts() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "sst1"), ("d", "sst1")]);
        add_l0_sst(&storage, &dir, 2, &[("b", "sst2"), ("f", "sst2")]);
        storage.put(b"c", b"imm");
        freeze(&storage);
        storage.put(b"e", b"mem");

        let result = scan_range(&storage, Bound::Unbounded, Bound::Unbounded);

        assert_eq!(
            result,
            expected(&[
                ("a", "sst1"),
                ("b", "sst2"),
                ("c", "imm"),
                ("d", "sst1"),
                ("e", "mem"),
                ("f", "sst2"),
            ])
        );
    }

    // 同一个 key 出现在多个来源里：只出现一次，是最新来源里的值；被删除的不出现。
    #[test]
    fn scan_takes_newest_version_across_sources() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(
            &storage,
            &dir,
            1,
            &[("a", "sst1"), ("b", "sst1"), ("c", "sst1"), ("d", "sst1")],
        );
        add_l0_sst(&storage, &dir, 2, &[("b", "sst2"), ("c", "")]);
        storage.put(b"a", b"mem");
        storage.delete(b"d");

        let result = scan_range(&storage, Bound::Unbounded, Bound::Unbounded);

        assert_eq!(result, expected(&[("a", "mem"), ("b", "sst2")]));
    }

    // 下界对 SST 里的记录同样生效：包含、不包含、下界不存在三种情况。
    #[test]
    fn scan_lower_bound_applies_to_ssts() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "1"), ("c", "3"), ("e", "5")]);

        let from_c = scan_range(&storage, Bound::Included(b"c"), Bound::Unbounded);
        let after_c = scan_range(&storage, Bound::Excluded(b"c"), Bound::Unbounded);
        let from_b = scan_range(&storage, Bound::Included(b"b"), Bound::Unbounded);
        let after_b = scan_range(&storage, Bound::Excluded(b"b"), Bound::Unbounded);

        assert_eq!(from_c, expected(&[("c", "3"), ("e", "5")]));
        assert_eq!(after_c, expected(&[("e", "5")]));
        assert_eq!(from_b, expected(&[("c", "3"), ("e", "5")]));
        assert_eq!(after_b, expected(&[("c", "3"), ("e", "5")]));
    }

    // 上界对 SST 里的记录同样生效：SST 的游标自己不会停，由最外层在上界处停下。
    #[test]
    fn scan_upper_bound_applies_to_ssts() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("a", "1"), ("c", "3"), ("e", "5")]);

        let before_c = scan_range(&storage, Bound::Unbounded, Bound::Excluded(b"c"));
        let up_to_c = scan_range(&storage, Bound::Unbounded, Bound::Included(b"c"));
        let before_d = scan_range(&storage, Bound::Unbounded, Bound::Excluded(b"d"));

        assert_eq!(before_c, expected(&[("a", "1")]));
        assert_eq!(up_to_c, expected(&[("a", "1"), ("c", "3")]));
        assert_eq!(before_d, expected(&[("a", "1"), ("c", "3")]));
    }

    // 范围两头都有界，数据分在 memtable 和 SST 里。
    #[test]
    fn scan_bounded_range_over_memtable_and_sst() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(
            &storage,
            &dir,
            1,
            &[("a", "1"), ("c", "3"), ("e", "5"), ("g", "7")],
        );
        storage.put(b"b", b"2");
        storage.put(b"d", b"4");
        storage.put(b"f", b"6");

        let result = scan_range(&storage, Bound::Included(b"b"), Bound::Excluded(b"f"));

        assert_eq!(
            result,
            expected(&[("b", "2"), ("c", "3"), ("d", "4"), ("e", "5")])
        );
    }

    // 随机生成三个 SST、一张冻结的表和一张可写的表，对照有序表这个参考模型：
    // 每个 key 的点查结果、若干随机范围的扫描结果都要一致。
    #[test]
    fn get_and_scan_match_reference_model_with_ssts() {
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next_random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let key_of = |number: u64| format!("key-{number:03}");

        for round in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let storage = open_without_auto_freeze();
            // 模型：key → 最新的 value，空字符串表示已删除。从旧到新依次覆盖。
            let mut model: BTreeMap<String, String> = BTreeMap::new();

            // 三个 SST，id 小的更旧。
            for sst_id in 1..=3 {
                let mut records: BTreeMap<String, String> = BTreeMap::new();
                for _ in 0..(5 + next_random() % 30) {
                    let key = key_of(next_random() % 60);
                    let value = if next_random() % 4 == 0 {
                        String::new()
                    } else {
                        format!("sst{sst_id}-{}", next_random() % 1000)
                    };
                    records.insert(key, value);
                }
                let pairs: Vec<(&str, &str)> = records
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                add_l0_sst(&storage, &dir, sst_id, &pairs);
                model.extend(records);
            }

            // 然后是两批写入，中间冻结一次：先写的进冻结的表，后写的进可写的表。
            for batch in 0..2 {
                for _ in 0..(next_random() % 25) {
                    let key = key_of(next_random() % 60);
                    if next_random() % 4 == 0 {
                        storage.delete(key.as_bytes());
                        model.insert(key, String::new());
                    } else {
                        let value = format!("mem{batch}-{}", next_random() % 1000);
                        storage.put(key.as_bytes(), value.as_bytes());
                        model.insert(key, value);
                    }
                }
                if batch == 0 {
                    freeze(&storage);
                }
            }

            // 点查：每个可能的 key。
            for number in 0..60 {
                let key = key_of(number);
                let expected_value = model.get(&key).filter(|value| !value.is_empty()).cloned();
                assert_eq!(
                    get_str(&storage, &key),
                    expected_value,
                    "第 {round} 轮，点查 {key}"
                );
            }

            // 扫描：全表，加上若干随机的范围。
            let visible: Vec<(String, String)> = model
                .iter()
                .filter(|(_key, value)| !value.is_empty())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            assert_eq!(
                scan_range(&storage, Bound::Unbounded, Bound::Unbounded),
                visible,
                "第 {round} 轮，全表扫描"
            );
            for _ in 0..10 {
                let low = key_of(next_random() % 60);
                let high = key_of(next_random() % 60);
                let lower_included = next_random() % 2 == 0;
                let upper_included = next_random() % 2 == 0;
                let lower = if lower_included {
                    Bound::Included(low.as_bytes())
                } else {
                    Bound::Excluded(low.as_bytes())
                };
                let upper = if upper_included {
                    Bound::Included(high.as_bytes())
                } else {
                    Bound::Excluded(high.as_bytes())
                };
                let expected_in_range: Vec<(String, String)> = visible
                    .iter()
                    .filter(|(key, _value)| {
                        let above_lower = if lower_included {
                            *key >= low
                        } else {
                            *key > low
                        };
                        let below_upper = if upper_included {
                            *key <= high
                        } else {
                            *key < high
                        };
                        above_lower && below_upper
                    })
                    .cloned()
                    .collect();
                assert_eq!(
                    scan_range(&storage, lower, upper),
                    expected_in_range,
                    "第 {round} 轮，扫描 {low} 到 {high}（下界包含 {lower_included}，上界包含 {upper_included}）"
                );
            }
        }
    }

    // ---------- 写路径：flush ----------

    /// 写入几条记录，然后冻结。
    fn put_and_freeze(storage: &LsmStorage, pairs: &[(&str, &str)]) {
        for (key, value) in pairs {
            if value.is_empty() {
                storage.delete(key.as_bytes());
            } else {
                storage.put(key.as_bytes(), value.as_bytes());
            }
        }
        freeze(storage);
    }

    /// 读出一个 SST 里的全部记录，墓碑也在内。
    fn records_in_sst(sst: &Arc<SsTable>) -> Vec<(String, String)> {
        let mut iter = SsTableIterator::new_and_seek_to_first(Arc::clone(sst)).unwrap();
        collect_all(&mut iter)
    }

    // 打开引擎时目录不存在就创建。
    #[test]
    fn open_creates_the_directory() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("新目录").join("db");
        let options = LsmStorageOptions {
            memtable_size_limit: 1024,
            block_size: 64,
            block_cache_size: 1 << 20,
        };

        let _storage = LsmStorage::open(&path, options).unwrap();

        assert!(path.is_dir());
    }

    // flush 把最旧的那张冻结表写成 SST 放进 L0：冻结的表少一张，L0 多一个文件，
    // 文件里正是那张表的记录。
    #[test]
    fn flush_moves_oldest_frozen_memtable_to_l0() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "1"), ("b", "2")]);
        put_and_freeze(&storage, &[("c", "3")]);
        assert_eq!(storage.imm_memtable_count(), 2);

        storage.force_flush_next_imm_memtable().unwrap();

        assert_eq!(storage.imm_memtable_count(), 1);
        assert_eq!(storage.l0_sstable_count(), 1);
        let state = storage.snapshot();
        assert_eq!(
            records_in_sst(&state.l0_sstables[0]),
            expected(&[("a", "1"), ("b", "2")])
        );
        // 留下的是较新的那张表。
        assert_eq!(
            state.imm_memtables[0].get(b"c"),
            Some(Bytes::from_static(b"3"))
        );
    }

    // flush 前后读到的内容不变：点查、扫描都一样。
    #[test]
    fn flush_keeps_data_readable() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "1"), ("c", "3")]);
        put_and_freeze(&storage, &[("b", "2")]);
        storage.put(b"d", b"4");
        let before = scan_all(&storage);

        storage.force_flush_next_imm_memtable().unwrap();

        assert_eq!(scan_all(&storage), before);
        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
        assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
        assert_eq!(get_str(&storage, "c"), Some("3".to_string()));
        assert_eq!(get_str(&storage, "d"), Some("4".to_string()));
        assert_eq!(get_str(&storage, "e"), None);
    }

    // 每次 flush 在目录里写出一个新文件，文件名是递增的编号；L0 里新的在前。
    #[test]
    fn flush_writes_files_named_by_increasing_ids() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "1")]);
        put_and_freeze(&storage, &[("b", "2")]);
        assert_eq!(storage.sst_file_names(), Vec::<String>::new());

        storage.force_flush_next_imm_memtable().unwrap();
        assert_eq!(storage.sst_file_names(), vec!["00001.sst"]);

        storage.force_flush_next_imm_memtable().unwrap();
        assert_eq!(storage.sst_file_names(), vec!["00001.sst", "00002.sst"]);

        let state = storage.snapshot();
        let ids: Vec<usize> = state.l0_sstables.iter().map(|sst| sst.id()).collect();
        assert_eq!(ids, vec![2, 1]);
        // 先冻结的先 flush：编号 1 的文件里是 "a"，编号 2 的文件里是 "b"。
        assert_eq!(
            records_in_sst(&state.l0_sstables[1]),
            expected(&[("a", "1")])
        );
        assert_eq!(
            records_in_sst(&state.l0_sstables[0]),
            expected(&[("b", "2")])
        );
    }

    // 没有冻结的表时，flush 什么都不做，也不报错。
    #[test]
    fn flush_without_frozen_memtable_does_nothing() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");

        storage.force_flush_next_imm_memtable().unwrap();

        assert_eq!(storage.imm_memtable_count(), 0);
        assert_eq!(storage.l0_sstable_count(), 0);
        assert_eq!(storage.sst_file_names(), Vec::<String>::new());
        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
    }

    // 冻结的表是空的：把它从列表里拿掉，但不写文件。
    #[test]
    fn flush_drops_empty_frozen_memtable_without_writing_a_file() {
        let storage = open_without_auto_freeze();
        freeze(&storage);
        assert_eq!(storage.imm_memtable_count(), 1);

        storage.force_flush_next_imm_memtable().unwrap();

        assert_eq!(storage.imm_memtable_count(), 0);
        assert_eq!(storage.l0_sstable_count(), 0);
        assert_eq!(storage.sst_file_names(), Vec::<String>::new());
    }

    // 墓碑要写进 SST：它得继续盖住更旧的 SST 里的值。
    #[test]
    fn flush_writes_tombstones() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "1"), ("b", "2")]);
        storage.force_flush_next_imm_memtable().unwrap();
        put_and_freeze(&storage, &[("a", "")]);

        storage.force_flush_next_imm_memtable().unwrap();

        // 两个 SST：新的那个里是 "a" 的墓碑。
        let state = storage.snapshot();
        assert_eq!(
            records_in_sst(&state.l0_sstables[0]),
            expected(&[("a", "")])
        );
        // 读的时候 "a" 已经不存在，旧文件里的值没有复活。
        assert_eq!(get_str(&storage, "a"), None);
        assert_eq!(scan_all(&storage), expected(&[("b", "2")]));
    }

    // 同一个 key 先后落进两个 SST：读到的是后 flush 的那个里的值。
    #[test]
    fn flush_keeps_newer_value_winning_across_ssts() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "old"), ("b", "keep")]);
        put_and_freeze(&storage, &[("a", "new")]);

        storage.force_flush_next_imm_memtable().unwrap();
        storage.force_flush_next_imm_memtable().unwrap();

        assert_eq!(storage.imm_memtable_count(), 0);
        assert_eq!(get_str(&storage, "a"), Some("new".to_string()));
        assert_eq!(scan_all(&storage), expected(&[("a", "new"), ("b", "keep")]));
    }

    // flush 之后继续写入、冻结、再 flush，各处的数据合在一起仍然正确。
    #[test]
    fn flush_then_keep_writing() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "1"), ("b", "2")]);
        storage.force_flush_next_imm_memtable().unwrap();
        storage.put(b"b", b"22");
        storage.put(b"c", b"3");
        freeze(&storage);
        storage.delete(b"a");

        assert_eq!(scan_all(&storage), expected(&[("b", "22"), ("c", "3")]));

        storage.force_flush_next_imm_memtable().unwrap();

        assert_eq!(scan_all(&storage), expected(&[("b", "22"), ("c", "3")]));
        assert_eq!(storage.l0_sstable_count(), 2);
    }

    // flush 之前就建好的扫描游标不受影响：它手里的那张表还在，照样走完。
    #[test]
    fn flush_does_not_disturb_an_open_scan() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "1"), ("b", "2"), ("c", "3")]);
        let mut iter = storage.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
        assert_eq!(iter.key(), b"a");

        storage.force_flush_next_imm_memtable().unwrap();
        assert_eq!(storage.imm_memtable_count(), 0);

        assert_eq!(
            collect_all(&mut iter),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // 写文件失败（目录被删掉了）：flush 返回读写错误，状态不变，数据仍然读得到。
    #[test]
    fn flush_reports_io_error_and_keeps_state() {
        let storage = open_without_auto_freeze();
        put_and_freeze(&storage, &[("a", "1")]);
        std::fs::remove_dir_all(storage.dir()).unwrap();

        let result = storage.force_flush_next_imm_memtable();

        assert!(matches!(result, Err(crate::error::Error::Io(_))));
        assert_eq!(storage.imm_memtable_count(), 1);
        assert_eq!(storage.l0_sstable_count(), 0);
        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
    }

    // 记录很多、块很小：一张表 flush 成的 SST 有很多个块，读回来一条不少。
    #[test]
    fn flush_large_memtable_into_many_blocks() {
        let storage = open_without_auto_freeze();
        let mut model = Vec::new();
        for i in 0..500 {
            let key = format!("key-{i:04}");
            let value = format!("value-{i:04}");
            storage.put(key.as_bytes(), value.as_bytes());
            model.push((key, value));
        }
        freeze(&storage);

        storage.force_flush_next_imm_memtable().unwrap();

        let state = storage.snapshot();
        assert!(state.l0_sstables[0].block_count() > 50);
        assert_eq!(scan_all(&storage), model);
    }

    // 随机的写入、删除、冻结、flush 混在一起，每一步之后的点查与最后的扫描
    // 都与有序表这个参考模型一致。
    #[test]
    fn flush_matches_reference_model_under_random_operations() {
        let mut seed: u64 = 0xD1B5_4A32_D192_ED03;
        let mut next_random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for round in 0..20 {
            let storage = open_without_auto_freeze();
            let mut model: BTreeMap<String, String> = BTreeMap::new();

            for step in 0..300 {
                let key = format!("key-{:02}", next_random() % 40);
                match next_random() % 20 {
                    0..=11 => {
                        let value = format!("v{step}");
                        storage.put(key.as_bytes(), value.as_bytes());
                        model.insert(key.clone(), value);
                    }
                    12..=15 => {
                        storage.delete(key.as_bytes());
                        model.remove(&key);
                    }
                    16..=17 => freeze(&storage),
                    _ => storage.force_flush_next_imm_memtable().unwrap(),
                }

                assert_eq!(
                    get_str(&storage, &key),
                    model.get(&key).cloned(),
                    "第 {round} 轮第 {step} 步，点查 {key}"
                );
            }

            let expected_records: Vec<(String, String)> = model.into_iter().collect();
            assert_eq!(
                scan_all(&storage),
                expected_records,
                "第 {round} 轮，全表扫描"
            );
        }
    }

    // 几个线程不停写入（上限很小，频繁自动冻结），另一个线程不停 flush：
    // 全部结束并把剩下的表都 flush 掉之后，每一条写入都读得到。
    #[test]
    fn flush_concurrently_with_writes() {
        let storage = open_with_limit(256);
        let writers = 4;
        let puts_per_writer = 500;
        let done = std::sync::atomic::AtomicBool::new(false);

        thread::scope(|scope| {
            let storage = &storage;
            let done = &done;
            let flusher = scope.spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    storage.force_flush_next_imm_memtable().unwrap();
                }
            });

            let mut handles = Vec::new();
            for writer in 0..writers {
                handles.push(scope.spawn(move || {
                    for i in 0..puts_per_writer {
                        let key = format!("w{writer}-{i:04}");
                        storage.put(key.as_bytes(), b"v");
                        // 写完立刻读：不管它此刻在表里还是已经在文件里，都读得到。
                        assert_eq!(get_str(storage, &key), Some("v".to_string()));
                    }
                }));
            }
            // 先等所有线程结束、让 flush 的线程停下，再检查有没有线程出错。
            // 顺序不能反：写入的线程出错时如果直接在这里 panic，`done` 就永远
            // 不会被置位，flush 的线程会一直转下去，测试卡死而看不到报错。
            let writer_results: Vec<_> = handles.into_iter().map(|handle| handle.join()).collect();
            done.store(true, Ordering::SeqCst);
            let flusher_result = flusher.join();
            for result in writer_results {
                result.expect("写入的线程出错了，看上面的报错");
            }
            flusher_result.expect("flush 的线程出错了，看上面的报错");
        });

        // 把剩下的冻结表都 flush 掉。每次 flush 应当让冻结的表少一张；
        // 次数设了上限，flush 如果没有把表拿掉，这里会失败而不是永远转下去。
        let frozen_before = storage.imm_memtable_count();
        for _ in 0..frozen_before {
            storage.force_flush_next_imm_memtable().unwrap();
        }
        assert_eq!(
            storage.imm_memtable_count(),
            0,
            "flush 之后冻结的表没有减少"
        );
        assert!(storage.l0_sstable_count() > 1);
        for writer in 0..writers {
            for i in 0..puts_per_writer {
                let key = format!("w{writer}-{i:04}");
                assert_eq!(get_str(&storage, &key), Some("v".to_string()));
            }
        }
        assert_eq!(scan_all(&storage).len(), writers * puts_per_writer);
    }

    // ---------- 写路径：冻结的通知与 flush 到一张不剩 ----------

    /// 调用 `flush_all_imm_memtables`，设了时限：循环停不下来时测试失败而不是卡死。
    fn flush_all_with_deadline(storage: &Arc<TestStorage>) -> Result<()> {
        let storage = Arc::clone(storage);
        run_with_deadline("flush_all_imm_memtables ", move || {
            storage.flush_all_imm_memtables()
        })
    }

    // 写入的返回值：这一次写入让表到了上限并被冻结时为真，其余时候为假。
    #[test]
    fn write_reports_whether_it_froze_a_memtable() {
        let storage = open_with_limit(40);

        let first = storage.put(b"key-1", b"0123456789");
        assert!(!first);
        assert_eq!(storage.imm_memtable_count(), 0);

        // 一直写到发生冻结为止：只有那一次返回真。
        let mut froze_count = 0;
        for i in 2..10 {
            let froze = storage.put(format!("key-{i}").as_bytes(), b"0123456789");
            if froze {
                froze_count += 1;
            }
            assert_eq!(storage.imm_memtable_count(), froze_count);
        }
        assert!(froze_count >= 1);

        // 删除写入的是墓碑，同样会让表变大、被冻结。
        let frozen_before = storage.imm_memtable_count();
        let mut froze_by_delete = false;
        for i in 0..40 {
            froze_by_delete |= storage.delete(format!("gone-{i:02}").as_bytes());
        }
        assert!(froze_by_delete);
        assert!(storage.imm_memtable_count() > frozen_before);
    }

    // 立即冻结：不管表有多大，原来的表进冻结列表，换上一张空表。
    #[test]
    fn freeze_memtable_freezes_regardless_of_size() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");

        storage.freeze_memtable();
        storage.freeze_memtable();

        assert_eq!(storage.imm_memtable_count(), 2);
        assert_eq!(storage.snapshot().memtable.approximate_size(), 0);
        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
    }

    // flush 到一张不剩：冻结了几张表，一次调用全部写成 SST。
    #[test]
    fn flush_all_leaves_no_frozen_memtable() {
        let storage = Arc::new(open_without_auto_freeze());
        put_and_freeze(&storage, &[("a", "1")]);
        put_and_freeze(&storage, &[("b", "2")]);
        put_and_freeze(&storage, &[("c", "3")]);

        flush_all_with_deadline(&storage).unwrap();

        assert_eq!(storage.imm_memtable_count(), 0);
        assert_eq!(storage.l0_sstable_count(), 3);
        assert_eq!(
            scan_all(&storage),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // 没有冻结的表：什么都不做，直接成功。
    #[test]
    fn flush_all_without_frozen_memtable_does_nothing() {
        let storage = Arc::new(open_without_auto_freeze());
        storage.put(b"a", b"1");

        flush_all_with_deadline(&storage).unwrap();

        assert_eq!(storage.l0_sstable_count(), 0);
        assert!(storage.sst_file_names().is_empty());
    }

    // 中途写失败（目录被删掉了）：停下并返回错误，没写成的表都还在。
    #[test]
    fn flush_all_stops_at_the_first_error() {
        let storage = Arc::new(open_without_auto_freeze());
        put_and_freeze(&storage, &[("a", "1")]);
        put_and_freeze(&storage, &[("b", "2")]);
        std::fs::remove_dir_all(storage.dir()).unwrap();

        let result = flush_all_with_deadline(&storage);

        assert!(matches!(result, Err(crate::error::Error::Io(_))));
        assert_eq!(storage.imm_memtable_count(), 2);
        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
        assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
    }

    // flush 出来的 SST 用的是引擎的块缓存：点查读过的块留在缓存里，再查不增加块数。
    #[test]
    fn reads_fill_the_block_cache() {
        let storage = open_with_limit(usize::MAX);
        for i in 0..100 {
            storage.put(format!("key-{i:03}").as_bytes(), b"value");
        }
        storage.freeze_memtable();
        storage.flush_all_imm_memtables().unwrap();
        assert_eq!(storage.block_cache_usage(), (0, 0));

        assert_eq!(
            storage.get(b"key-042").unwrap(),
            Some(Bytes::from_static(b"value"))
        );
        let (blocks_after_first_get, bytes_after_first_get) = storage.block_cache_usage();
        assert_eq!(blocks_after_first_get, 1);
        assert!(bytes_after_first_get > 0);

        assert_eq!(
            storage.get(b"key-042").unwrap(),
            Some(Bytes::from_static(b"value"))
        );
        assert_eq!(storage.block_cache_usage().0, 1);
    }

    // ---------- 按 key 范围跳过 SST ----------

    /// 三个 key 范围互不重叠的 SST，从新到旧是 [g, i]、[a, c]、[d, f]。
    /// 每个文件只有一个块，所以块缓存里的块数就是读过的文件数。
    fn storage_with_three_disjoint_ssts() -> (TestStorage, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 1, &[("d", "4"), ("e", "5"), ("f", "6")]);
        add_l0_sst(&storage, &dir, 2, &[("a", "1"), ("b", "2"), ("c", "3")]);
        add_l0_sst(&storage, &dir, 3, &[("g", "7"), ("h", "8"), ("i", "9")]);
        assert_eq!(storage.block_cache_usage().0, 0);
        (storage, dir)
    }

    // 点查只读范围包含这个 key 的文件。比它新的两个文件，一个在它之后、一个在它之前，都被跳过。
    #[test]
    fn get_reads_only_the_sst_whose_range_contains_the_key() {
        let (storage, _dir) = storage_with_three_disjoint_ssts();

        assert_eq!(get_str(&storage, "e"), Some("5".to_string()));

        assert_eq!(storage.block_cache_usage().0, 1, "读了范围之外的文件");
    }

    // 落在所有文件范围之外、或者两个文件之间的 key：一个块都不读。
    #[test]
    fn get_reads_no_block_for_keys_outside_every_sst() {
        let (storage, _dir) = storage_with_three_disjoint_ssts();

        assert_eq!(get_str(&storage, "0"), None);
        assert_eq!(get_str(&storage, "cc"), None);
        assert_eq!(get_str(&storage, "z"), None);

        assert_eq!(storage.block_cache_usage().0, 0, "读了范围之外的文件");
    }

    // 扫描只为与范围相交的文件建游标，结果不受影响。
    #[test]
    fn scan_reads_only_the_ssts_overlapping_the_range() {
        let (storage, _dir) = storage_with_three_disjoint_ssts();

        let records = scan_range(&storage, Bound::Included(b"d"), Bound::Excluded(b"g"));

        assert_eq!(records, expected(&[("d", "4"), ("e", "5"), ("f", "6")]));
        assert_eq!(storage.block_cache_usage().0, 1, "读了与范围不相交的文件");
    }

    // 跨过文件边界的扫描与不设范围的扫描：结果完整，按 key 排好。
    #[test]
    fn scan_across_ssts_still_returns_everything_in_range() {
        let (storage, _dir) = storage_with_three_disjoint_ssts();

        let middle = scan_range(&storage, Bound::Excluded(b"b"), Bound::Included(b"h"));
        let all = scan_range(&storage, Bound::Unbounded, Bound::Unbounded);

        assert_eq!(
            middle,
            expected(&[
                ("c", "3"),
                ("d", "4"),
                ("e", "5"),
                ("f", "6"),
                ("g", "7"),
                ("h", "8")
            ])
        );
        assert_eq!(all.len(), 9);
    }

    // ---------- 统计计数器 ----------

    // 写入记下 key 与 value 的字节；flush 记下文件的长度；点查记下进入的 SST 与从文件读出的块。
    #[test]
    fn counters_follow_writes_flushes_and_gets() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");
        storage.put(b"bb", b"22");
        storage.delete(b"c");
        assert_eq!(storage.stats().user_bytes_written, 2 + 4 + 1);

        storage.freeze_memtable();
        storage.flush_all_imm_memtables().unwrap();
        let sst_file_size = storage.snapshot().l0_sstables[0].file_size();
        assert_eq!(storage.stats().flush_bytes_written, sst_file_size);

        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
        // 范围之外：不进入 SST。
        assert_eq!(get_str(&storage, "z"), None);

        let stats = storage.stats();
        assert_eq!(stats.gets, 3);
        assert_eq!(stats.tables_probed, 2);
        // 第二次查同一个块命中缓存，不再读文件。
        assert_eq!(stats.blocks_read, 1);
    }

    // 内存里查到的点查也算一次点查，但不进入任何 SST。
    #[test]
    fn gets_answered_from_memory_probe_no_sst() {
        let storage = open_without_auto_freeze();
        storage.put(b"a", b"1");

        assert_eq!(get_str(&storage, "a"), Some("1".to_string()));

        let stats = storage.stats();
        assert_eq!(stats.gets, 1);
        assert_eq!(stats.tables_probed, 0);
        assert_eq!(stats.blocks_read, 0);
    }

    // 块缓存的容量设为 0 时没有缓存：每次读块都从文件读，结果照样正确。
    #[test]
    fn zero_block_cache_size_disables_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let options = LsmStorageOptions {
            memtable_size_limit: usize::MAX,
            block_size: 64,
            block_cache_size: 0,
        };
        let storage = LsmStorage::open(dir.path(), options).unwrap();
        storage.put(b"a", b"1");
        storage.freeze_memtable();
        storage.flush_all_imm_memtables().unwrap();

        for _ in 0..3 {
            assert_eq!(get_str(&storage, "a"), Some("1".to_string()));
        }

        assert_eq!(storage.block_cache_usage(), (0, 0));
        assert_eq!(storage.stats().blocks_read, 3);
    }
}
