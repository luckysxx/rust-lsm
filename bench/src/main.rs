//! 基准驱动：按顺序在同一个被测对象上跑若干种负载，把结果打印成 Markdown。
//!
//! 用法见 `lsm/README.md`；结果怎样保存见 `lsm/bench/results/README.md`。

mod runner;
mod target;
mod workload;

use std::process::ExitCode;

use lsm::compaction::CompactionOptions;
use lsm::compaction::simple_leveled::SimpleLeveledOptions;
use lsm::compaction::tiered::TieredOptions;
use runner::{Config, WorkloadResult, run_workload};
use target::{EngineConfig, KvTarget, TARGET_NAMES, create_target, join_counts};
use workload::{KEY_SIZE, MAX_KEYS, Workload};

const USAGE: &str = "\
用法：lsm-bench [选项]

  --num N            key 的个数，也是每种负载的操作次数（默认 1000000）
  --value-size N     value 的字节数（默认 100）
  --seed N           随机种子（默认 1）
  --workloads A,B,C  按顺序执行的负载（默认 fillseq 以外的五种）
  --target NAME      被测对象（默认 btreemap）
  --threads N        同时发起操作的线程数（默认 1）
  --scan-length N    范围扫描每次读的记录数（默认 100）
  --memtable-mb N    被测对象为 engine 时，memtable 的上限，单位 MB（默认 64）
  --block-cache-mb N 被测对象为 engine 时，块缓存的容量，单位 MB；0 表示不用（默认 64）
  --max-levels N     被测对象为 engine 时，L0 之下有几层（默认 4；tiered 不用）
  --compaction NAME  被测对象为 engine 时，后台自动合并的策略：none（不自动合并）、
                     simple（简单分层）或 tiered（默认 none）
  --l0-trigger N     simple 策略下，L0 攒到几个文件就并进 L1（默认 2）
  --size-ratio N     simple 策略下，下层的文件数至少是上层的百分之几（默认 200）
  --num-tiers N      tiered 策略下，tier 数到几个才开始检查（默认 8）
  --max-size-amp N   tiered 策略下，空间放大的阈值，百分数（默认 200）
  --tier-size-ratio N  tiered 策略下，下一个 tier 比前面之和大出百分之几才算大得多（默认 1）
  --min-merge-width N  tiered 策略下，按大小比例至少合并几个 tier（默认 2）
  --max-merge-width N  tiered 策略下，减少段数时最多合并几个 tier；0 表示不限（默认 0）
  --help             显示本说明

负载：fillseq（按顺序写）、fillrandom、overwrite、readrandom、readmissing、seekrandom（范围扫描）、
compact（把磁盘上的全部 SST 合并成一层，只做一次；不在默认列表里）。
每段负载计时结束后，等引擎把冻结的表全部写成文件、后台合并做完，再开始下一段；
等待的时间单独列出，不计入吞吐。
被测对象：btreemap（互斥锁加 BTreeMap）、memtable（引擎的单张 memtable）、engine（引擎层）。
读负载读的是前面的写负载留下的数据，所以要把 fillrandom 排在前面。";

struct Args {
    config: Config,
    workloads: Vec<Workload>,
    target: String,
    memtable_megabytes: usize,
    block_cache_megabytes: usize,
    max_levels: usize,
    /// `none`、`simple` 或 `tiered`。
    compaction: String,
    l0_trigger: usize,
    size_ratio_percent: usize,
    num_tiers: usize,
    max_size_amplification_percent: usize,
    tier_size_ratio: usize,
    min_merge_width: usize,
    /// 0 表示不限。
    max_merge_width: usize,
}

impl Args {
    /// 复现这次运行的合并参数：只列出所选策略用到的选项。
    fn compaction_flags(&self) -> String {
        match self.compaction.as_str() {
            "simple" => format!(
                "--compaction simple --l0-trigger {} --size-ratio {}",
                self.l0_trigger, self.size_ratio_percent
            ),
            "tiered" => format!(
                "--compaction tiered --num-tiers {} --max-size-amp {} --tier-size-ratio {} --min-merge-width {} --max-merge-width {}",
                self.num_tiers,
                self.max_size_amplification_percent,
                self.tier_size_ratio,
                self.min_merge_width,
                self.max_merge_width
            ),
            _ => "--compaction none".to_string(),
        }
    }

