//! 对外的句柄：拥有引擎，以及在后台做 flush 与 compaction 的两个线程。
//!
//! `LsmStorage` 只管数据怎么组织，自己不起线程。`Db` 在它外面包一层：
//!
//! - 打开时起一个后台 flush 线程，有冻结的 memtable 就把它们写成 SST；
//!   选项里给了 compaction 的参数时，再起一个后台 compaction 线程，按策略反复合并；
//! - 写入冻结了一张表时给 flush 线程发 `Wake`，让它立刻开始 flush；
//! - 关闭时让两个线程停下、等它们结束，再把还在内存里的数据都写成 SST。
//!
//! 引擎用 `Arc` 包着，`Db` 和每个后台线程各拿一份指针。
//!
//! 后台线程只有两种结束的方式：收到停止的信号，或者发信号的那一头没了。
//! 它们由 `Db` 启动，也由 `Db` 负责叫停并等它们结束。

use std::ops::Bound;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bytes::Bytes;

use crate::compaction::CompactionOptions;
use crate::error::Result;
use crate::lsm_storage::{
    LsmStorage, LsmStorageOptions, ScanIterator, WriteBatchRecord, WriteOptions,
};
use crate::stats::StatsSnapshot;

/// flush 线程等信号的最长时间。等满这么久还没有信号，它也会检查一次有没有冻结的
/// memtable，有就写成 SST。
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// compaction 线程两次检查之间等待的时间。没有人叫醒它，它就每隔这么久看一次
/// 要不要合并。
const COMPACTION_INTERVAL: Duration = Duration::from_millis(50);

/// 发给后台线程（flush 或 compaction）的信号。
enum Signal {
    /// 有活要干，立刻检查一次，不必等到下一次自己醒来。
    /// 写入冻结了一张 memtable 时发给 flush 线程。
    Wake,
    /// 关闭或释放 `Db` 时发出。线程收到后不再开始新的工作，函数返回，线程结束。
    Stop,
}

pub struct Db {
    /// 引擎本身。每个后台线程手里有另一份指向它的指针。
    storage: Arc<LsmStorage>,
    /// 通道的发送端，用来给 flush 线程发信号。接收端在线程手里。
    flush_sender: Sender<Signal>,
    /// flush 线程的把手，等它结束时要用。
    ///
    /// 等待会把把手消耗掉，所以放在 `Option` 里：停线程时把它取走，原处留下 `None`。
    /// 外面套一把锁，是因为取走要修改这个字段，而停线程的方法拿到的只是 `&self`。
    flush_thread: Mutex<Option<JoinHandle<()>>>,
    /// 给 compaction 线程发信号的发送端。没有启动 compaction 线程时，接收端已经丢掉，
    /// 往这里发送会失败，不会有任何效果。
    compaction_sender: Sender<Signal>,
    /// compaction 线程的把手，用法与 `flush_thread` 相同。选项里没有 compaction 的参数时
    /// 不启动这个线程，一开始就是 `None`。
    compaction_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Db {
    /// 在 `path` 这个目录里打开数据库，并启动后台线程。
    pub fn open(path: &Path, options: LsmStorageOptions) -> Result<Self> {
        Self::open_with_intervals(path, options, FLUSH_INTERVAL, COMPACTION_INTERVAL)
    }

    /// 与 `open` 相同，只是两个后台线程自己醒来的间隔由调用方给出。测试用它。
    fn open_with_intervals(
        path: &Path,
        options: LsmStorageOptions,
        flush_interval: Duration,
        compaction_interval: Duration,
    ) -> Result<Self> {
        // 选项要整个交给引擎，先看清要不要起 compaction 线程。
        let start_compaction_thread =
            !matches!(options.compaction, CompactionOptions::NoCompaction);
        let storage = Arc::new(LsmStorage::open(path, options)?);

        // 线程什么时候结束不由这个函数决定，所以它不能借用这里的任何东西，
        // 要用的都得整个交给它：一份指向引擎的指针，和通道的接收端。
        // `move` 把这些变量搬进闭包，之后这个函数里就不能再用它们了。
        let (flush_sender, flush_receiver) = mpsc::channel();
        let storage_for_flush = Arc::clone(&storage);
        let flush_thread = thread::Builder::new()
            // 给线程起名字：panic 的信息和调试器里都会显示它。
            .name("lsm-flush".to_string())
            .spawn(move || flush_loop(storage_for_flush, flush_receiver, flush_interval))?;

        let (compaction_sender, compaction_receiver) = mpsc::channel();
        let compaction_thread = if start_compaction_thread {
            let storage_for_compaction = Arc::clone(&storage);
            let handle = thread::Builder::new()
                .name("lsm-compaction".to_string())
                .spawn(move || {
                    compaction_loop(
                        storage_for_compaction,
                        compaction_receiver,
                        compaction_interval,
                    )
                })?;
            Some(handle)
        } else {
            // 不自动合并：接收端在这里被丢掉，没有线程。
            None
        };

        Ok(Self {
            storage,
            flush_sender,
            flush_thread: Mutex::new(Some(flush_thread)),
            compaction_sender,
            compaction_thread: Mutex::new(compaction_thread),
        })
    }

    /// 写入一条记录，用默认的选项（不同步）。key 与 value 都不能为空。
    ///
    /// key 或 value 超过长度上限时返回 [`crate::error::Error::InvalidArgument`]，
    /// 追加 WAL 失败时返回 [`crate::error::Error::Io`]。
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.put_opt(key, value, WriteOptions::default())
    }

