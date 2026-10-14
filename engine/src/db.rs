//! 对外的句柄：拥有引擎，以及在后台做 flush 的那个线程。
//!
//! `LsmStorage` 只管数据怎么组织，自己不起线程。`Db` 在它外面包一层：
//!
//! - 打开时起一个后台线程，有冻结的 memtable 就把它们写成 SST；
//! - 写入冻结了一张表时给那个线程发 `Wake`，让它立刻开始 flush；
//! - 关闭时让线程停下、等它结束，再把还在内存里的数据都写成 SST。
//!
//! 引擎用 `Arc` 包着，`Db` 和后台线程各拿一份指针。
//!
//! 后台线程只有两种结束的方式：收到停止的信号，或者发信号的那一头没了。
//! 它由 `Db` 启动，也由 `Db` 负责叫停并等它结束。

use std::ops::Bound;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bytes::Bytes;

use crate::error::Result;
use crate::lsm_storage::{LsmStorage, LsmStorageOptions, ScanIterator};
use crate::stats::StatsSnapshot;

/// 后台线程等信号的最长时间。等满这么久还没有信号，它也会检查一次有没有冻结的
/// memtable，有就写成 SST。
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// 发给后台线程的信号。
enum FlushSignal {
    /// 写入冻结了一张 memtable 时发出。
    /// 线程收到后把冻结的 memtable 全部写成 SST，然后继续等下一个信号。
    Wake,
    /// 关闭或释放 `Db` 时发出。
    /// 线程收到后不再做 flush，`flush_loop` 返回，线程结束。
    Stop,
}

