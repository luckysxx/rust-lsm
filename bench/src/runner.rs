//! 执行一种负载：逐个操作计时，记录延迟分布、总耗时与计数器的增量。

use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use lsm::stats::StatsSnapshot;

use crate::target::KvTarget;
use crate::workload::{
    KEY_SIZE, Rng, Workload, existing_key, make_value, missing_key, shuffled_indexes,
};

/// 一次运行的参数。记进报告，重跑时照抄。
#[derive(Debug, Clone)]
pub struct Config {
    /// key 的个数，也是每种负载的操作次数。
    pub num: u64,
    pub value_size: usize,
    pub seed: u64,
}

/// 一种负载跑完后的结果。
pub struct WorkloadResult {
    pub workload: Workload,
    pub ops: u64,
    /// 从第一个操作开始到最后一个操作结束。不含生成操作序列的时间。
    pub elapsed: Duration,
    /// 每次操作的耗时，单位纳秒。
    pub latency_nanos: Histogram<u64>,
    /// 读负载中查到值的次数；写负载为 0。
    pub found: u64,
    /// 写负载中交给被测对象的字节（key 加 value）；读负载为 0。
    pub user_bytes: u64,
    /// 这段负载期间引擎计数器的增量；被测对象没有计数器时为 `None`。
    pub stats: Option<StatsSnapshot>,
}

impl WorkloadResult {
    pub fn ops_per_second(&self) -> f64 {
        self.ops as f64 / self.elapsed.as_secs_f64()
    }
}

pub fn run_workload(target: &dyn KvTarget, workload: Workload, config: &Config) -> WorkloadResult {
    let seed = config.seed.wrapping_add(workload.seed_offset());
    let mut rng = Rng::new(seed);
    let value = make_value(config.value_size, config.seed);

    // 三位有效数字：记录值与真实值相差不超过千分之一。
    let mut latency_nanos = Histogram::<u64>::new(3).unwrap();
    let mut found = 0u64;
    let mut user_bytes = 0u64;

    let stats_before = target.stats();
    let started;

    match workload {
        Workload::FillRandom => {
            let order = shuffled_indexes(config.num, &mut rng);
            started = Instant::now();
            for index in order {
                let key = existing_key(index);
                let op_started = Instant::now();
                target.put(&key, &value);
                record(&mut latency_nanos, op_started.elapsed());
                user_bytes += (KEY_SIZE + value.len()) as u64;
            }
        }
        Workload::Overwrite => {
            started = Instant::now();
            for _ in 0..config.num {
                let index = rng.below(config.num);
                let key = existing_key(index);
                let op_started = Instant::now();
                target.put(&key, &value);
                record(&mut latency_nanos, op_started.elapsed());
                user_bytes += (KEY_SIZE + value.len()) as u64;
            }
        }
        Workload::ReadRandom => {
            started = Instant::now();
            for _ in 0..config.num {
                let index = rng.below(config.num);
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
            started = Instant::now();
            for _ in 0..config.num {
                let index = rng.below(config.num);
                let key = missing_key(index);
                let op_started = Instant::now();
                let result = target.get(&key);
                record(&mut latency_nanos, op_started.elapsed());
                if result.is_some() {
                    found += 1;
                }
            }
        }
    }

    let elapsed = started.elapsed();
    let stats_after = target.stats();
    let stats = match (stats_before, stats_after) {
        (Some(before), Some(after)) => Some(after.since(&before)),
        _ => None,
    };

    WorkloadResult {
        workload,
        ops: config.num,
        elapsed,
        latency_nanos,
        found,
        user_bytes,
        stats,
    }
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
        }
    }

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

    #[test]
    fn target_without_counters_reports_no_stats() {
        let target = BTreeMapTarget::new();
        let result = run_workload(&target, Workload::FillRandom, &small_config());

        assert!(result.stats.is_none());
    }
}