    /// 删除一个 key，用默认的选项。
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        self.delete_opt(key, WriteOptions::default())
    }

    /// 写入一条记录，选项由调用方给：`sync` 为真时返回之前 WAL 已落盘。
    pub fn put_opt(&self, key: &[u8], value: &[u8], options: WriteOptions) -> Result<()> {
        let froze = self.storage.put_opt(key, value, options)?;
        if froze {
            self.wake_flush_thread();
        }
        Ok(())
    }

    /// 删除一个 key，选项由调用方给。
    pub fn delete_opt(&self, key: &[u8], options: WriteOptions) -> Result<()> {
        let froze = self.storage.delete_opt(key, options)?;
        if froze {
            self.wake_flush_thread();
        }
        Ok(())
    }

    /// 写入一批记录，用默认的选项。整批是 WAL 里的一条记录，崩溃之后要么全在、要么全不在；
    /// 同一个 key 出现几次时，后面的覆盖前面的。错误与 `put` 相同，有一条不合法时整批都不写。
    pub fn write_batch<T: AsRef<[u8]>>(&self, batch: &[WriteBatchRecord<T>]) -> Result<()> {
        self.write_batch_opt(batch, WriteOptions::default())
    }

    /// 写入一批记录，选项由调用方给。
    pub fn write_batch_opt<T: AsRef<[u8]>>(
        &self,
        batch: &[WriteBatchRecord<T>],
        options: WriteOptions,
    ) -> Result<()> {
        let froze = self.storage.write_batch_opt(batch, options)?;
        if froze {
            self.wake_flush_thread();
        }
        Ok(())
    }

    /// 把 WAL 同步到磁盘：返回之后，之前完成的写入断电也不会丢。没开 WAL 时什么都不做。
    pub fn sync(&self) -> Result<()> {
        self.storage.sync()
    }

    /// 查找一个 key；不存在或已被删除时返回 `None`。
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.storage.get(key)
    }

    /// 按 key 从小到大扫描范围内的记录，已删除的 key 不出现。
    pub fn scan(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<ScanIterator> {
        self.storage.scan(lower, upper)
    }

    /// 当前有几张冻结的 memtable 还没有写成 SST。
    pub fn imm_memtable_count(&self) -> usize {
        self.storage.imm_memtable_count()
    }

    /// 当前 L0 有几个 SST。
    pub fn l0_sstable_count(&self) -> usize {
        self.storage.l0_sstable_count()
    }

    /// 是否选用了 tiered 策略：不用 L0，`level_sstable_counts` 给出的是各个 tier 的文件数。
    pub fn uses_tiers(&self) -> bool {
        self.storage.uses_tiers()
    }

    /// 当前 L1 往下各层分别有几个 SST，第 0 项是 L1。选用 tiered 策略时是各个 tier 的，
    /// 第 0 项最新。
    pub fn level_sstable_counts(&self) -> Vec<usize> {
        self.storage.level_sstable_counts()
    }

    /// 按后台合并的参数，现在的形状还有没有要做的合并任务。后台正在做的那个任务换进状态
    /// 之前也算。没有开启自动合并时总是 `false`。
    ///
    /// 用来等后台合并做完，例如基准在两段负载之间。
    pub fn has_pending_compaction(&self) -> bool {
        self.storage.has_pending_compaction()
    }

    /// 把全部文件合并成一个 sorted run，见 [`LsmStorage::force_full_compaction`]。
    ///
    /// 在调用它的线程里做完才返回；后台的 flush 照常进行。后台的 compaction 正在合并时，
    /// 等它这个任务做完再开始。
    pub fn force_full_compaction(&self) -> Result<()> {
        self.storage.force_full_compaction()
    }

    /// 块缓存里现在有几个块、一共多少字节。
    pub fn block_cache_usage(&self) -> (u64, u64) {
        self.storage.block_cache_usage()
    }

    /// 统计计数器当前的值：写入与 flush 的字节、点查次数、进入的 SST、从文件读出的块。
    pub fn stats(&self) -> StatsSnapshot {
        self.storage.stats()
    }

    /// 关闭数据库：停下后台线程，把还在内存里的数据全部写成 SST。
    ///
    /// 没开 WAL 时，memtable 里的数据只在内存里，正常退出前必须调用这个方法，否则最后写入的
    /// 那部分数据就丢了。开了 WAL 时不调用也不丢（下次打开时重放 WAL），调用之后下次打开
    /// 不用重放，WAL 也随 flush 删掉。
    ///
    /// 不管成功还是失败，返回时后台线程都已经结束。compaction 线程正在合并时，
    /// 等它把手上这个任务做完。写文件失败时返回错误，没写成的数据仍然留在内存里、
    /// 照常读得到；可以再调用一次重试。调用成功之后不应再写入。
    pub fn close(&self) -> Result<()> {
        // 先停 compaction：下面的 flush 会产生新的 L0 文件，不再为它们开始新的合并。
        stop_thread(
            &self.compaction_thread,
            &self.compaction_sender,
            "compaction",
        );
        // 再停 flush：从这里往后，做 flush 的只有当前这个线程。
        stop_thread(&self.flush_thread, &self.flush_sender, "flush");
        // 可写的那张表里也有数据，冻结之后才能 flush。
        self.storage.freeze_memtable()?;
        self.storage.flush_all_imm_memtables()
    }

    /// 给 flush 线程发 `Wake`：它收到后把冻结的 memtable 全部写成 SST。
    fn wake_flush_thread(&self) {
        // 发送只在接收端已经不存在时失败，也就是后台线程已经结束了。
        // 那时没有人可叫，什么都不用做。
        let _ = self.flush_sender.send(Signal::Wake);
    }
}

impl Drop for Db {
    /// `Db` 被释放时停下后台线程，不让它们留在进程里继续跑。
    ///
    /// 这里不做 flush：flush 会失败，而 `drop` 没有办法把错误交给调用方。
    /// 要保住数据，得在释放之前调用 `close`。
    fn drop(&mut self) {
        stop_thread(
            &self.compaction_thread,
            &self.compaction_sender,
            "compaction",
        );
        stop_thread(&self.flush_thread, &self.flush_sender, "flush");
    }
}

/// 让一个后台线程停下，并等它结束。线程已经停过了、或者从来没启动过，就什么都不做。
///
/// `thread` 是放线程把手的地方，`sender` 是给它发信号的发送端，`name` 写进出错信息里。
fn stop_thread(thread: &Mutex<Option<JoinHandle<()>>>, sender: &Sender<Signal>, name: &str) {
    // 把把手取走。这一句结束时锁就放开了：下面等线程结束可能要等很久，
    // 不能拿着锁等。
    let handle = thread.lock().unwrap().take();
    let Some(handle) = handle else {
        return;
    };

    // 发送失败说明线程已经自己结束了，照样往下等它。
    let _ = sender.send(Signal::Stop);
    // 线程正在写文件的话，会先把手上的写完再看信号，所以这里会等到它写完。
    if handle.join().is_err() {
        eprintln!("后台 {name} 线程是 panic 结束的");
    }
}

