# LSM 引擎（Rust）

存储这条线的第二个引擎：用 Rust 写的 LSM，以 [mini-lsm](../references/mini-lsm/) 为骨架对照。通用的存储知识在 [storage/notes](../storage/notes/README.md)，这里只放 LSM 的实现与实现笔记；与 `storage/` 的分工见 [存储代码导读](../storage/README.md)。

## 代码地图

`lsm/` 是一个 cargo workspace，工具链由 [rust-toolchain.toml](rust-toolchain.toml) 固定为 1.98.1。

| 目录 | crate | 作用 | 读代码的入口 |
| --- | --- | --- | --- |
| engine | `lsm` | 引擎。对外的句柄 `Db`（后台 flush 与 compaction 线程、关闭）；memtable、引擎层、游标与归并、数据块、SST、compaction 的策略与执行、错误类型、统计计数器 | [lib.rs](engine/src/lib.rs)、[db.rs](engine/src/db.rs)、[compaction/simple_leveled.rs](engine/src/compaction/simple_leveled.rs)、[lsm_storage/compact.rs](engine/src/lsm_storage/compact.rs)、[table.rs](engine/src/table.rs)、[block.rs](engine/src/block.rs)、[lsm_storage.rs](engine/src/lsm_storage.rs)、[mem_table.rs](engine/src/mem_table.rs)、[iterators.rs](engine/src/iterators.rs)、[iterators/merge_iterator.rs](engine/src/iterators/merge_iterator.rs)、[lsm_iterator.rs](engine/src/lsm_iterator.rs)、[stats.rs](engine/src/stats.rs) |
| bench | `lsm-bench` | 基准驱动：固定负载、吞吐、延迟分位数、写放大与读放大 | [main.rs](bench/src/main.rs)、[workload.rs](bench/src/workload.rs)、[runner.rs](bench/src/runner.rs)、[target.rs](bench/src/target.rs) |
| cli | `lsm-cli` | 交互式命令行程序：打开一个目录，一行一条命令地操作引擎，观察 SST 文件的出现 | [main.rs](cli/src/main.rs)、[command.rs](cli/src/command.rs)、[execute.rs](cli/src/execute.rs) |
| bench/results | | 每次基准运行的记录 | [记录方式与列表](bench/results/README.md) |
| notes | | 实现笔记 | [阅读索引](notes/README.md) |

## 为什么这样组织

- **引擎、基准驱动、命令行程序是三个 package。** 依赖按 package 声明，分开后 `hdrhistogram`、`anyhow` 不会变成引擎的依赖；引擎的错误是手写的枚举，两个程序用 `anyhow`。基准驱动与命令行程序只能用引擎的 `pub` 接口，与真正的使用者处在同一位置。另一种做法是把可执行文件放在引擎 package 的 `src/bin/` 下，mini-lsm 采用的是那一种。
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
cargo run -p lsm-cli -- target/demo-db
```

在工作区根目录可用 `make test-lsm`、`make check-lsm`、`make lsm-bench`。

基准必须用 `--release`；调试构建的报告会在"编译"一行标明数字不可用于比较。

## 当前边界

读写路径已经打通，并有自动合并：写入进 memtable，写满冻结，后台 flush 线程把冻结的表写成 L0 的 SST；选项里给了合并参数时，后台 compaction 线程按简单分层策略（L0 文件数触发、相邻层文件数之比触发）把文件一层层合并下去，L0 之下的层数由 `max_levels` 设定。点查与扫描覆盖可写的表、冻结的表、L0 与各层；L0 逐个文件查，各层每层只查一个候选文件，扫描每层一个串接游标。读块先查整个引擎共用的块缓存，每个 SST 先用首末 key 排除，点查再问 Bloom filter。另有手动的全量合并（结果放进最底层）。`close` 先停两个后台线程，再把内存里的数据全部写成 SST。还没有 WAL 与 manifest：重新打开不读取已有的文件，命令行程序因此拒绝打开已有 SST 的目录；也还没有写入停顿。基准驱动有三个被测对象：互斥锁保护的 `BTreeMap`（内存基线）、单张 memtable、完整的引擎（写 SST 到临时目录），用 `--target` 选择；负载有 `fillseq`（顺序写，需显式指定）、`fillrandom`、`overwrite`、`readrandom`、`readmissing`、`seekrandom`（范围扫描）、`compact`（全量合并一次，需显式指定），`--threads` 指定线程数，`--memtable-mb`、`--block-cache-mb`、`--max-levels` 设定引擎的 memtable 上限、块缓存容量与层数，`--compaction simple` 打开后台合并（`--l0-trigger`、`--size-ratio` 是它的参数）；每段负载结束后等后台的 flush 与合并做完再读计数器，报告列出写放大、每次点查进入的 SST 与读的块、各层的文件数与空间放大。读写混合的负载还没有。详细状态见 [当前状态](../docs/status.md)。
