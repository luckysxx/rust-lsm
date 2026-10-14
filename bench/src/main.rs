//! 基准驱动：按顺序在同一个被测对象上跑若干种负载，把结果打印成 Markdown。
//!
//! 用法见 `lsm/README.md`；结果怎样保存见 `lsm/bench/results/README.md`。

mod runner;
mod target;
mod workload;

use std::process::ExitCode;

use runner::{Config, WorkloadResult, run_workload};
use target::{KvTarget, TARGET_NAMES, create_target};
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
  --help             显示本说明

负载：fillseq（按顺序写）、fillrandom、overwrite、readrandom、readmissing、seekrandom（范围扫描）。
每段负载计时结束后，等引擎把冻结的表全部写成文件，再开始下一段；等待的时间单独列出，不计入吞吐。
被测对象：btreemap（互斥锁加 BTreeMap）、memtable（引擎的单张 memtable）、engine（引擎层）。
读负载读的是前面的写负载留下的数据，所以要把 fillrandom 排在前面。";

struct Args {
    config: Config,
    workloads: Vec<Workload>,
    target: String,
    memtable_megabytes: usize,
    block_cache_megabytes: usize,
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
    Ok(Some(Args {
        config,
        workloads,
        target,
        memtable_megabytes,
        block_cache_megabytes,
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

    let memtable_size_limit = args.memtable_megabytes * 1024 * 1024;
    let block_cache_size = args.block_cache_megabytes * 1024 * 1024;
    let target = create_target(&args.target, memtable_size_limit, block_cache_size)
        .expect("名字在解析参数时已经检查过");

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
        "- 参数：`--target {} --threads {} --num {} --value-size {} --seed {} --scan-length {} --memtable-mb {} --block-cache-mb {} --workloads {}`",
        args.target,
        config.threads,
        config.num,
        config.value_size,
        config.seed,
        config.scan_length,
        args.memtable_megabytes,
        args.block_cache_megabytes,
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
        "| 负载 | 操作数 | 耗时 (s) | 吞吐 (ops/s) | 写入 (MB/s) | p50 (µs) | p99 (µs) | p99.9 (µs) | 最大 (µs) | 查到或读到的记录 | 之后等落盘 (s) |"
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
    println!(
        "| 负载 | 用户写入 (字节) | 磁盘写入 (字节) | 写放大 | 每次点查进入的 SST | 每次点查读的数据块 |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    for result in results {
        let Some(stats) = result.stats else {
            continue;
        };
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            result.workload.name(),
            stats.user_bytes_written,
            stats.disk_bytes_written(),
            ratio(stats.write_amplification()),
            ratio(stats.tables_probed_per_get()),
            ratio(stats.blocks_read_per_get()),
        );
    }
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
        ]);
        let args = parse_args(&raw).unwrap().unwrap();

        assert_eq!(args.config.num, 500);
        assert_eq!(args.config.value_size, 32);
        assert_eq!(args.config.seed, 9);
        assert_eq!(args.config.threads, 4);
        assert_eq!(args.config.scan_length, 10);
        assert_eq!(args.memtable_megabytes, 8);
        assert_eq!(args.block_cache_megabytes, 0);
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
    }

    // --help 不算错误，只是不执行负载。
    #[test]
    fn help_is_not_an_error() {
        assert!(parse_args(&strings(&["--help"])).unwrap().is_none());
    }
}