pub struct Db {
    /// 引擎本身。后台线程手里有另一份指向它的指针。
    storage: Arc<LsmStorage>,
    /// 通道的发送端，用来给后台线程发信号。接收端在线程手里。
    flush_sender: Sender<FlushSignal>,
    /// 后台线程的把手，等它结束时要用。
    ///
    /// 等待会把把手消耗掉，所以放在 `Option` 里：停线程时把它取走，原处留下 `None`。
    /// 外面套一把锁，是因为取走要修改这个字段，而停线程的方法拿到的只是 `&self`。
    flush_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Db {
    /// 在 `path` 这个目录里打开数据库，并启动后台的 flush 线程。
    pub fn open(path: &Path, options: LsmStorageOptions) -> Result<Self> {
        Self::open_with_flush_interval(path, options, FLUSH_INTERVAL)
    }

    /// 与 `open` 相同，只是后台线程自己醒来的间隔由调用方给出。测试用它。
    fn open_with_flush_interval(
        path: &Path,
        options: LsmStorageOptions,
        flush_interval: Duration,
    ) -> Result<Self> {
        let storage = Arc::new(LsmStorage::open(path, options)?);
        let (flush_sender, flush_receiver) = mpsc::channel();

        // 线程什么时候结束不由这个函数决定，所以它不能借用这里的任何东西，
        // 要用的都得整个交给它：一份指向引擎的指针，和通道的接收端。
        // `move` 把这两个变量搬进闭包，之后这个函数里就不能再用它们了。
        let storage_for_thread = Arc::clone(&storage);
        let flush_thread = thread::Builder::new()
            // 给线程起名字：panic 的信息和调试器里都会显示它。
            .name("lsm-flush".to_string())
            .spawn(move || flush_loop(storage_for_thread, flush_receiver, flush_interval))?;

        Ok(Self {
            storage,
            flush_sender,
            flush_thread: Mutex::new(Some(flush_thread)),
        })
    }

    /// 写入一条记录。key 与 value 都不能为空。
    pub fn put(&self, key: &[u8], value: &[u8]) {
        let froze = self.storage.put(key, value);
        if froze {
            self.wake_flush_thread();
        }
    }

    /// 删除一个 key。
    pub fn delete(&self, key: &[u8]) {
        let froze = self.storage.delete(key);
        if froze {
            self.wake_flush_thread();
        }
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
    /// 目前没有 WAL，memtable 里的数据只在内存里。正常退出前必须调用这个方法，
    /// 否则最后写入的那部分数据就丢了。
    ///
    /// 不管成功还是失败，返回时后台线程都已经结束。写文件失败时返回错误，
    /// 没写成的数据仍然留在内存里、照常读得到；可以再调用一次重试。
    /// 调用成功之后不应再写入。
    pub fn close(&self) -> Result<()> {
        // 先让后台线程停下：从这里往后，做 flush 的只有当前这个线程。
        self.stop_flush_thread();
        // 可写的那张表里也有数据，冻结之后才能 flush。
        self.storage.freeze_memtable();
        self.storage.flush_all_imm_memtables()
    }

    /// 给后台线程发 `Wake`：它收到后把冻结的 memtable 全部写成 SST。
    fn wake_flush_thread(&self) {
        // 发送只在接收端已经不存在时失败，也就是后台线程已经结束了。
        // 那时没有人可叫，什么都不用做。
        let _ = self.flush_sender.send(FlushSignal::Wake);
    }

    /// 让后台线程停下，并等它结束。线程已经停过了就什么都不做。
    fn stop_flush_thread(&self) {
        // 把把手取走。这一句结束时锁就放开了：下面等线程结束可能要等很久，
        // 不能拿着锁等。
        let flush_thread = self.flush_thread.lock().unwrap().take();
        let Some(flush_thread) = flush_thread else {
            return;
        };

        // 发送失败说明线程已经自己结束了，照样往下等它。
        let _ = self.flush_sender.send(FlushSignal::Stop);
        // 线程正在写文件的话，会先把手上的写完再看信号，所以这里会等到它写完。
        if flush_thread.join().is_err() {
            eprintln!("后台 flush 线程是 panic 结束的");
        }
    }
}

impl Drop for Db {
    /// `Db` 被释放时停下后台线程，不让它留在进程里继续跑。
    ///
    /// 这里不做 flush：flush 会失败，而 `drop` 没有办法把错误交给调用方。
    /// 要保住数据，得在释放之前调用 `close`。
    fn drop(&mut self) {
        self.stop_flush_thread();
    }
}

/// 后台线程执行的函数：等信号，有冻结的 memtable 就把它们都写成 SST，如此反复。
///
/// 每一轮最多等 `interval` 这么久。等到的结果有四种：
///
/// - 收到 `Wake`，或者等到超时：把冻结的表都 flush 掉，然后进入下一轮；
/// - 收到 `Stop`，或者发送端已经不存在：函数返回，线程随之结束。
///
/// flush 失败时线程不能跟着结束：把错误打印出来，下一轮再试。
/// 这一轮没写成的表还留在冻结列表里，数据没有丢。
fn flush_loop(storage: Arc<LsmStorage>, receiver: Receiver<FlushSignal>, interval: Duration) {
    loop {
        // 最多等 `interval` 这么久。这里的 `Ok` 只表示收到了一条信号，`Err` 只表示没收到，
        // 所以按"接下来做什么"分成两组，而不是按 `Ok` 和 `Err` 分。
        match receiver.recv_timeout(interval) {
            Ok(FlushSignal::Wake) | Err(RecvTimeoutError::Timeout) => {}
            Ok(FlushSignal::Stop) | Err(RecvTimeoutError::Disconnected) => return,
        }

        // 失败时线程不结束：没写成的表还在冻结列表里，下一轮再试。
        if let Err(error) = storage.flush_all_imm_memtables() {
            eprintln!("后台 flush 失败，下一轮重试：{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use crate::table::{FileObject, SsTable, SsTableIterator};
    use crate::test_util::{run_with_deadline, wait_until};

    /// 长到测试期间不会到点：用它时，后台线程只可能是被信号叫醒的。
    const NEVER: Duration = Duration::from_secs(3600);
    /// 短到很快就会到点：用它时，不发信号后台线程也会自己醒来。
    const SOON: Duration = Duration::from_millis(10);

    fn options_with_limit(memtable_size_limit: usize) -> LsmStorageOptions {
        LsmStorageOptions {
            memtable_size_limit,
            block_size: 64,
            block_cache_size: 1 << 20,
        }
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
        sender: Option<Sender<FlushSignal>>,
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

        fn send(&self, signal: FlushSignal) {
            self.sender.as_ref().unwrap().send(signal).unwrap();
        }

        /// 写入一条记录然后冻结，不通知线程。
        fn put_and_freeze(&self, key: &str, value: &str) {
            self.storage.put(key.as_bytes(), value.as_bytes());
            self.storage.freeze_memtable();
        }
    }

    impl Drop for RunningLoop {
        /// 测试结束时让线程停下。只发信号、不等它：线程要是不听停止的信号，
        /// 等它会让每个测试都卡住，那个错误由专门的测试去报。
        fn drop(&mut self) {
            if let Some(sender) = &self.sender {
                let _ = sender.send(FlushSignal::Stop);
            }
        }
    }

    // 收到 Wake：把冻结的表写成 SST。间隔极长，所以只可能是信号起的作用。
    #[test]
    fn loop_flushes_when_woken() {
        let running = RunningLoop::start(NEVER);
        running.put_and_freeze("a", "1");

        running.send(FlushSignal::Wake);

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

        running.send(FlushSignal::Wake);

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

        running.send(FlushSignal::Stop);

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
            let db = Db::open_with_flush_interval(dir.path(), options, flush_interval).unwrap();
            Self {
                db: Some(Arc::new(db)),
                dir,
            }
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

        db.put(b"a", b"1");
        db.put(b"b", b"2");
        db.put(b"c", b"3");
        db.put(b"a", b"11");
        db.delete(b"b");

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
            db.put(format!("key-{i:03}").as_bytes(), b"value");
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
            db.put(format!("key-{i:03}").as_bytes(), b"value");
        }
        // 删除也一样会叫醒。
        for i in 0..50 {
            db.delete(format!("key-{i:03}").as_bytes());
        }

        assert!(
            wait_until(|| db.imm_memtable_count() == 0 && db.l0_sstable_count() > 0),
            "表被冻结之后后台线程没有被叫醒"
        );
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
                        db.put(key.as_bytes(), b"v");
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

    // 关闭：可写的表里的数据也被写成 SST。直接读文件核对，墓碑也在里面。
    #[test]
    fn close_writes_the_memtable_to_an_sst() {
        let db = TestDb::open(usize::MAX, SOON);
        db.put(b"a", b"1");
        db.put(b"b", b"2");
        db.delete(b"c");

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
        db.put(b"a", b"1");
        db.storage.freeze_memtable();
        db.put(b"b", b"2");
        db.storage.freeze_memtable();
        db.put(b"c", b"3");
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
        db.put(b"a", b"1");

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
        db.put(b"a", b"1");
        std::fs::remove_dir_all(db.dir()).unwrap();

        let result = db.close();

        assert!(matches!(result, Err(crate::error::Error::Io(_))));
        assert_eq!(db.get_str("a"), Some("1".to_string()));

        std::fs::create_dir(db.dir()).unwrap();
        db.close().unwrap();

        assert_eq!(records_in_sst_files(db.dir()), expected(&[("a", "1")]));
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
}