/// flush 线程执行的函数：等信号，有冻结的 memtable 就把它们都写成 SST，如此反复。
///
/// 每一轮最多等 `interval` 这么久。等到的结果有四种：
///
/// - 收到 `Wake`，或者等到超时：把冻结的表都 flush 掉，然后进入下一轮；
/// - 收到 `Stop`，或者发送端已经不存在：函数返回，线程随之结束。
///
/// flush 失败时线程不能跟着结束：把错误打印出来，下一轮再试。
/// 这一轮没写成的表还留在冻结列表里，数据没有丢。
fn flush_loop(storage: Arc<LsmStorage>, receiver: Receiver<Signal>, interval: Duration) {
    loop {
        // 最多等 `interval` 这么久。这里的 `Ok` 只表示收到了一条信号，`Err` 只表示没收到，
        // 所以按"接下来做什么"分成两组，而不是按 `Ok` 和 `Err` 分。
        match receiver.recv_timeout(interval) {
            Ok(Signal::Wake) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Signal::Stop) | Err(RecvTimeoutError::Disconnected) => return,
        }

        // 失败时线程不结束：没写成的表还在冻结列表里，下一轮再试。
        if let Err(error) = storage.flush_all_imm_memtables() {
            eprintln!("后台 flush 失败，下一轮重试：{error}");
        }
    }
}