    /// 被测对象为 engine 时用的配置。
    fn engine_config(&self) -> EngineConfig {
        let compaction = match self.compaction.as_str() {
            "simple" => CompactionOptions::SimpleLeveled(SimpleLeveledOptions {
                level0_file_num_compaction_trigger: self.l0_trigger,
                size_ratio_percent: self.size_ratio_percent,
            }),
            "tiered" => CompactionOptions::Tiered(TieredOptions {
                num_tiers: self.num_tiers,
                max_size_amplification_percent: self.max_size_amplification_percent,
                size_ratio: self.tier_size_ratio,
                min_merge_width: self.min_merge_width,
                max_merge_width: if self.max_merge_width == 0 {
                    None
                } else {
                    Some(self.max_merge_width)
                },
            }),
            _ => CompactionOptions::NoCompaction,
        };
        EngineConfig {
            memtable_size_limit: self.memtable_megabytes * 1024 * 1024,
            block_cache_size: self.block_cache_megabytes * 1024 * 1024,
            max_levels: self.max_levels,
            compaction,
        }
    }
}

fn parse_args(raw: &[String]) -> Result<Option<Args>, String> {
    let mut config = Config {
        num: 1_000_000,
        value_size: 100,
        seed: 1,
        threads: 1,
        scan_length: 100,
    };
    let mut workloads = Workload::DEFAULT.to_vec();
    let mut target = "btreemap".to_string();
    let mut memtable_megabytes = 64;
    let mut block_cache_megabytes = 64;
    let mut max_levels = 4;
    let mut compaction = "none".to_string();
    let mut l0_trigger = 2;
    let mut size_ratio_percent = 200;
    let mut num_tiers = 8;
    let mut max_size_amplification_percent = 200;
    let mut tier_size_ratio = 1;
    let mut min_merge_width = 2;
    let mut max_merge_width = 0;

    let mut index = 0;
    while index < raw.len() {
        let flag = raw[index].as_str();
        if flag == "--help" {
            return Ok(None);
        }

        let value = raw
            .get(index + 1)
            .ok_or_else(|| format!("{flag} 后面缺少值"))?;
        match flag {
            "--num" => config.num = parse_number(flag, value)?,
            "--value-size" => config.value_size = parse_number(flag, value)? as usize,
            "--seed" => config.seed = parse_number(flag, value)?,
            "--workloads" => workloads = parse_workloads(value)?,
            "--target" => target = value.clone(),
            "--threads" => config.threads = parse_number(flag, value)? as usize,
            "--scan-length" => config.scan_length = parse_number(flag, value)? as usize,
            "--memtable-mb" => memtable_megabytes = parse_number(flag, value)? as usize,
            "--block-cache-mb" => block_cache_megabytes = parse_number(flag, value)? as usize,
            "--max-levels" => max_levels = parse_number(flag, value)? as usize,
            "--compaction" => compaction = value.clone(),
            "--l0-trigger" => l0_trigger = parse_number(flag, value)? as usize,
            "--size-ratio" => size_ratio_percent = parse_number(flag, value)? as usize,
            "--num-tiers" => num_tiers = parse_number(flag, value)? as usize,
            "--max-size-amp" => {
                max_size_amplification_percent = parse_number(flag, value)? as usize
            }
            "--tier-size-ratio" => tier_size_ratio = parse_number(flag, value)? as usize,
            "--min-merge-width" => min_merge_width = parse_number(flag, value)? as usize,
            "--max-merge-width" => max_merge_width = parse_number(flag, value)? as usize,
            _ => return Err(format!("不认识的选项：{flag}")),
        }
        index += 2;
    }

    if config.num == 0 || config.num > MAX_KEYS {
        return Err(format!("--num 必须在 1 到 {MAX_KEYS} 之间"));
    }
    if config.threads == 0 || config.threads as u64 > config.num {
        return Err("--threads 必须至少为 1，且不超过 --num".to_string());
    }
    if !TARGET_NAMES.contains(&target.as_str()) {
        return Err(format!("不认识的被测对象：{target}"));
    }
    if config.value_size == 0 {
        return Err("--value-size 必须至少为 1：空 value 在引擎里表示删除".to_string());
    }
    if config.scan_length == 0 {
        return Err("--scan-length 必须至少为 1".to_string());
    }
    if memtable_megabytes == 0 {
        return Err("--memtable-mb 必须至少为 1".to_string());
    }
    if max_levels == 0 {
        return Err("--max-levels 必须至少为 1".to_string());
    }
    if !["none", "simple", "tiered"].contains(&compaction.as_str()) {
        return Err(format!("不认识的合并策略：{compaction}"));
    }
    if l0_trigger == 0 {
        return Err("--l0-trigger 必须至少为 1".to_string());
    }
    if num_tiers < 2 {
        return Err("--num-tiers 必须至少为 2".to_string());
    }
    if min_merge_width < 2 {
        return Err("--min-merge-width 必须至少为 2".to_string());
    }
    if max_merge_width == 1 {
        return Err("--max-merge-width 必须是 0（不限）或至少为 2".to_string());
    }
    Ok(Some(Args {
        config,
        workloads,
        target,
        memtable_megabytes,
        block_cache_megabytes,
        max_levels,
        compaction,
        l0_trigger,
        size_ratio_percent,
        num_tiers,
        max_size_amplification_percent,
        tier_size_ratio,
        min_merge_width,
        max_merge_width,
    }))
}

