//! 多线程测试共用的工具：等一个条件成立、给一段代码设时限。
//!
//! 多线程的测试有两条规矩：
//!
//! - 不用"睡一会儿"来等另一个线程做完事。睡多久都只是猜：机器忙的时候不够，
//!   机器闲的时候白等。改成反复检查那件事做完了没有，设一个最长等待时间。
//! - 测试不能卡死。被测的代码有错时线程可能永远不结束，直接等它，测试就停在那里，
//!   连报错都看不到。所以凡是可能等不到的地方都设时限，超时就让测试失败。

use std::thread;
use std::time::{Duration, Instant};

/// 最长等待时间。正常情况下几毫秒就够，只有出错时才会等满。
pub const DEADLINE: Duration = Duration::from_secs(5);

/// 反复检查 `condition`，直到它成立或者超过最长等待时间。返回它最后有没有成立。
pub fn wait_until(condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    condition()
}

/// 在另一个线程里执行 `work`，返回它的结果；超过最长等待时间还没结束，测试失败。
///
/// 超时之后那个线程还卡在那里，没有办法从外面停掉它；测试进程结束时它随进程一起消失。
/// `what` 写进失败信息里，说明卡住的是哪一步。
pub fn run_with_deadline<T: Send + 'static>(
    what: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let handle = thread::spawn(work);
    let finished = wait_until(|| handle.is_finished());
    assert!(
        finished,
        "{what}没有在 {} 秒内结束，很可能卡住了",
        DEADLINE.as_secs()
    );
    match handle.join() {
        Ok(result) => result,
        // 那个线程是 panic 结束的：把它的 panic 原样在这里重新抛出。
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