/// compaction 线程执行的函数：等信号或超时，然后按策略反复合并，直到没有任务，如此反复。
///
/// 每一轮最多等 `interval` 这么久，等到的结果与 `flush_loop` 一样分成两组：
/// `Wake` 或超时就开始合并；`Stop` 或发送端已经不存在就返回。
///
/// 醒来之后反复调用 `compact_once`，直到它返回 `false`（没有任务了）：一次合并之后
/// 常常还有下一个任务，不必等到下一轮。用哪种策略由引擎的选项决定。每做完一个任务看一眼有没有 `Stop`，
/// 有就立刻返回：一连串任务可能要做很久，关闭时不该等它们全部做完。
///
/// 合并失败时线程不结束：打印错误，这一轮不再继续，下一轮再试。失败的任务没有改变状态
/// （或者状态已经换好、只是旧文件没删掉），数据没有丢。
fn compaction_loop(storage: Arc<LsmStorage>, receiver: Receiver<Signal>, interval: Duration) {
    loop {
        match receiver.recv_timeout(interval) {
            Ok(Signal::Wake) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Signal::Stop) | Err(RecvTimeoutError::Disconnected) => return,
        }

        // 一个接一个地做，直到没有任务。
        loop {
            match storage.compact_once() {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    eprintln!("后台 compaction 失败，下一轮重试：{error}");
                    break;
                }
            }
            // 做完一个任务，看一眼有没有 Stop。没有信号时 `try_recv` 立刻返回 `Empty`，不等。
            match receiver.try_recv() {
                Ok(Signal::Wake) | Err(TryRecvError::Empty) => {}
                Ok(Signal::Stop) | Err(TryRecvError::Disconnected) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::leveled::LeveledOptions;
    use crate::compaction::simple_leveled::SimpleLeveledOptions;
    use crate::compaction::tiered::TieredOptions;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use crate::table::{FileObject, SsTable, SsTableIterator};
    use crate::test_util::{run_with_deadline, run_with_slow_deadline, wait_until};

    /// 长到测试期间不会到点：用它时，后台线程只可能是被信号叫醒的。
    const NEVER: Duration = Duration::from_secs(3600);
    /// 短到很快就会到点：用它时，不发信号后台线程也会自己醒来。
    const SOON: Duration = Duration::from_millis(10);

    fn options_with_limit(memtable_size_limit: usize) -> LsmStorageOptions {
        LsmStorageOptions {
            memtable_size_limit,
            block_size: 64,
            block_cache_size: 1 << 20,
            target_sst_size: 1024,
            max_levels: 3,
            compaction: CompactionOptions::NoCompaction,
            enable_wal: false,
        }
    }

    /// 测试用的合并参数：简单分层，L0 攒到 2 个文件就并进 L1，下层至少是上层的 2 倍。
    fn compaction_options() -> CompactionOptions {
        CompactionOptions::SimpleLeveled(SimpleLeveledOptions {
            level0_file_num_compaction_trigger: 2,
            size_ratio_percent: 200,
        })
    }

    /// 测试用的 tiered 参数：4 个 tier 开始检查，其余是 RocksDB 的默认值。
    fn tiered_options() -> CompactionOptions {
        CompactionOptions::Tiered(TieredOptions {
            num_tiers: 4,
            max_size_amplification_percent: 200,
            size_ratio: 1,
            min_merge_width: 2,
            max_merge_width: None,
        })
    }

    /// 测试用的 leveled 参数：触发值 2、比例 2、基准大小 1 KB，数据不多也会用到好几层，
    /// 部分合并与 trivial move 都会发生。
    fn leveled_options() -> CompactionOptions {
        CompactionOptions::Leveled(LeveledOptions {
            level0_file_num_compaction_trigger: 2,
            level_size_multiplier: 2,
            base_level_size_bytes: 1024,
        })
    }

    /// 目录里现有的 SST 文件名，按名字排序。
    fn sst_file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sst"))
            .collect();
        names.sort();
        names
    }

    /// 直接从目录里的 SST 文件读出全部记录，不经过引擎；文件按编号从小到大读。
    fn records_in_sst_files(dir: &Path) -> Vec<(String, String)> {
        let mut records = Vec::new();
        for (index, name) in sst_file_names(dir).iter().enumerate() {
            let file = FileObject::open(&dir.join(name)).unwrap();
            let sst = Arc::new(SsTable::open(index, None, file).unwrap());
            let mut iter = SsTableIterator::new_and_seek_to_first(sst).unwrap();
            records.extend(collect_all(&mut iter));
        }
        records
    }

    // ---------- 后台线程执行的函数 ----------

    /// 一个正在跑 `flush_loop` 的线程，连同它用的引擎和通道。
    struct RunningLoop {
        storage: Arc<LsmStorage>,
        /// 放在 `Option` 里，测试可以把发送端取走丢掉。
        sender: Option<Sender<Signal>>,
        handle: JoinHandle<()>,
        dir: tempfile::TempDir,
    }

    impl RunningLoop {
        /// 引擎不自动冻结；线程自己醒来的间隔是 `interval`。
        fn start(interval: Duration) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let options = options_with_limit(usize::MAX);
            let storage = Arc::new(LsmStorage::open(dir.path(), options).unwrap());
            let (sender, receiver) = mpsc::channel();
            let storage_for_thread = Arc::clone(&storage);
            let handle = thread::spawn(move || flush_loop(storage_for_thread, receiver, interval));
            Self {
                storage,
                sender: Some(sender),
                handle,
                dir,
            }
        }

        fn send(&self, signal: Signal) {
            self.sender.as_ref().unwrap().send(signal).unwrap();
        }

        /// 写入一条记录然后冻结，不通知线程。
        fn put_and_freeze(&self, key: &str, value: &str) {
            self.storage.put(key.as_bytes(), value.as_bytes()).unwrap();
            self.storage.freeze_memtable().unwrap();
        }
    }

    impl Drop for RunningLoop {
        /// 测试结束时让线程停下。只发信号、不等它：线程要是不听停止的信号，
        /// 等它会让每个测试都卡住，那个错误由专门的测试去报。
        fn drop(&mut self) {
            if let Some(sender) = &self.sender {
                let _ = sender.send(Signal::Stop);
            }
        }
    }

    // 收到 Wake：把冻结的表写成 SST。间隔极长，所以只可能是信号起的作用。
    #[test]
    fn loop_flushes_when_woken() {
        let running = RunningLoop::start(NEVER);
        running.put_and_freeze("a", "1");

        running.send(Signal::Wake);

        assert!(
            wait_until(|| running.storage.l0_sstable_count() == 1),
            "收到 Wake 之后没有 flush"
        );
        assert_eq!(running.storage.imm_memtable_count(), 0);
    }

    // 一次 Wake 要把当时所有冻结的表都写完，而不是只写一张。
    #[test]
    fn loop_flushes_every_frozen_memtable_on_one_wake() {
        let running = RunningLoop::start(NEVER);
        running.put_and_freeze("a", "1");
        running.put_and_freeze("b", "2");
        running.put_and_freeze("c", "3");

        running.send(Signal::Wake);

        assert!(
            wait_until(|| running.storage.imm_memtable_count() == 0),
            "一次 Wake 之后还有冻结的表没有 flush"
        );
        assert_eq!(running.storage.l0_sstable_count(), 3);
    }

    // 没有人发信号：到了间隔线程自己醒来，照样 flush。
    #[test]
    fn loop_flushes_on_timeout_without_any_signal() {
        let running = RunningLoop::start(SOON);

        running.put_and_freeze("a", "1");

        assert!(
            wait_until(|| running.storage.l0_sstable_count() == 1),
            "等到超时之后没有 flush"
        );
    }

    // 超时不是结束：线程醒来发现没有活，接着等下一轮。
    #[test]
    fn loop_keeps_running_across_timeouts() {
        let running = RunningLoop::start(SOON);

        // 让它空转若干轮。
        thread::sleep(SOON * 10);
        assert!(!running.handle.is_finished(), "线程在超时之后结束了");
        running.put_and_freeze("a", "1");

        assert!(wait_until(|| running.storage.l0_sstable_count() == 1));
    }

    // 收到 Stop：函数返回，线程结束。
    #[test]
    fn loop_exits_on_stop() {
        let running = RunningLoop::start(NEVER);

        running.send(Signal::Stop);

        assert!(
            wait_until(|| running.handle.is_finished()),
            "收到 Stop 之后线程没有结束"
        );
    }

    // 发送端没了（没有人能再发信号）：函数返回，线程结束。
    #[test]
    fn loop_exits_when_sender_is_gone() {
        let mut running = RunningLoop::start(NEVER);

        drop(running.sender.take());

        assert!(
            wait_until(|| running.handle.is_finished()),
            "发送端没了之后线程没有结束"
        );
    }

    // flush 失败（目录被删掉了）：线程不结束，表还在；目录恢复之后下一轮写成功。
    #[test]
    fn loop_survives_flush_error_and_retries() {
        let running = RunningLoop::start(SOON);
        std::fs::remove_dir_all(running.dir.path()).unwrap();
        running.put_and_freeze("a", "1");

        // 让它失败若干轮。
        thread::sleep(SOON * 10);
        assert!(!running.handle.is_finished(), "flush 失败之后线程结束了");
        assert_eq!(running.storage.imm_memtable_count(), 1);
        assert_eq!(running.storage.l0_sstable_count(), 0);

        std::fs::create_dir(running.dir.path()).unwrap();

        assert!(
            wait_until(|| running.storage.l0_sstable_count() == 1),
            "目录恢复之后没有重试成功"
        );
        assert_eq!(running.storage.imm_memtable_count(), 0);
    }

    // ---------- compaction 线程执行的函数 ----------

    /// 一个正在跑 `compaction_loop` 的线程，连同它用的引擎和通道。
    struct RunningCompaction {
        storage: Arc<LsmStorage>,
        /// 放在 `Option` 里，测试可以把发送端取走丢掉。
        sender: Option<Sender<Signal>>,
        handle: JoinHandle<()>,
        dir: tempfile::TempDir,
    }

    impl RunningCompaction {
        /// 引擎不自动冻结、只有 3 层；合并参数是 `compaction_options()`；
        /// 线程自己醒来的间隔是 `interval`。
        fn start(interval: Duration) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut options = options_with_limit(usize::MAX);
            options.compaction = compaction_options();
            let storage = Arc::new(LsmStorage::open(dir.path(), options).unwrap());
            let (sender, receiver) = mpsc::channel();
            let storage_for_thread = Arc::clone(&storage);
            let handle =
                thread::spawn(move || compaction_loop(storage_for_thread, receiver, interval));
            Self {
                storage,
                sender: Some(sender),
                handle,
                dir,
            }
        }

        fn send(&self, signal: Signal) {
            self.sender.as_ref().unwrap().send(signal).unwrap();
        }

        /// 写入一条记录，冻结并 flush 成一个 L0 文件。在测试自己的线程里做，不经过后台线程。
        fn flush_one_file(&self, key: &str, value: &str) {
            self.storage.put(key.as_bytes(), value.as_bytes()).unwrap();
            self.storage.freeze_memtable().unwrap();
            self.storage.flush_all_imm_memtables().unwrap();
        }

        /// 现在的形状是否满足合并策略：没有要做的任务。
        fn settled(&self) -> bool {
            !self.storage.has_pending_compaction()
        }
    }

    impl Drop for RunningCompaction {
        /// 测试结束时让线程停下。只发信号、不等它，理由与 `RunningLoop` 相同。
        fn drop(&mut self) {
            if let Some(sender) = &self.sender {
                let _ = sender.send(Signal::Stop);
            }
        }
    }

    // 一次 Wake 就合并到没有任务为止：L0 的 2 个文件要连着做三个任务
    // （L0→L1、L1→L2、L2→L3）才停。间隔极长，所以只可能是这一次 Wake 做完的。
    #[test]
    fn compaction_loop_runs_tasks_until_none_is_left_on_one_wake() {
        let running = RunningCompaction::start(NEVER);
        running.flush_one_file("a", "1");
        running.flush_one_file("b", "2");
        assert!(!running.settled());

        running.send(Signal::Wake);

        assert!(
            wait_until(|| running.settled()),
            "一次 Wake 之后没有合并到停下：{:?}",
            running.storage.level_sstable_counts()
        );
        assert_eq!(running.storage.l0_sstable_count(), 0);
        assert_eq!(running.storage.level_sstable_counts(), vec![0, 0, 1]);
    }

    // 没有人发信号：到了间隔线程自己醒来，照样合并。
    #[test]
    fn compaction_loop_compacts_on_timeout_without_any_signal() {
        let running = RunningCompaction::start(SOON);

        running.flush_one_file("a", "1");
        running.flush_one_file("b", "2");

        assert!(wait_until(|| running.settled()), "等到超时之后没有合并");
        assert_eq!(running.storage.l0_sstable_count(), 0);
    }

    // 超时不是结束：线程醒来发现没有任务，接着等下一轮；之后有了任务照样合并。
    #[test]
    fn compaction_loop_keeps_running_while_idle() {
        let running = RunningCompaction::start(SOON);

        // 让它空转若干轮。
        thread::sleep(SOON * 10);
        assert!(!running.handle.is_finished(), "线程在没有任务时结束了");
        running.flush_one_file("a", "1");
        running.flush_one_file("b", "2");

        assert!(wait_until(|| running.settled()));
    }

    // 收到 Stop：函数返回，线程结束。
    #[test]
    fn compaction_loop_exits_on_stop() {
        let running = RunningCompaction::start(NEVER);

        running.send(Signal::Stop);

        assert!(
            wait_until(|| running.handle.is_finished()),
            "收到 Stop 之后线程没有结束"
        );
    }

    // 发送端没了：函数返回，线程结束。
    #[test]
    fn compaction_loop_exits_when_sender_is_gone() {
        let mut running = RunningCompaction::start(NEVER);

        drop(running.sender.take());

        assert!(
            wait_until(|| running.handle.is_finished()),
            "发送端没了之后线程没有结束"
        );
    }

    // 一连串任务做到一半收到 Stop：做完手上的这个就返回，不把剩下的做完。
    // Wake 和 Stop 紧挨着发出：线程做完第一个任务（要写文件、同步，至少几百微秒）时，
    // Stop 早已在通道里等着。
    #[test]
    fn compaction_loop_checks_for_stop_between_tasks() {
        let running = RunningCompaction::start(NEVER);
        running.flush_one_file("a", "1");
        running.flush_one_file("b", "2");

        running.send(Signal::Wake);
        running.send(Signal::Stop);

        assert!(
            wait_until(|| running.handle.is_finished()),
            "收到 Stop 之后线程没有结束"
        );
        // 要连做三个任务才能停下；在任务之间看到了 Stop，就没有做完。
        assert!(
            !running.settled(),
            "收到 Stop 之后还把剩下的任务做完了：{:?}",
            running.storage.level_sstable_counts()
        );
    }

    // 合并失败（目录被删掉了，写不了新文件）：线程不结束，状态不变；目录恢复之后再叫醒它，
    // 合并成功。间隔极长，线程只在收到 Wake 时干活：删目录时不会撞上它正在写文件。
    #[test]
    fn compaction_loop_survives_errors_and_retries() {
        let running = RunningCompaction::start(NEVER);
        running.flush_one_file("a", "1");
        running.flush_one_file("b", "2");
        std::fs::remove_dir_all(running.dir.path()).unwrap();

        running.send(Signal::Wake);
        // 让它失败一次。
        thread::sleep(SOON * 10);
        assert!(!running.handle.is_finished(), "合并失败之后线程结束了");
        assert_eq!(running.storage.l0_sstable_count(), 2);

        std::fs::create_dir(running.dir.path()).unwrap();

        // 写新文件、换状态都会成功；删除旧文件会失败（它们随目录一起没了），这一轮停下。
        // 所以一直叫醒它，每次接着做后面的任务，直到停下。
        let settled = wait_until(|| {
            running.send(Signal::Wake);
            running.settled()
        });
        assert!(
            settled,
            "目录恢复之后没有重试成功：{:?}",
            running.storage.level_sstable_counts()
        );
        assert_eq!(running.storage.l0_sstable_count(), 0);
    }

    // ---------- Db ----------

    /// 测试用的数据库：开在一个临时目录里，目录随它一起清理。
    ///
    /// 实现了 `Deref`，可以直接当作 `Db` 用。释放时给 `Db` 的释放设了时限：
    /// 后台线程不肯结束的话，测试失败而不是卡死。
    struct TestDb {
        /// 用 `Arc` 包着，是为了能把它交给另一个线程去关闭或释放。
        db: Option<Arc<Db>>,
        dir: tempfile::TempDir,
    }

    impl std::ops::Deref for TestDb {
        type Target = Db;

        fn deref(&self) -> &Db {
            self.db.as_ref().unwrap()
        }
    }

    impl TestDb {
        fn open(memtable_size_limit: usize, flush_interval: Duration) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let options = options_with_limit(memtable_size_limit);
            let db = Db::open_with_intervals(dir.path(), options, flush_interval, SOON).unwrap();
            Self {
                db: Some(Arc::new(db)),
                dir,
            }
        }

        /// 打开一个会在后台自动合并的数据库：memtable 写满 `memtable_size_limit` 字节就冻结，
        /// 合并的参数是 `compaction_options()`，compaction 线程每隔 `compaction_interval`
        /// 自己醒来一次。
        fn open_with_compaction(memtable_size_limit: usize, compaction_interval: Duration) -> Self {
            Self::open_with_strategy(
                memtable_size_limit,
                compaction_interval,
                compaction_options(),
            )
        }

        /// 与 `open_with_compaction` 相同，只是合并策略由调用方给出。
        fn open_with_strategy(
            memtable_size_limit: usize,
            compaction_interval: Duration,
            compaction: CompactionOptions,
        ) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut options = options_with_limit(memtable_size_limit);
            options.compaction = compaction;
            let db =
                Db::open_with_intervals(dir.path(), options, SOON, compaction_interval).unwrap();
            Self {
                db: Some(Arc::new(db)),
                dir,
            }
        }

        /// 现在的形状是否满足合并策略：没有要做的任务。
        fn settled(&self) -> bool {
            !self.has_pending_compaction()
        }

        fn dir(&self) -> &Path {
            self.dir.path()
        }

        /// 调用 `close`，设了时限。
        fn close(&self) -> Result<()> {
            let db = Arc::clone(self.db.as_ref().unwrap());
            run_with_deadline("close ", move || db.close())
        }

        /// 释放 `Db`，设了时限。返回有没有按时释放完。
        fn drop_db(&mut self) -> bool {
            let Some(db) = self.db.take() else {
                return true;
            };
            let dropper = thread::spawn(move || drop(db));
            wait_until(|| dropper.is_finished())
        }

        fn get_str(&self, key: &str) -> Option<String> {
            let value = self.get(key.as_bytes()).unwrap();
            value.map(|value| String::from_utf8_lossy(&value).into_owned())
        }

        fn scan_all(&self) -> Vec<(String, String)> {
            let mut iter = self.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
            collect_all(&mut iter)
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let dropped_in_time = self.drop_db();
            // 测试本身已经失败时不再报第二个错。
            if !dropped_in_time && !thread::panicking() {
                panic!("释放 Db 没有按时结束：后台线程很可能没有在收到 Stop 之后结束");
            }
        }
    }

    // 读写都转给引擎：写入、覆盖、删除、点查、扫描的结果与直接用引擎一样。
    #[test]
    fn db_reads_what_was_written() {
        let db = TestDb::open(usize::MAX, SOON);

        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.put(b"c", b"3").unwrap();
        db.put(b"a", b"11").unwrap();
        db.delete(b"b").unwrap();

        assert_eq!(db.get_str("a"), Some("11".to_string()));
        assert_eq!(db.get_str("b"), None);
        assert_eq!(db.scan_all(), expected(&[("a", "11"), ("c", "3")]));
    }

    // 不用任何人手动触发：写满的表被冻结之后，后台线程把它们写成 SST。
    #[test]
    fn db_flushes_in_the_background() {
        // 上限很小，每写几条就冻结一次。
        let db = TestDb::open(64, SOON);

        for i in 0..200 {
            db.put(format!("key-{i:03}").as_bytes(), b"value").unwrap();
        }

        assert!(
            wait_until(|| db.imm_memtable_count() == 0),
            "冻结的表一直没有被 flush 完"
        );
        assert!(db.l0_sstable_count() > 1);
        assert!(!sst_file_names(db.dir()).is_empty());
        for i in 0..200 {
            let key = format!("key-{i:03}");
            assert_eq!(db.get_str(&key), Some("value".to_string()), "{key}");
        }
    }

    // 写入冻结了一张表时会叫醒后台线程。间隔极长，所以只可能是被叫醒的。
    #[test]
    fn db_wakes_the_flush_thread_when_a_memtable_freezes() {
        let db = TestDb::open(64, NEVER);

        for i in 0..50 {
            db.put(format!("key-{i:03}").as_bytes(), b"value").unwrap();
        }
        // 删除也一样会叫醒。
        for i in 0..50 {
            db.delete(format!("key-{i:03}").as_bytes()).unwrap();
        }

        assert!(
            wait_until(|| db.imm_memtable_count() == 0 && db.l0_sstable_count() > 0),
            "表被冻结之后后台线程没有被叫醒"
        );
    }

    // 写批次冻结了一张表时也会叫醒后台线程；批次里的写入与删除都生效。
    #[test]
    fn db_write_batch_wakes_the_flush_thread() {
        let db = TestDb::open(64, NEVER);
        let keys: Vec<String> = (0..20).map(|i| format!("key-{i:03}")).collect();
        let mut batch: Vec<WriteBatchRecord<&[u8]>> = keys
            .iter()
            .map(|key| WriteBatchRecord::Put(key.as_bytes(), &b"value"[..]))
            .collect();
        batch.push(WriteBatchRecord::Delete(keys[0].as_bytes()));

        db.write_batch(&batch).unwrap();

        assert!(
            wait_until(|| db.imm_memtable_count() == 0 && db.l0_sstable_count() > 0),
            "写批次冻结了表之后后台线程没有被叫醒"
        );
        assert_eq!(db.get_str("key-000"), None);
        assert_eq!(db.get_str("key-019"), Some("value".to_string()));
    }

    // 几个线程同时写入，后台线程同时在 flush：每条记录写完立刻读得到，最后一条不少。
    #[test]
    fn db_keeps_data_readable_while_flushing_in_the_background() {
        let db = TestDb::open(256, SOON);
        let writers = 4;
        let puts_per_writer = 500;

        thread::scope(|scope| {
            let db = &db;
            for writer in 0..writers {
                scope.spawn(move || {
                    for i in 0..puts_per_writer {
                        let key = format!("w{writer}-{i:04}");
                        db.put(key.as_bytes(), b"v").unwrap();
                        // 不管它此刻在表里还是已经在文件里，都读得到。
                        assert_eq!(db.get_str(&key), Some("v".to_string()));
                    }
                });
            }
        });

        assert!(wait_until(|| db.imm_memtable_count() == 0));
        assert!(db.l0_sstable_count() > 1);
        assert_eq!(db.scan_all().len(), writers * puts_per_writer);
    }

    // 写入、后台 flush、compaction 同时进行：每条记录写完立刻读得到；
    // 最后再合并一次，L0 清空，数据一条不少，目录里只剩 L1 的文件。
    // 整个测试设了时限：几把锁的先后顺序出错时，测试报"卡住"而不是停在那里。
    #[test]
    fn db_compacts_while_writing_and_flushing() {
        run_with_deadline("写入、flush 与 compaction 同时进行", || {
            let db = TestDb::open(256, SOON);
            let writers = 2;
            let puts_per_writer = 500;

            thread::scope(|scope| {
                let db = &db;
                for writer in 0..writers {
                    scope.spawn(move || {
                        for i in 0..puts_per_writer {
                            let key = format!("w{writer}-{i:04}");
                            db.put(key.as_bytes(), b"v").unwrap();
                            assert_eq!(db.get_str(&key), Some("v".to_string()));
                        }
                    });
                }
                // 写入期间反复合并：每次都可能撞上正在进行的 flush。
                scope.spawn(move || {
                    for _ in 0..20 {
                        db.force_full_compaction().unwrap();
                    }
                });
            });

            assert!(wait_until(|| db.imm_memtable_count() == 0));
            db.force_full_compaction().unwrap();
            assert_eq!(db.l0_sstable_count(), 0);
            let level_counts = db.level_sstable_counts();
            let bottom_count = *level_counts.last().unwrap();
            assert!(bottom_count > 0);
            assert_eq!(db.scan_all().len(), writers * puts_per_writer);
            assert_eq!(sst_file_names(db.dir()).len(), bottom_count);
        });
    }

    // 选项里给了合并参数：不用任何人手动触发，后台把文件一层一层合并下去，形状满足策略；
    // 数据一条不少。
    #[test]
    fn db_compacts_in_the_background() {
        let db = TestDb::open_with_compaction(64, SOON);

        for i in 0..300 {
            db.put(format!("key-{i:03}").as_bytes(), b"value").unwrap();
        }

        assert!(
            wait_until(|| db.imm_memtable_count() == 0 && db.settled()),
            "后台没有合并到停下：L0 {}，各层 {:?}",
            db.l0_sstable_count(),
            db.level_sstable_counts()
        );
        let level_counts = db.level_sstable_counts();
        assert!(
            level_counts.iter().sum::<usize>() > 0,
            "没有任何文件被合并下去"
        );
        assert_eq!(db.scan_all().len(), 300);
    }

    // tiered：后台自动合并，L0 一直是空的，停下时 tier 数不到 num_tiers；数据一条不少。
    #[test]
    fn db_compacts_in_the_background_with_tiered() {
        let db = TestDb::open_with_strategy(64, SOON, tiered_options());

        for i in 0..300 {
            db.put(format!("key-{i:03}").as_bytes(), b"value").unwrap();
        }

        assert!(
            wait_until(|| db.imm_memtable_count() == 0 && db.settled()),
            "后台没有合并到停下：各 tier {:?}",
            db.level_sstable_counts()
        );
        assert_eq!(db.l0_sstable_count(), 0);
        let tier_count = db.level_sstable_counts().len();
        assert!((1..4).contains(&tier_count), "tier 数是 {tier_count}");
        assert!(db.stats().compaction_bytes_written > 0);
        assert_eq!(db.scan_all().len(), 300);
    }

    // 没有开启自动合并时，不管 L0 积了多少文件，都不算有待做的合并。
    #[test]
    fn has_pending_compaction_is_false_without_compaction_options() {
        let db = TestDb::open(usize::MAX, SOON);
        for key in ["a", "b", "c"] {
            db.put(key.as_bytes(), b"v").unwrap();
            db.storage.freeze_memtable().unwrap();
            db.storage.flush_all_imm_memtables().unwrap();
        }

        assert_eq!(db.l0_sstable_count(), 3);
        assert!(!db.has_pending_compaction());
    }

    // 选项里没有合并参数：不起 compaction 线程，L0 的文件只增不减。
    #[test]
    fn db_without_compaction_options_never_compacts() {
        let db = TestDb::open(64, SOON);

        for i in 0..300 {
            db.put(format!("key-{i:03}").as_bytes(), b"value").unwrap();
        }

        assert!(wait_until(|| db.imm_memtable_count() == 0));
        // 再多等几轮 compaction 的间隔，确认没有人在合并。
        thread::sleep(SOON * 10);
        assert!(db.l0_sstable_count() > 2);
        assert_eq!(db.level_sstable_counts(), vec![0, 0, 0]);
    }

    // 几个线程同时写入和读，后台同时在 flush 和合并：每条记录写完立刻读得到；
    // 关闭之后数据一条不少，目录里的文件正好是状态里的文件。
    #[test]
    fn db_keeps_data_readable_while_compacting_in_the_background() {
        keeps_data_readable_while_compacting(compaction_options());
    }

    // 同上，策略换成 tiered：合并期间 flush 不断在最前面加 tier，换状态时要原样留下它们。
    #[test]
    fn db_keeps_data_readable_while_tiered_compacting_in_the_background() {
        keeps_data_readable_while_compacting(tiered_options());
    }

    // 同上，策略换成 leveled：合并期间 flush 不断往 L0 加文件，部分合并只换掉下层的一部分。
    #[test]
    fn db_keeps_data_readable_while_leveled_compacting_in_the_background() {
        keeps_data_readable_while_compacting(leveled_options());
    }

    /// 几个线程同时写入和读，后台按 `compaction` 同时在 flush 和合并，最后关闭并检查；
    /// 再在同一个目录里重新打开，状态与数据都与关闭时相同（flush 与合并交错着往 manifest
    /// 追加记录，重放的结果仍是最后的状态）。
    /// 整个过程设了时限：几把锁的先后顺序出错时，测试报"卡住"而不是停在那里。
    fn keeps_data_readable_while_compacting(compaction: CompactionOptions) {
        run_with_slow_deadline("写入、flush 与后台合并同时进行", move || {
            let mut db = TestDb::open_with_strategy(256, SOON, compaction.clone());
            let writers = 4;
            let puts_per_writer = 500;

            thread::scope(|scope| {
                let db = &db;
                for writer in 0..writers {
                    scope.spawn(move || {
                        for i in 0..puts_per_writer {
                            let key = format!("w{writer}-{i:04}");
                            db.put(key.as_bytes(), b"v").unwrap();
                            assert_eq!(db.get_str(&key), Some("v".to_string()));
                        }
                    });
                }
            });

            db.close().unwrap();
            assert_eq!(db.scan_all().len(), writers * puts_per_writer);
            let snapshot = db.storage.snapshot();
            let mut ids: Vec<usize> = snapshot.l0_sstables.iter().map(|sst| sst.id()).collect();
            for level in &snapshot.levels {
                ids.extend(level.iter().map(|sst| sst.id()));
            }
            ids.sort();
            let names_in_state: Vec<String> = ids.iter().map(|id| format!("{id:05}.sst")).collect();
            assert_eq!(sst_file_names(db.dir()), names_in_state);

            // 重新打开。两个后台线程只在收到信号时醒来，比较之前不会改动状态。
            let ids_before = state_ids(&db.storage);
            let data_before = db.scan_all();
            assert!(db.drop_db());
            let mut options = options_with_limit(256);
            options.compaction = compaction;
            let reopened = Db::open_with_intervals(db.dir(), options, NEVER, NEVER).unwrap();
            db.db = Some(Arc::new(reopened));
            assert_eq!(state_ids(&db.storage), ids_before);
            assert_eq!(db.scan_all(), data_before);
        });
    }

    /// 状态里各处的文件编号：L0（新的在前），以及 L1 往下各层（或各个 tier）。
    fn state_ids(storage: &LsmStorage) -> (Vec<usize>, Vec<Vec<usize>>) {
        let snapshot = storage.snapshot();
        let ids_of =
            |ssts: &[Arc<SsTable>]| -> Vec<usize> { ssts.iter().map(|sst| sst.id()).collect() };
        let levels = snapshot.levels.iter().map(|level| ids_of(level)).collect();
        (ids_of(&snapshot.l0_sstables), levels)
    }

    // 关闭：可写的表里的数据也被写成 SST。直接读文件核对，墓碑也在里面。
    #[test]
    fn close_writes_the_memtable_to_an_sst() {
        let db = TestDb::open(usize::MAX, SOON);
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.delete(b"c").unwrap();

        db.close().unwrap();

        assert_eq!(db.imm_memtable_count(), 0);
        assert_eq!(db.l0_sstable_count(), 1);
        assert_eq!(
            records_in_sst_files(db.dir()),
            expected(&[("a", "1"), ("b", "2"), ("c", "")])
        );
        // 关闭之后数据照常读得到。
        assert_eq!(db.get_str("a"), Some("1".to_string()));
        assert_eq!(db.get_str("c"), None);
    }

    // 关闭：后台线程还没来得及写的冻结表，连同可写的表，全部写成 SST。
    #[test]
    fn close_flushes_frozen_memtables_and_the_memtable() {
        // 间隔极长，冻结时又绕过了 Db、不通知线程：关闭之前后台线程一张都不会写。
        let db = TestDb::open(usize::MAX, NEVER);
        db.put(b"a", b"1").unwrap();
        db.storage.freeze_memtable().unwrap();
        db.put(b"b", b"2").unwrap();
        db.storage.freeze_memtable().unwrap();
        db.put(b"c", b"3").unwrap();
        assert_eq!(db.imm_memtable_count(), 2);

        db.close().unwrap();

        assert_eq!(db.imm_memtable_count(), 0);
        assert_eq!(db.l0_sstable_count(), 3);
        assert_eq!(
            records_in_sst_files(db.dir()),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // 什么都没写就关闭：不留下文件。
    #[test]
    fn close_an_empty_db_writes_no_file() {
        let db = TestDb::open(usize::MAX, SOON);

        db.close().unwrap();

        assert_eq!(db.l0_sstable_count(), 0);
        assert!(sst_file_names(db.dir()).is_empty());
    }

    // 关闭两次：第二次没有事可做，照样成功。
    #[test]
    fn close_twice_is_fine() {
        let db = TestDb::open(usize::MAX, SOON);
        db.put(b"a", b"1").unwrap();

        db.close().unwrap();
        db.close().unwrap();

        assert_eq!(db.l0_sstable_count(), 1);
        assert_eq!(sst_file_names(db.dir()).len(), 1);
    }

    // 关闭之后后台线程已经结束：它手里那份指向引擎的指针没有了，只剩 Db 自己的一份。
    #[test]
    fn close_stops_the_flush_thread() {
        let db = TestDb::open(usize::MAX, SOON);
        assert_eq!(Arc::strong_count(&db.storage), 2);

        db.close().unwrap();

        assert_eq!(
            Arc::strong_count(&db.storage),
            1,
            "close 返回之后后台线程还在"
        );
    }

    // 写文件失败（目录被删掉了）：关闭返回错误，数据还读得到；目录恢复后再关闭一次就成功。
    #[test]
    fn close_reports_flush_error_and_can_be_retried() {
        let db = TestDb::open(usize::MAX, NEVER);
        db.put(b"a", b"1").unwrap();
        std::fs::remove_dir_all(db.dir()).unwrap();

        let result = db.close();

        assert!(matches!(result, Err(crate::error::Error::Io(_))));
        assert_eq!(db.get_str("a"), Some("1".to_string()));

        std::fs::create_dir(db.dir()).unwrap();
        db.close().unwrap();

        assert_eq!(records_in_sst_files(db.dir()), expected(&[("a", "1")]));
    }

    // 有 compaction 线程时，关闭把两个后台线程都停下：只剩 Db 自己的一份指针。
    #[test]
    fn close_stops_the_compaction_thread_too() {
        let db = TestDb::open_with_compaction(usize::MAX, SOON);
        assert_eq!(Arc::strong_count(&db.storage), 3);

        db.close().unwrap();

        assert_eq!(
            Arc::strong_count(&db.storage),
            1,
            "close 返回之后还有后台线程在"
        );
    }

    // 有 compaction 线程时，没有调用 close 就释放：两个后台线程都被停下。
    #[test]
    fn dropping_the_db_stops_the_compaction_thread_too() {
        let mut db = TestDb::open_with_compaction(usize::MAX, NEVER);
        let storage = Arc::clone(&db.storage);
        assert_eq!(Arc::strong_count(&storage), 4);

        assert!(db.drop_db(), "释放 Db 没有按时结束");

        assert_eq!(Arc::strong_count(&storage), 1);
    }

    // 没有调用 close 就释放：后台线程照样被停下，不会留在进程里。
    #[test]
    fn dropping_the_db_stops_the_flush_thread() {
        let mut db = TestDb::open(usize::MAX, NEVER);
        let storage = Arc::clone(&db.storage);
        assert_eq!(Arc::strong_count(&storage), 3);

        assert!(db.drop_db(), "释放 Db 没有按时结束");

        // 只剩测试自己手里的这一份。
        assert_eq!(Arc::strong_count(&storage), 1);
    }

    // 开着 WAL，不 close 就释放（像进程被杀掉）：重新打开后数据都在；重放出的表由后台
    // flush 线程写成 SST，它们的 WAL 随后删掉，只剩新表的 WAL。
    #[test]
    fn data_survives_drop_without_close_when_wal_is_enabled() {
        run_with_deadline("不 close 就重开", || {
            let dir = tempfile::tempdir().unwrap();
            let options = LsmStorageOptions {
                enable_wal: true,
                ..options_with_limit(200)
            };
            let db = Db::open_with_intervals(dir.path(), options.clone(), NEVER, NEVER).unwrap();
            for i in 0..50 {
                db.put(format!("key-{i:03}").as_bytes(), b"value").unwrap();
            }
            db.delete(b"key-007").unwrap();
            drop(db);

            let db = Db::open_with_intervals(dir.path(), options, SOON, NEVER).unwrap();

            for i in 0..50 {
                let expected = if i == 7 {
                    None
                } else {
                    Some(Bytes::from_static(b"value"))
                };
                assert_eq!(db.get(format!("key-{i:03}").as_bytes()).unwrap(), expected);
            }
            assert!(
                wait_until(|| db.imm_memtable_count() == 0),
                "重放出的表没有被 flush"
            );
            let wal_count = std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|entry| {
                    let name = entry.as_ref().unwrap().file_name();
                    name.to_string_lossy().ends_with(".wal")
                })
                .count();
            assert_eq!(wal_count, 1);
            db.close().unwrap();
        });
    }
}