fn parse_number(flag: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{flag} 的值不是非负整数：{value}"))
}

fn parse_workloads(list: &str) -> Result<Vec<Workload>, String> {
    let mut workloads = Vec::new();
    for name in list.split(',') {
        let workload = Workload::parse(name).ok_or_else(|| format!("不认识的负载：{name}"))?;
        workloads.push(workload);
    }
    Ok(workloads)
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&raw) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let target =
        create_target(&args.target, &args.engine_config()).expect("名字在解析参数时已经检查过");

    let mut results = Vec::new();
    for workload in &args.workloads {
        eprintln!("运行 {} ...", workload.name());
        results.push(run_workload(target.as_ref(), *workload, &args.config));
    }

    print_report(target.as_ref(), &args, &results);
    ExitCode::SUCCESS
}

fn print_report(target: &dyn KvTarget, args: &Args, results: &[WorkloadResult]) {
    let config = &args.config;
    let workload_names: Vec<&str> = args.workloads.iter().map(|w| w.name()).collect();

    let profile = if cfg!(debug_assertions) {
        "debug（数字不可用于比较，请加 --release）"
    } else {
        "release"
    };
    let toolchain = option_env!("RUSTUP_TOOLCHAIN").unwrap_or("未知");
    let cpus = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(0);

    println!("## 运行条件");
    println!();
    println!("- 被测对象：{}", target.name());
    if let Some(details) = target.details() {
        println!("- 被测对象说明：{details}");
    }
    println!(
        "- 参数：`--target {} --threads {} --num {} --value-size {} --seed {} --scan-length {} --memtable-mb {} --block-cache-mb {} --max-levels {} {} --workloads {}`",
        args.target,
        config.threads,
        config.num,
        config.value_size,
        config.seed,
        config.scan_length,
        args.memtable_megabytes,
        args.block_cache_megabytes,
        args.max_levels,
        args.compaction_flags(),
        workload_names.join(",")
    );
    println!(
        "- key {} 字节，value {} 字节，{} 个线程；范围扫描每次读 {} 条",
        KEY_SIZE, config.value_size, config.threads, config.scan_length
    );
    println!("- 编译：{profile}，工具链 {toolchain}");
    println!(
        "- 系统：{} {}，逻辑 CPU {} 个",
        std::env::consts::OS,
        std::env::consts::ARCH,
        cpus
    );
    println!();

    println!("## 吞吐与延迟");
    println!();
    println!(
        "| 负载 | 操作数 | 耗时 (s) | 吞吐 (ops/s) | 写入 (MB/s) | p50 (µs) | p99 (µs) | p99.9 (µs) | 最大 (µs) | 查到或读到的记录 | 之后等后台 (s) |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for result in results {
        let latency = &result.latency_nanos;
        println!(
            "| {} | {} | {:.3} | {:.0} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {:.3} |",
            result.workload.name(),
            result.ops,
            result.elapsed.as_secs_f64(),
            result.ops_per_second(),
            megabytes_per_second(result),
            micros(latency.value_at_quantile(0.50)),
            micros(latency.value_at_quantile(0.99)),
            micros(latency.value_at_quantile(0.999)),
            micros(latency.max()),
            result.found,
            result.drain.as_secs_f64(),
        );
    }
    println!();

    println!("## 放大");
    println!();
    if results.iter().all(|result| result.stats.is_none()) {
        println!("不适用：{} 不写磁盘，没有引擎计数器。", target.name());
        return;
    }
    // 空间放大的分母：全部 key 各一份的大小。基准里没有删除，写满之后 key 的个数不变。
    let live_bytes = args.config.num * (KEY_SIZE + args.config.value_size) as u64;
    println!(
        "| 负载 | 用户写入 (字节) | 磁盘写入 (字节) | 写放大 | 每次点查进入的 SST | 每次点查被 Bloom filter 排除的 SST | 每次点查读的数据块 | 结束时 L0 / L1 / … 的 SST | 结束时 SST 的总字节 | 空间放大 |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    let mut total_user_bytes = 0;
    let mut total_disk_bytes = 0;
    for result in results {
        let Some(stats) = result.stats else {
            continue;
        };
        total_user_bytes += stats.user_bytes_written;
        total_disk_bytes += stats.disk_bytes_written();
        let (layout, total_bytes, space_amplification) = match &result.sst_layout {
            Some(layout) => (
                format!("{} / {}", layout.l0_files, join_counts(&layout.level_files)),
                layout.total_bytes.to_string(),
                ratio(Some(layout.total_bytes as f64 / live_bytes as f64)),
            ),
            None => ("—".to_string(), "—".to_string(), "—".to_string()),
        };
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            result.workload.name(),
            stats.user_bytes_written,
            stats.disk_bytes_written(),
            ratio(stats.write_amplification()),
            ratio(stats.tables_probed_per_get()),
            ratio(stats.tables_skipped_by_bloom_per_get()),
            ratio(stats.blocks_read_per_get()),
            layout,
            total_bytes,
            space_amplification,
        );
    }
    // 整次运行的写放大：compaction 自己没有用户写入，它写的字节要摊到前面的写入上。
    let total_write_amplification = if total_user_bytes == 0 {
        None
    } else {
        Some(total_disk_bytes as f64 / total_user_bytes as f64)
    };
    println!(
        "| 合计 | {total_user_bytes} | {total_disk_bytes} | {} | — | — | — | — | — | — |",
        ratio(total_write_amplification)
    );
    println!();
    println!("空间放大 = 结束时 SST 的总字节 ÷ 全部 key 各一份的大小（{live_bytes} 字节）。");
}

