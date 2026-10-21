//! 真实进程崩溃：子进程开着 WAL 不停地写，每完成一次写入就把序号打印出来；父进程读到
//! 足够多的序号之后用 SIGKILL 杀掉它，不给它任何清理的机会（`close`、`Drop` 都不会执行），
//! 然后自己打开同一个目录检查。
//!
//! 子进程就是这个测试程序自己：父进程用 `--exact crash_child` 重新启动当前的可执行文件，
//! 并用环境变量告诉它数据库在哪里。没有这个环境变量时，`crash_child` 什么都不做。
//!
//! 进程崩溃时，已经 `write` 进页缓存的数据不会丢（断电才会），所以不管写入同不同步，
//! 每一次完成了的写入都应当还在。被杀掉时正在进行的那次写入可能只写了一半，打开时被截掉。
//! 子进程打印的序号落后于它实际完成的写入（父进程读到第 N 个时，它可能已经又写了几个），
//! 所以检查的是：打开后的数据恰好是"前 M 次写入"的结果，M 不小于父进程读到的最后一个序号。
//! 也就是说，完成了的写入一条不少，而且没有"后面的在、前面的没了"的空洞。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use lsm::compaction::CompactionOptions;
use lsm::compaction::simple_leveled::SimpleLeveledOptions;
use lsm::db::Db;
use lsm::lsm_storage::{LsmStorageOptions, WriteOptions};

/// 告诉子进程数据库所在的目录。
const DIR_ENV: &str = "LSM_CRASH_CHILD_DIR";
/// 告诉子进程每次写入要不要同步：`1` 是要。
const SYNC_ENV: &str = "LSM_CRASH_CHILD_SYNC";
/// 告诉子进程从第几次写入开始：接着上一个子进程往后写。
const START_ENV: &str = "LSM_CRASH_CHILD_START";
/// 写入轮流落在这么多个 key 上：同一个 key 被反复覆盖，检查时要读到最后一次。
const KEYS: u64 = 300;
/// 等子进程打印下一个序号的最长时间。超过了说明它卡住或者没起来，测试失败。
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);

/// memtable 很小、后台按简单分层合并：几千次写入里有上百次冻结、flush 与合并，
/// 被杀掉的那一刻多半有某个后台操作正在进行。
fn options() -> LsmStorageOptions {
    LsmStorageOptions {
        memtable_size_limit: 2048,
        block_size: 256,
        block_cache_size: 1 << 20,
        target_sst_size: 2048,
        max_levels: 3,
        compaction: CompactionOptions::SimpleLeveled(SimpleLeveledOptions {
            level0_file_num_compaction_trigger: 2,
            size_ratio_percent: 200,
        }),
        enable_wal: true,
    }
}

/// 第 `i` 次写入的 key。
fn key_of(i: u64) -> String {
    format!("key-{:03}", i % KEYS)
}

/// 第 `i` 次写入的 value：序号写在里面，读回来就知道是哪一次写入留下的。每 7 次里有一次删除。
fn value_of(i: u64) -> Option<String> {
    if i % 7 == 3 {
        None
    } else {
        Some(format!("value-{i:08}"))
    }
}

/// 子进程：只有父进程设了 `DIR_ENV` 时才做事。打开数据库，从 `START_ENV` 开始一直写下去，
/// 每完成一次就打印它的序号，直到被杀掉。
#[test]
fn crash_child() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let sync = std::env::var(SYNC_ENV).is_ok_and(|value| value == "1");
    let start: u64 = std::env::var(START_ENV).unwrap().parse().unwrap();
    let db = Db::open(Path::new(&dir), options()).unwrap();
    let write_options = WriteOptions { sync };
    let mut stdout = std::io::stdout().lock();
    // 一直写到被杀掉为止。
    let mut i = start;
    loop {
        let key = key_of(i);
        match value_of(i) {
            Some(value) => db.put_opt(key.as_bytes(), value.as_bytes(), write_options),
            None => db.delete_opt(key.as_bytes(), write_options),
        }
        .unwrap();
        // 写入已经返回：这一次算完成了。
        writeln!(stdout, "acked {i}").unwrap();
        stdout.flush().unwrap();
        i += 1;
    }
}

