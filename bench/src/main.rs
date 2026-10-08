//! 基准驱动：按顺序在同一个被测对象上跑若干种负载，把结果打印成 Markdown。
//!
//! 用法见 `lsm/README.md`；结果怎样保存见 `lsm/bench/results/README.md`。

mod runner;
mod target;
mod workload;

use std::process::ExitCode;

use runner::{Config, WorkloadResult, run_workload};
use target::{BTreeMapTarget, KvTarget};
use workload::{KEY_SIZE, MAX_KEYS, Workload};

const USAGE: &str = "\
用法：lsm-bench [选项]

  --num N            key 的个数，也是每种负载的操作次数（默认 1000000）
  --value-size N     value 的字节数（默认 100）
  --seed N           随机种子（默认 1）
  --workloads A,B,C  按顺序执行的负载（默认 fillrandom,overwrite,readrandom,readmissing）
  --help             显示本说明

负载：fillrandom、overwrite、readrandom、readmissing。
读负载读的是前面的写负载留下的数据，所以要把 fillrandom 排在前面。";

struct Args {
    config: Config,
    workloads: Vec<Workload>,
}

fn parse_args(raw: &[String]) -> Result<Option<Args>, String> {
    let mut config = Config {
        num: 1_000_000,
        value_size: 100,
        seed: 1,
    };
    let mut workloads = Workload::ALL.to_vec();

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
            _ => return Err(format!("不认识的选项：{flag}")),
        }
        index += 2;
    }

    if config.num == 0 || config.num > MAX_KEYS {
        return Err(format!("--num 必须在 1 到 {MAX_KEYS} 之间"));
    }
    Ok(Some(Args { config, workloads }))
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

    let target = BTreeMapTarget::new();

    let mut results = Vec::new();
    for workload in &args.workloads {
        eprintln!("运行 {} ...", workload.name());
        results.push(run_workload(&target, *workload, &args.config));
    }

    print_report(&target, &args, &results);
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
    println!(
        "- 参数：`--num {} --value-size {} --seed {} --workloads {}`",
        config.num,
        config.value_size,
        config.seed,
        workload_names.join(",")
    );
    println!(
        "- key {} 字节，value {} 字节，单线程",
        KEY_SIZE, config.value_size
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
        "| 负载 | 操作数 | 耗时 (s) | 吞吐 (ops/s) | 写入 (MB/s) | p50 (µs) | p99 (µs) | p99.9 (µs) | 最大 (µs) | 查到 |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for result in results {
        let latency = &result.latency_nanos;
        println!(
            "| {} | {} | {:.3} | {:.0} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {} |",
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

    #[test]
    fn defaults_match_db_bench_sizes() {
        let args = parse_args(&[]).unwrap().unwrap();

        assert_eq!(args.config.num, 1_000_000);
        assert_eq!(args.config.value_size, 100);
        assert_eq!(args.workloads, Workload::ALL.to_vec());
    }

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
        ]);
        let args = parse_args(&raw).unwrap().unwrap();

        assert_eq!(args.config.num, 500);
        assert_eq!(args.config.value_size, 32);
        assert_eq!(args.config.seed, 9);
        assert_eq!(
            args.workloads,
            vec![Workload::FillRandom, Workload::ReadMissing]
        );
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse_args(&strings(&["--num"])).is_err());
        assert!(parse_args(&strings(&["--num", "abc"])).is_err());
        assert!(parse_args(&strings(&["--num", "0"])).is_err());
        assert!(parse_args(&strings(&["--workloads", "fillrandom,nosuch"])).is_err());
        assert!(parse_args(&strings(&["--frobnicate", "1"])).is_err());
    }

    #[test]
    fn help_is_not_an_error() {
        assert!(parse_args(&strings(&["--help"])).unwrap().is_none());
    }
}