/// 写负载每秒交给被测对象多少 MB（key 加 value）；读负载没有这一项。
fn megabytes_per_second(result: &WorkloadResult) -> String {
    if result.user_bytes == 0 {
        return "—".to_string();
    }
    let megabytes = result.user_bytes as f64 / (1024.0 * 1024.0);
    let rate = megabytes / result.elapsed.as_secs_f64();
    format!("{rate:.1}")
}

fn micros(nanos: u64) -> f64 {
    nanos as f64 / 1000.0
}

fn ratio(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.2}"),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    // 不带参数时的默认规模与 db_bench 相同，并执行全部负载。
    #[test]
    fn defaults_match_db_bench_sizes() {
        let args = parse_args(&[]).unwrap().unwrap();

        assert_eq!(args.config.num, 1_000_000);
        assert_eq!(args.config.value_size, 100);
        assert_eq!(args.config.threads, 1);
        assert_eq!(args.config.scan_length, 100);
        assert_eq!(args.memtable_megabytes, 64);
        assert_eq!(args.block_cache_megabytes, 64);
        assert_eq!(args.max_levels, 4);
        assert_eq!(args.compaction, "none");
        assert!(matches!(
            args.engine_config().compaction,
            CompactionOptions::NoCompaction
        ));
        assert_eq!(args.target, "btreemap");
        assert_eq!(args.workloads, Workload::DEFAULT.to_vec());
    }

    // 命令行选项能覆盖各个默认值。
    #[test]
    fn flags_override_defaults() {
        let raw = strings(&[
            "--num",
            "500",
            "--value-size",
            "32",
            "--seed",
            "9",
            "--workloads",
            "fillrandom,readmissing",
            "--target",
            "memtable",
            "--threads",
            "4",
            "--scan-length",
            "10",
            "--memtable-mb",
            "8",
            "--block-cache-mb",
            "0",
            "--max-levels",
            "3",
            "--compaction",
            "simple",
            "--l0-trigger",
            "4",
            "--size-ratio",
            "400",
        ]);
        let args = parse_args(&raw).unwrap().unwrap();

        assert_eq!(args.config.num, 500);
        assert_eq!(args.config.value_size, 32);
        assert_eq!(args.config.seed, 9);
        assert_eq!(args.config.threads, 4);
        assert_eq!(args.config.scan_length, 10);
        assert_eq!(args.memtable_megabytes, 8);
        assert_eq!(args.block_cache_megabytes, 0);
        assert_eq!(args.max_levels, 3);
        let CompactionOptions::SimpleLeveled(compaction) = args.engine_config().compaction else {
            panic!("应当是简单分层");
        };
        assert_eq!(compaction.level0_file_num_compaction_trigger, 4);
        assert_eq!(compaction.size_ratio_percent, 400);
        assert_eq!(args.target, "memtable");
        assert_eq!(
            args.workloads,
            vec![Workload::FillRandom, Workload::ReadMissing]
        );
    }

    // 缺值、非数字、不认识的负载、选项或被测对象，以及各项数值为 0 或线程数多于操作数，都被拒绝。
    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse_args(&strings(&["--num"])).is_err());
        assert!(parse_args(&strings(&["--num", "abc"])).is_err());
        assert!(parse_args(&strings(&["--num", "0"])).is_err());
        assert!(parse_args(&strings(&["--workloads", "fillrandom,nosuch"])).is_err());
        assert!(parse_args(&strings(&["--frobnicate", "1"])).is_err());
        assert!(parse_args(&strings(&["--target", "nosuch"])).is_err());
        assert!(parse_args(&strings(&["--threads", "0"])).is_err());
        assert!(parse_args(&strings(&["--num", "2", "--threads", "3"])).is_err());
        assert!(parse_args(&strings(&["--value-size", "0"])).is_err());
        assert!(parse_args(&strings(&["--scan-length", "0"])).is_err());
        assert!(parse_args(&strings(&["--memtable-mb", "0"])).is_err());
        assert!(parse_args(&strings(&["--max-levels", "0"])).is_err());
        assert!(parse_args(&strings(&["--compaction", "leveled"])).is_err());
        assert!(parse_args(&strings(&["--num-tiers", "1"])).is_err());
        assert!(parse_args(&strings(&["--min-merge-width", "1"])).is_err());
        assert!(parse_args(&strings(&["--max-merge-width", "1"])).is_err());
        assert!(parse_args(&strings(&["--l0-trigger", "0"])).is_err());
    }

    // tiered 的选项：默认值是 RocksDB 的，num_tiers 默认 8；max-merge-width 为 0 表示不限。
    #[test]
    fn tiered_flags() {
        let args = parse_args(&strings(&["--compaction", "tiered"]))
            .unwrap()
            .unwrap();
        let CompactionOptions::Tiered(defaults) = args.engine_config().compaction else {
            panic!("应当是 tiered");
        };
        assert_eq!(defaults.num_tiers, 8);
        assert_eq!(defaults.max_size_amplification_percent, 200);
        assert_eq!(defaults.size_ratio, 1);
        assert_eq!(defaults.min_merge_width, 2);
        assert_eq!(defaults.max_merge_width, None);

        let raw = strings(&[
            "--compaction",
            "tiered",
            "--num-tiers",
            "16",
            "--max-size-amp",
            "300",
            "--tier-size-ratio",
            "10",
            "--min-merge-width",
            "3",
            "--max-merge-width",
            "6",
        ]);
        let args = parse_args(&raw).unwrap().unwrap();
        let CompactionOptions::Tiered(options) = args.engine_config().compaction else {
            panic!("应当是 tiered");
        };
        assert_eq!(options.num_tiers, 16);
        assert_eq!(options.max_size_amplification_percent, 300);
        assert_eq!(options.size_ratio, 10);
        assert_eq!(options.min_merge_width, 3);
        assert_eq!(options.max_merge_width, Some(6));
        assert_eq!(
            args.compaction_flags(),
            "--compaction tiered --num-tiers 16 --max-size-amp 300 --tier-size-ratio 10 --min-merge-width 3 --max-merge-width 6"
        );
    }

    // --help 不算错误，只是不执行负载。
    #[test]
    fn help_is_not_an_error() {
        assert!(parse_args(&strings(&["--help"])).unwrap().is_none());
    }
}
