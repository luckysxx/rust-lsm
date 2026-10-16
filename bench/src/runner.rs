//! 执行一种负载：把操作分给若干线程，逐个操作计时，记录延迟分布、总耗时与计数器的增量。
//! 计时结束后等被测对象的后台工作做完（引擎把冻结的表写成文件），再读计数器，
//! 这样每段负载的计数器只包含它自己引起的工作，下一段负载也从相同的状态开始。

use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use lsm::stats::StatsSnapshot;

use crate::target::{KvTarget, SstLayout};
use crate::workload::{
    KEY_SIZE, Rng, Workload, existing_key, make_value, missing_key, shuffled_indexes,
};

/// 相邻两个线程的种子相差这么多，各线程的随机序列互不相同。
const THREAD_SEED_STEP: u64 = 1000;

/// 一次运行的参数。记进报告，重跑时照抄。
#[derive(Debug, Clone)]
pub struct Config {
    /// key 的个数，也是每种负载的操作次数（所有线程合计）。
    pub num: u64,
    pub value_size: usize,
    pub seed: u64,
    /// 同时发起操作的线程数。
    pub threads: usize,
    /// 范围扫描负载每次读多少条记录。
    pub scan_length: usize,
}

impl Config {
    /// 一种负载一共执行多少次操作。
    ///
    /// 范围扫描的一次操作要读 `scan_length` 条记录，次数相应减少，
    /// 使读到的记录总数与其他负载的操作数相当。
    pub fn total_ops(&self, workload: Workload) -> u64 {
        match workload {
            Workload::SeekRandom => (self.num / self.scan_length as u64).max(1),
            Workload::Compact => 1,
            _ => self.num,
        }
    }
}

/// 一种负载跑完后的结果。
pub struct WorkloadResult {
    pub workload: Workload,
    pub ops: u64,
    /// 从所有线程一起开始，到最后一个线程结束。不含生成操作序列的时间。
    pub elapsed: Duration,
    /// 计时结束之后，等后台工作做完又花了多久。不计入吞吐。
    pub drain: Duration,
    /// 每次操作的耗时，单位纳秒。所有线程的记录合在一起。
    pub latency_nanos: Histogram<u64>,
    /// 点查负载中查到值的次数；范围扫描中读到的记录总数；写负载为 0。
    pub found: u64,
    /// 写负载中交给被测对象的字节（key 加 value）；读负载为 0。
    pub user_bytes: u64,
    /// 这段负载期间引擎计数器的增量；被测对象没有计数器时为 `None`。
    pub stats: Option<StatsSnapshot>,
    /// 这段负载结束、后台工作做完之后，磁盘上的 SST；不写磁盘的被测对象为 `None`。
    pub sst_layout: Option<SstLayout>,
}

impl WorkloadResult {
    pub fn ops_per_second(&self) -> f64 {
        self.ops as f64 / self.elapsed.as_secs_f64()
    }
}

/// 一个线程要做的事，开始计时之前就准备好。
struct ThreadPlan {
    /// 这个线程的操作次数。
    ops: u64,
    /// 这个线程自己的随机序列。
    rng: Rng,
    /// 只用于 fillseq 与 fillrandom：这个线程要依次写入的 key 编号。
    /// fillseq 是连续的一段，fillrandom 是打乱过的。
    fill_order: Vec<u64>,
}

/// 一个线程跑完后的结果。
struct ThreadResult {
    latency_nanos: Histogram<u64>,
    found: u64,
    user_bytes: u64,
}

/// 把一种负载的操作分给各个线程。
fn plan_threads(workload: Workload, config: &Config) -> Vec<ThreadPlan> {
    let base_seed = config.seed.wrapping_add(workload.seed_offset());
    // compaction 只做一次，不分给多个线程。
    let threads = if workload == Workload::Compact {
        1
    } else {
        config.threads as u64
    };
    let total_ops = config.total_ops(workload);

    // 两种 fill 都要保证每个 key 恰好写一次：先排好全部编号，再切给各线程。
    // fillseq 按顺序，fillrandom 整体打乱。
    let is_fill = matches!(workload, Workload::FillSeq | Workload::FillRandom);
    let mut all_fill_order = Vec::new();
    if workload == Workload::FillSeq {
        all_fill_order = (0..config.num).collect();
    }
    if workload == Workload::FillRandom {
        let mut rng = Rng::new(base_seed);
        all_fill_order = shuffled_indexes(config.num, &mut rng);
    }

    let mut plans = Vec::new();
    let mut assigned = 0u64;
    for thread_index in 0..threads {
        // 除不尽时，前面几个线程各多做一次。
        let mut ops = total_ops / threads;
        if thread_index < total_ops % threads {
            ops += 1;
        }

        let mut fill_order = Vec::new();
        if is_fill {
            let start = assigned as usize;
            let end = (assigned + ops) as usize;
            fill_order = all_fill_order[start..end].to_vec();
        }
        assigned += ops;

        let thread_seed = base_seed.wrapping_add(thread_index * THREAD_SEED_STEP);
        plans.push(ThreadPlan {
            ops,
            rng: Rng::new(thread_seed),
            fill_order,
        });
    }
    plans
}