/// 启动一个子进程从第 `start` 次写入开始写，等它报告完成第 `kill_after` 次之后用 SIGKILL
/// 杀掉它。返回父进程看到的最后一个完成的序号。
fn run_child_until_killed(dir: &Path, sync: bool, start: u64, kill_after: u64) -> u64 {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
        .env(DIR_ENV, dir)
        .env(SYNC_ENV, if sync { "1" } else { "0" })
        .env(START_ENV, start.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // 读输出放在另一个线程里，主线程等它时可以设时限。测试框架自己也往标准输出打印几行，
    // 只认 "acked 序号" 这种。
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let Some(number) = line.strip_prefix("acked ") else {
                continue;
            };
            // 主线程已经不等了（杀掉子进程之后），不用再读。
            if sender.send(number.parse::<u64>().unwrap()).is_err() {
                return;
            }
        }
    });

    let mut last_acked = None;
    loop {
        match receiver.recv_timeout(CHILD_TIMEOUT) {
            Ok(i) => {
                last_acked = Some(i);
                if i >= kill_after {
                    break;
                }
            }
            Err(error) => {
                kill(&mut child);
                panic!("子进程没有按时报告进度（最后完成的是 {last_acked:?}）：{error}");
            }
        }
    }
    kill(&mut child);
    last_acked.unwrap()
}

/// SIGKILL，并等它真的结束。
fn kill(child: &mut Child) {
    // Unix 上 `kill` 发的是 SIGKILL：进程立刻结束，不执行任何清理。
    child.kill().unwrap();
    child.wait().unwrap();
}

/// 打开数据库，检查读到的恰好是前 M 次写入的结果，返回 M；M 至少是 `at_least`。
fn check_prefix(dir: &Path, at_least: u64) -> u64 {
    let db = Db::open(dir, options()).unwrap();
    // 每个 key 读到的值记着是哪一次写入留下的；被删掉的 key 读不到。
    let mut newest_seen = 0;
    let mut seen = Vec::new();
    for k in 0..KEYS {
        let value = db.get(key_of(k).as_bytes()).unwrap();
        let index = value.map(|value| {
            let text = String::from_utf8(value.to_vec()).unwrap();
            text.strip_prefix("value-").unwrap().parse::<u64>().unwrap()
        });
        if let Some(index) = index {
            newest_seen = newest_seen.max(index);
        }
        seen.push(index);
    }
    // M 是"最后完成的一次写入"。读到的最大序号不一定是它（最后几次可能是删除），
    // 所以从两者中较大的开始，往后试几个，找一个与全部 key 都对得上的。
    let lowest = at_least.max(newest_seen);
    for m in lowest..lowest + 2 * KEYS {
        if (0..KEYS).all(|k| seen[k as usize] == expected_after(m, k)) {
            drop(db);
            return m;
        }
    }
    panic!(
        "读到的数据不是任何一个前缀的结果：完成了的写入丢了，或者出现了空洞（至少应到第 {at_least} 次）"
    );
}

/// 做完第 0 次到第 `m` 次写入之后，第 `k` 个 key 的值是哪一次写入留下的；被删了或者没写过是 `None`。
fn expected_after(m: u64, k: u64) -> Option<u64> {
    if m < k {
        return None;
    }
    // 不超过 m、落在第 k 个 key 上的最后一次写入。
    let last = m - (m - k) % KEYS;
    value_of(last).map(|_| last)
}

// 写入不同步，进程被 SIGKILL：完成了的写入一条不少，没有空洞。
#[test]
fn completed_writes_survive_a_process_kill() {
    let dir = tempfile::tempdir().unwrap();

    let acked = run_child_until_killed(dir.path(), false, 0, 3000);

    let recovered = check_prefix(dir.path(), acked);
    assert!(recovered >= acked);
}

// 每次写入都同步：同样一条不少。
#[test]
fn synced_writes_survive_a_process_kill() {
    let dir = tempfile::tempdir().unwrap();

    let acked = run_child_until_killed(dir.path(), true, 0, 60);

    check_prefix(dir.path(), acked);
}

// 连续崩溃三次，每次接着上一次恢复出的数据往后写：恢复出的数据库再崩溃，照样能恢复。
#[test]
fn repeated_kills_keep_every_completed_write() {
    let dir = tempfile::tempdir().unwrap();
    let mut next = 0;
    for round in 0..3 {
        let acked = run_child_until_killed(dir.path(), false, next, next + 1500);
        let recovered = check_prefix(dir.path(), acked);
        assert!(recovered >= acked, "第 {round} 轮");
        next = recovered + 1;
    }
}
