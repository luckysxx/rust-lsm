# LSM 引擎（Rust）

存储这条线的第二个引擎：用 Rust 写的 LSM，以 [mini-lsm](../references/mini-lsm/) 为骨架对照。通用的存储知识在 [storage/notes](../storage/notes/README.md)，这里只放 LSM 的实现与实现笔记；与 `storage/` 的分工见 [存储代码导读](../storage/README.md)。

## 代码地图

`lsm/` 是一个 cargo workspace，工具链由 [rust-toolchain.toml](rust-toolchain.toml) 固定为 1.98.1。

| 目录 | crate | 作用 | 读代码的入口 |
| --- | --- | --- | --- |
| engine | `lsm` | 引擎。对外的句柄 `Db`（后台 flush 与 compaction 线程、关闭）；memtable、WAL、manifest、引擎层、游标与归并、数据块、SST、compaction 的策略与执行、错误类型、统计计数器 | [lib.rs](engine/src/lib.rs)、[db.rs](engine/src/db.rs)、[compaction/simple_leveled.rs](engine/src/compaction/simple_leveled.rs)、[compaction/leveled.rs](engine/src/compaction/leveled.rs)、[lsm_storage/compact.rs](engine/src/lsm_storage/compact.rs)、[table.rs](engine/src/table.rs)、[block.rs](engine/src/block.rs)、[lsm_storage.rs](engine/src/lsm_storage.rs)、[mem_table.rs](engine/src/mem_table.rs)、[wal.rs](engine/src/wal.rs)、[manifest.rs](engine/src/manifest.rs)、[iterators.rs](engine/src/iterators.rs)、[iterators/merge_iterator.rs](engine/src/iterators/merge_iterator.rs)、[lsm_iterator.rs](engine/src/lsm_iterator.rs)、[stats.rs](engine/src/stats.rs) |
| bench | `lsm-bench` | 基准驱动：固定负载、吞吐、延迟分位数、写放大与读放大 | [main.rs](bench/src/main.rs)、[workload.rs](bench/src/workload.rs)、[runner.rs](bench/src/runner.rs)、[target.rs](bench/src/target.rs) |
| cli | `lsm-cli` | 交互式命令行程序：打开一个目录，一行一条命令地操作引擎，观察 SST 与 WAL 文件的出现；`crash` 不关闭就结束进程，再打开时从 WAL 恢复 | [main.rs](cli/src/main.rs)、[command.rs](cli/src/command.rs)、[execute.rs](cli/src/execute.rs) |
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
cargo run -p lsm-cli -- target/demo-tiered 4096 tiered
cargo run -p lsm-cli -- target/demo-leveled 4096 leveled
```

在工作区根目录可用 `make test-lsm`、`make check-lsm`、`make lsm-bench`。

基准必须用 `--release`；调试构建的报告会在"编译"一行标明数字不可用于比较。

## 当前边界

读写路径已经打通，并有自动合并：写入进 memtable，写满冻结，后台 flush 线程把冻结的表写成 L0 的 SST；选项里选了合并策略时，后台 compaction 线程按策略合并：简单分层（L0 文件数触发、相邻层文件数之比触发）把文件一层层合并下去，L0 之下的层数由 `max_levels` 设定；tiered 不用 L0，flush 出的文件成为最前面的一个新 tier，按空间放大、相邻大小比例、tier 个数三种触发合并最新的几个 tier；leveled 按字节从最底层往上给每层定目标，L0 直接并进基准层，其余层每次只把一个文件与下层重叠的文件合并，不重叠时只挪动（几个互不重叠的 L0 文件可以一起挪）。点查与扫描覆盖可写的表、冻结的表、L0 与各层；L0 逐个文件查，各层每层只查一个候选文件，扫描每层一个串接游标。读块先查整个引擎共用的块缓存，每个 SST 先用首末 key 排除，点查再问 Bloom filter。另有手动的全量合并（分层时结果放进最底层，tiered 时合成一个 tier）。`close` 先停两个后台线程，再把内存里的数据全部写成 SST。MANIFEST 记下每次换状态的增量，重新打开时重放它恢复各层的 SST；开了 WAL（`enable_wal`）时写入先追加到当前 memtable 的 WAL（`WriteOptions::sync` 或 `sync()` 时同步），flush 在 MANIFEST 里记下高水位后删掉对应的 WAL，打开时重放高水位之上的 WAL，进程崩溃不丢已完成的写入（`engine/tests/crash.rs` 用 SIGKILL 验证）。还没有写入停顿、group commit 与写批次。基准驱动有三个被测对象：互斥锁保护的 `BTreeMap`（内存基线）、单张 memtable、完整的引擎（写 SST 到临时目录），用 `--target` 选择；负载有 `fillseq`（顺序写，需显式指定）、`fillrandom`、`overwrite`、`readrandom`、`readmissing`、`seekrandom`（范围扫描）、`compact`（全量合并一次，需显式指定），`--threads` 指定线程数，`--memtable-mb`、`--block-cache-mb`、`--max-levels` 设定引擎的 memtable 上限、块缓存容量与层数，`--compaction simple`、`tiered` 或 `leveled` 打开后台合并（简单分层的参数是 `--l0-trigger`、`--size-ratio`，tiered 的是 `--num-tiers`、`--max-size-amp`、`--tier-size-ratio`、`--min-merge-width`、`--max-merge-width`，leveled 的是 `--l0-trigger`、`--level-multiplier`、`--base-level-mb`），`--wal off|on|sync` 选择不写 WAL、写不同步或每次同步；每段负载结束后等后台的 flush 与合并做完再读计数器，报告列出写放大（其中 WAL 的字节单列）、每次点查进入的 SST 与读的块、各层的文件数与空间放大。读写混合的负载还没有。详细状态见 [当前状态](../docs/status.md)。
