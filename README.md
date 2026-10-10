# LSM 引擎（Rust）

存储这条线的第二个引擎：用 Rust 写的 LSM，以 [mini-lsm](../references/mini-lsm/) 为骨架对照。通用的存储知识在 [storage/notes](../storage/notes/README.md)，这里只放 LSM 的实现与实现笔记；与 `storage/` 的分工见 [存储代码导读](../storage/README.md)。

## 代码地图

`lsm/` 是一个 cargo workspace，工具链由 [rust-toolchain.toml](rust-toolchain.toml) 固定为 1.98.1。

| 目录 | crate | 作用 | 读代码的入口 |
| --- | --- | --- | --- |
| engine | `lsm` | 引擎。目前有 memtable、引擎层（可写与冻结的 memtable）、游标与归并、统计计数器 | [lib.rs](engine/src/lib.rs)、[lsm_storage.rs](engine/src/lsm_storage.rs)、[mem_table.rs](engine/src/mem_table.rs)、[iterators.rs](engine/src/iterators.rs)、[iterators/merge_iterator.rs](engine/src/iterators/merge_iterator.rs)、[lsm_iterator.rs](engine/src/lsm_iterator.rs)、[stats.rs](engine/src/stats.rs) |
| bench | `lsm-bench` | 基准驱动：固定负载、吞吐、延迟分位数、放大 | [main.rs](bench/src/main.rs)、[workload.rs](bench/src/workload.rs)、[runner.rs](bench/src/runner.rs)、[target.rs](bench/src/target.rs) |
| bench/results | | 每次基准运行的记录 | [记录方式与列表](bench/results/README.md) |
| notes | | 实现笔记 | [阅读索引](notes/README.md) |

## 为什么这样组织

- **引擎与基准驱动是两个 package。** 依赖按 package 声明，分开后 `hdrhistogram` 不会变成引擎的依赖；基准驱动只能用引擎的 `pub` 接口，与真正的使用者处在同一位置。另一种做法是把可执行文件放在引擎 package 的 `src/bin/` 下，mini-lsm 采用的是那一种。
- **用 workspace。** 成员共用一份 `Cargo.lock` 与一个 `target/`，在 `lsm/` 下执行一次命令就作用于全部成员。之后的服务端也作为成员加入。
- **固定工具链。** 基准数字与 clippy 的检查结果随编译器版本变化，固定后前后可比。命令要在 `lsm/` 目录内执行，rustup 才会选中这个版本。

这些机制本身见 Rust 笔记：[[rust-learning/notes/13-engineering/01-Cargo-基础、package、crate-与编译目标|package、crate 与编译目标]]、[[rust-learning/notes/13-engineering/03-Cargo-workspace|Cargo workspace]]、[[rust-learning/notes/13-engineering/04-依赖版本、Cargo.lock-与工具链|依赖版本、Cargo.lock 与工具链]]、[[rust-learning/notes/13-engineering/08-单元测试与测试组织|单元测试与测试组织]]、[[rust-learning/notes/13-engineering/15-格式化、Clippy-与编译器诊断|格式化、Clippy 与编译器诊断]]。

约定：单元测试写在被测代码同一个文件末尾的 `tests` 子模块里；每个测试上方有一行中文注释，说明它检查的是哪条性质。

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

引擎目前有 memtable（一张并发跳表）和把多张 memtable 组织起来的引擎层：写入、删除、点查、范围扫描、写满冻结；数据只在内存里，冻结的表还不会被写成 SST。基准驱动有三个被测对象：互斥锁保护的 `BTreeMap`（内存基线）、单张 memtable、引擎层，用 `--target` 选择；负载有 `fillrandom`、`overwrite`、`readrandom`、`readmissing`、`seekrandom`（范围扫描）五种，`--threads` 指定线程数，`--memtable-mb` 设定引擎的 memtable 上限；读写混合与空间放大的测量随引擎功能加入。详细状态见 [当前状态](../docs/status.md)。
