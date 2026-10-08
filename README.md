# LSM 引擎（Rust）

存储这条线的第二个引擎：用 Rust 写的 LSM，以 [mini-lsm](../references/mini-lsm/) 为骨架对照。通用的存储知识在 [storage/notes](../storage/notes/README.md)，这里只放 LSM 的实现与实现笔记；与 `storage/` 的分工见 [存储代码导读](../storage/README.md)。

## 代码地图

`lsm/` 是一个 cargo workspace，工具链由 [rust-toolchain.toml](rust-toolchain.toml) 固定为 1.98.1。

| 目录 | crate | 作用 | 读代码的入口 |
| --- | --- | --- | --- |
| engine | `lsm` | 引擎。目前只有统计计数器 | [lib.rs](engine/src/lib.rs)、[stats.rs](engine/src/stats.rs) |
| bench | `lsm-bench` | 基准驱动：固定负载、吞吐、延迟分位数、放大 | [main.rs](bench/src/main.rs)、[workload.rs](bench/src/workload.rs)、[runner.rs](bench/src/runner.rs)、[target.rs](bench/src/target.rs) |
| bench/results | | 每次基准运行的记录 | [记录方式与列表](bench/results/README.md) |
| notes | | 实现笔记 | [阅读索引](notes/README.md) |

## 运行

在 `lsm/` 目录内：

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo run --release -p lsm-bench
cargo run --release -p lsm-bench -- --help
```

在工作区根目录可用 `make test-lsm`、`make check-lsm`、`make lsm-bench`。

基准必须用 `--release`；调试构建的报告会在"编译"一行标明数字不可用于比较。

## 当前边界

引擎还没有任何读写功能。基准驱动目前只有一个被测对象：互斥锁保护的 `BTreeMap`，作为内存基线。负载有 `fillrandom`、`overwrite`、`readrandom`、`readmissing` 四种，单线程；范围扫描、空间放大的测量随引擎功能加入。详细状态见 [当前状态](../docs/status.md)。