pub fn run_workload(target: &dyn KvTarget, workload: Workload, config: &Config) -> WorkloadResult {
    let value = make_value(config.value_size, config.seed);
    let plans = plan_threads(workload, config);
    let key_count = config.num;
    let scan_length = config.scan_length;

    // 起跑线：各工作线程加上发令的主线程，到齐后一起放行。
    let start_line = Barrier::new(plans.len() + 1);

    let stats_before = target.stats();

    let (thread_results, elapsed) = thread::scope(|scope| {
        let mut handles = Vec::new();
        for plan in plans {
            let value = &value;
            let start_line = &start_line;
            let handle = scope.spawn(move || {
                run_thread(
                    target,
                    workload,
                    plan,
                    key_count,
                    scan_length,
                    value,
                    start_line,
                )
            });
            handles.push(handle);
        }

        start_line.wait();
        let started = Instant::now();

        let mut thread_results = Vec::new();
        for handle in handles {
            thread_results.push(handle.join().unwrap());
        }
        (thread_results, started.elapsed())
    });

    // 计时已经结束：等后台工作做完再读计数器。
    let drain_started = Instant::now();
    target.wait_for_background();
    let drain = drain_started.elapsed();

    let stats_after = target.stats();
    let sst_layout = target.sst_layout();
    let stats = match (stats_before, stats_after) {
        (Some(before), Some(after)) => Some(after.since(&before)),
        _ => None,
    };

    let mut latency_nanos = new_histogram();
    let mut found = 0u64;
    let mut user_bytes = 0u64;
    for thread_result in &thread_results {
        latency_nanos
            .add(&thread_result.latency_nanos)
            .expect("两个直方图的精度相同，合并不应失败");
        found += thread_result.found;
        user_bytes += thread_result.user_bytes;
    }

    WorkloadResult {
        workload,
        ops: config.total_ops(workload),
        elapsed,
        drain,
        latency_nanos,
        found,
        user_bytes,
        stats,
        sst_layout,
    }
}

/// 一个线程的全部工作：在起跑线等齐，然后逐个执行自己的操作并计时。
fn run_thread(
    target: &dyn KvTarget,
    workload: Workload,
    plan: ThreadPlan,
    key_count: u64,
    scan_length: usize,
    value: &[u8],
    start_line: &Barrier,
) -> ThreadResult {
    let mut rng = plan.rng;
    let mut latency_nanos = new_histogram();
    let mut found = 0u64;
    let mut user_bytes = 0u64;

    start_line.wait();

    match workload {
        Workload::FillSeq | Workload::FillRandom => {
            for index in plan.fill_order {
                let key = existing_key(index);
                let op_started = Instant::now();
                target.put(&key, value);
                record(&mut latency_nanos, op_started.elapsed());
                user_bytes += (KEY_SIZE + value.len()) as u64;
            }
        }
        Workload::Overwrite => {
            for _ in 0..plan.ops {
                let index = rng.below(key_count);
                let key = existing_key(index);
                let op_started = Instant::now();
                target.put(&key, value);
                record(&mut latency_nanos, op_started.elapsed());
                user_bytes += (KEY_SIZE + value.len()) as u64;
            }
        }
        Workload::ReadRandom => {
            for _ in 0..plan.ops {
                let index = rng.below(key_count);
                let key = existing_key(index);
                let op_started = Instant::now();
                let result = target.get(&key);
                record(&mut latency_nanos, op_started.elapsed());
                if result.is_some() {
                    found += 1;
                }
            }
        }
        Workload::ReadMissing => {
            for _ in 0..plan.ops {
                let index = rng.below(key_count);
                let key = missing_key(index);
                let op_started = Instant::now();
                let result = target.get(&key);
                record(&mut latency_nanos, op_started.elapsed());
                if result.is_some() {
                    found += 1;
                }
            }
        }
        Workload::SeekRandom => {
            for _ in 0..plan.ops {
                let index = rng.below(key_count);
                let start_key = existing_key(index);
                let op_started = Instant::now();
                let records_read = target.scan(&start_key, scan_length);
                record(&mut latency_nanos, op_started.elapsed());
                found += records_read as u64;
            }
        }
        Workload::Compact => {
            let op_started = Instant::now();
            target.compact();
            record(&mut latency_nanos, op_started.elapsed());
        }
    }

    ThreadResult {
        latency_nanos,
        found,
        user_bytes,
    }
}

/// 三位有效数字：记录值与真实值相差不超过千分之一。
fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new(3).unwrap()
}

fn record(latency_nanos: &mut Histogram<u64>, elapsed: Duration) {
    let nanos = elapsed.as_nanos() as u64;
    latency_nanos
        .record(nanos)
        .expect("延迟直方图会自动扩大范围，记录不应失败");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::BTreeMapTarget;

    fn small_config() -> Config {
        Config {
            num: 1000,
            value_size: 100,
            seed: 1,
            threads: 1,
            scan_length: 10,
        }
    }

    fn threaded_config(threads: usize) -> Config {
        Config {
            threads,
            ..small_config()
        }
    }

    // fillrandom 把每个存在的 key 都写了一次，没有写出不存在的 key。
    #[test]
    fn fillrandom_writes_every_key_once() {
        let target = BTreeMapTarget::new();
        let config = small_config();

        let result = run_workload(&target, Workload::FillRandom, &config);

        assert_eq!(result.ops, 1000);
        assert_eq!(result.latency_nanos.len(), 1000);
        assert_eq!(result.user_bytes, 1000 * (16 + 100));
        for index in 0..config.num {
            assert!(target.get(&existing_key(index)).is_some());
            assert!(target.get(&missing_key(index)).is_none());
        }
    }

    // readrandom 每次都查到，readmissing 一次也查不到。
    #[test]
    fn readrandom_finds_all_and_readmissing_finds_none() {
        let target = BTreeMapTarget::new();
        let config = small_config();
        run_workload(&target, Workload::FillRandom, &config);

        let hits = run_workload(&target, Workload::ReadRandom, &config);
        let misses = run_workload(&target, Workload::ReadMissing, &config);

        assert_eq!(hits.found, 1000);
        assert_eq!(misses.found, 0);
        assert_eq!(hits.user_bytes, 0);
    }

    // overwrite 只覆盖已有的 key，不产生新的 key。
    #[test]
    fn overwrite_does_not_create_new_keys() {
        let target = BTreeMapTarget::new();
        let config = small_config();
        run_workload(&target, Workload::FillRandom, &config);

        let result = run_workload(&target, Workload::Overwrite, &config);

        assert_eq!(result.user_bytes, 1000 * (16 + 100));
        for index in 0..config.num {
            assert!(target.get(&missing_key(index)).is_none());
        }
        // 2000 以上的编号从未写过。
        assert!(target.get(&existing_key(config.num)).is_none());
    }

    // compact 只执行一次，线程数设多少都只用一个线程。
    #[test]
    fn compact_runs_once_on_one_thread() {
        let plans = plan_threads(Workload::Compact, &threaded_config(4));

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].ops, 1);
        assert_eq!(threaded_config(4).total_ops(Workload::Compact), 1);
    }

    // 对引擎执行 compact：L0 清空，数据进了 L1，读到的不变；计数器记下了 compaction 写的字节。
    #[test]
    fn compact_moves_engine_data_into_l1() {
        use crate::target::EngineTarget;

        // memtable 上限 16 KB，1000 条记录写出好几个 L0 文件。
        let target = EngineTarget::new(16 * 1024, 1 << 20);
        let config = small_config();
        run_workload(&target, Workload::FillRandom, &config);
        let before = target.sst_layout().unwrap();
        assert!(before.l0_files > 1);

        let result = run_workload(&target, Workload::Compact, &config);

        let after = result.sst_layout.unwrap();
        assert_eq!(result.ops, 1);
        assert_eq!(after.l0_files, 0);
        assert!(after.l1_files > 0);
        assert!(result.stats.unwrap().compaction_bytes_written > 0);
        let hits = run_workload(&target, Workload::ReadRandom, &config);
        assert_eq!(hits.found, config.num);
    }

    // 被测对象没有计数器时，结果里的放大数据为空。
    #[test]
    fn target_without_counters_reports_no_stats() {
        let target = BTreeMapTarget::new();
        let result = run_workload(&target, Workload::FillRandom, &small_config());

        assert!(result.stats.is_none());
        assert!(result.sst_layout.is_none());
    }

    // 操作数除不尽线程数时，各线程的份额相差不超过 1，合计等于总数。
    #[test]
    fn plan_splits_ops_across_threads() {
        let config = Config {
            num: 10,
            ..threaded_config(3)
        };

        let plans = plan_threads(Workload::ReadRandom, &config);

        let ops: Vec<u64> = plans.iter().map(|plan| plan.ops).collect();
        assert_eq!(ops, vec![4, 3, 3]);
    }

    // fillrandom 分给各线程的 key 编号合起来，每个编号恰好出现一次。
    #[test]
    fn plan_gives_every_fill_key_to_exactly_one_thread() {
        let plans = plan_threads(Workload::FillRandom, &threaded_config(7));

        let mut all_indexes = Vec::new();
        for plan in &plans {
            assert_eq!(plan.fill_order.len() as u64, plan.ops);
            all_indexes.extend_from_slice(&plan.fill_order);
        }
        all_indexes.sort_unstable();

        let expected: Vec<u64> = (0..1000).collect();
        assert_eq!(all_indexes, expected);
    }

    // fillseq 按编号从小到大写；分给多个线程时，每个线程是连续的一段。
    #[test]
    fn plan_gives_fillseq_keys_in_order() {
        let plans = plan_threads(Workload::FillSeq, &threaded_config(3));

        assert_eq!(plans[0].fill_order, (0..334).collect::<Vec<u64>>());
        assert_eq!(plans[1].fill_order, (334..667).collect::<Vec<u64>>());
        assert_eq!(plans[2].fill_order, (667..1000).collect::<Vec<u64>>());
    }

    // fillseq 写入的 key 与 fillrandom 相同，只是顺序不同。
    #[test]
    fn fillseq_writes_every_key_once() {
        let target = BTreeMapTarget::new();
        let config = small_config();

        let result = run_workload(&target, Workload::FillSeq, &config);

        assert_eq!(result.user_bytes, 1000 * (16 + 100));
        for index in 0..config.num {
            assert!(target.get(&existing_key(index)).is_some());
            assert!(target.get(&missing_key(index)).is_none());
        }
    }

    // 多线程下 fillrandom 仍然把每个 key 写一次，每次操作都记了延迟。
    #[test]
    fn threaded_fillrandom_writes_every_key_once() {
        let target = BTreeMapTarget::new();
        let config = threaded_config(4);

        let result = run_workload(&target, Workload::FillRandom, &config);

        assert_eq!(result.latency_nanos.len(), 1000);
        assert_eq!(result.user_bytes, 1000 * (16 + 100));
        for index in 0..config.num {
            assert!(target.get(&existing_key(index)).is_some());
        }
    }

    // 多线程下读负载的命中次数与单线程时的规律相同：存在的全中，不存在的全不中。
    #[test]
    fn threaded_reads_count_hits_from_all_threads() {
        let target = BTreeMapTarget::new();
        let config = threaded_config(4);
        run_workload(&target, Workload::FillRandom, &config);

        let hits = run_workload(&target, Workload::ReadRandom, &config);
        let misses = run_workload(&target, Workload::ReadMissing, &config);

        assert_eq!(hits.found, 1000);
        assert_eq!(hits.latency_nanos.len(), 1000);
        assert_eq!(misses.found, 0);
    }

    // 范围扫描的次数是 key 的个数除以每次读的条数，至少一次。
    #[test]
    fn seekrandom_runs_fewer_but_longer_operations() {
        let config = small_config();
        let tiny = Config {
            num: 3,
            ..small_config()
        };

        assert_eq!(config.total_ops(Workload::SeekRandom), 100);
        assert_eq!(config.total_ops(Workload::ReadRandom), 1000);
        assert_eq!(tiny.total_ops(Workload::SeekRandom), 1);
    }

    // 范围扫描：每次从一个已有的 key 起读若干条；读到的记录数不超过"次数乘以每次的条数"，
    // 只有起点靠近末尾的那几次会读不满。
    #[test]
    fn seekrandom_reads_records_in_runs() {
        let target = BTreeMapTarget::new();
        let config = small_config();
        run_workload(&target, Workload::FillRandom, &config);

        let result = run_workload(&target, Workload::SeekRandom, &config);

        assert_eq!(result.ops, 100);
        assert_eq!(result.latency_nanos.len(), 100);
        assert!(result.found <= 100 * 10);
        assert!(result.found > 100 * 9);
        assert_eq!(result.user_bytes, 0);
    }

    // 多线程下范围扫描的次数与读到的记录数合计不变量仍然成立。
    #[test]
    fn threaded_seekrandom_splits_scans_across_threads() {
        let target = BTreeMapTarget::new();
        let config = threaded_config(4);
        run_workload(&target, Workload::FillRandom, &config);

        let result = run_workload(&target, Workload::SeekRandom, &config);

        assert_eq!(result.latency_nanos.len(), 100);
        assert!(result.found <= 100 * 10);
        assert!(result.found > 100 * 9);
    }
}
