# rust-lsm

A learning-oriented LSM-tree storage engine in Rust, built on the skeleton of [mini-lsm](https://github.com/skyzh/mini-lsm). It has a memtable, SSTs with Bloom filters and per-block checksums, four compaction strategies (none, simple leveled, tiered, leveled with trivial moves), an incremental MANIFEST, and a per-memtable WAL with group commit. Recovery after a real process crash (SIGKILL) is tested. A benchmark driver measures throughput, latency percentiles, and write / read / space amplification, and every significant change comes with a recorded run. Design notes are written in Chinese. This is a study project and is not meant for production use.

一个学习用的 LSM 存储引擎，用 Rust 写，以 [mini-lsm](https://github.com/skyzh/mini-lsm) 为骨架，按它的前两周推进。每一步都附带基准记录和设计笔记。笔记写的不只是怎样实现，还有为什么这样做、有哪些取舍、和 RocksDB、GreptimeDB mito2 的对照。不用于生产。

## 现在能做什么

对外的入口是 [`lsm::db::Db`](engine/src/db.rs)。

- **写入**：`put`、`delete`、`write_batch`，每次写入可以选择是否同步（`WriteOptions { sync }`）。写入先追加到当前 memtable 的 WAL，再插入 memtable（无锁跳表）。memtable 写满就冻结，交给后台 flush 线程写成 L0 的 SST。
- **读取**：`get`、`scan`，依次覆盖可写的 memtable、冻结的 memtable、L0 和下面各层。每个 SST 先按首末 key 排除；点查还要问 Bloom filter，读块时先查共用的块缓存（[moka](https://github.com/moka-rs/moka)）。多路归并游标在同一个 key 上取最新的版本，并跳过墓碑。
- **合并**：打开数据库时四选一。
  - `NoCompaction`
  - `SimpleLeveled`：按相邻层的文件数之比触发。
  - `Tiered`：按空间放大、相邻大小比例、tier 个数三种条件触发。
  - `Leveled`：从最底层往上算每层的目标大小，每次只把一个文件和下层重叠的文件合并；没有重叠时只挪动文件。

  另有手动的全量合并 `force_full_compaction`。
- **持久化与恢复**：
  - flush 与合并的每次状态变化，都作为一条增量追加到 `MANIFEST` 并同步，之后才生效。
  - 每张 memtable 有自己的 WAL。flush 在 manifest 里记下高水位，然后删掉这张表的 WAL。
  - 打开时重放 manifest 和高水位之上的 WAL，并删掉崩溃留下的残留文件。
  - 一个写批次是 WAL 里的一条记录，崩溃后整批要么都在，要么都不在。
- **磁盘格式**：
  - 块内的 key 相对块首做前缀压缩。
  - 每个数据块、索引、Bloom filter 各带一个 crc32。
  - WAL 与 manifest 的每条记录带长度和 crc32。
  - SST 带格式版本号，仍能读没有校验和的旧版本。
- **工具**：基准驱动 `lsm-bench`，以及交互式命令行程序 `lsm-cli`。

## 在 mini-lsm 之上做了什么

整体结构沿用 mini-lsm：memtable、块与 SST、归并游标、三种合并策略、manifest、WAL、写批次与校验和。下面是与它不同、或者它没有的部分。

- **基准驱动与放大计数**（[笔记](notes/01-benchmark-plan-and-amplification.md)）：
  - 引擎内部有一组计数器：用户写入的字节、WAL 字节、flush 与合并写出的字节、每次点查进入几个 SST、读了几个块、Bloom filter 挡掉了几次。
  - 基准驱动跑 db_bench 式的固定负载，报告吞吐、延迟分位数，以及写、读、空间三种放大。被测对象可以是内存基线（`BTreeMap`）、单张 memtable 或完整的引擎。
  - 每次重要改动的前后对比都有[记录](bench/results/README.md)。
- **真实进程崩溃的测试**（[`engine/tests/crash.rs`](engine/tests/crash.rs)）：子进程一边写一边把完成的写入报告给父进程，父进程用 SIGKILL 杀掉它，再打开数据库，检查读到的恰好是前若干次写入的结果：完成的写入一条不少，中间也没有空洞。memtable 设得很小，杀掉的那一刻多半有 flush 或合并正在进行。测试覆盖了同步与不同步两种写入，以及连续崩溃三次、每次接着恢复出的数据往后写的情况。
- **manifest 记录结果，而不是任务**（[笔记](notes/13-manifest.md)）：
  - mini-lsm 记下合并任务，恢复时重新算一遍结果。这里每条记录直接写出删掉哪些文件、加上哪些文件、放在哪一层，接近 RocksDB 的 VersionEdit。
  - 第一条记录写下数据库的形状（分层与层数，或者 tiered），用不同的形状打开时报错。
  - WAL 是否已被 flush 用一个高水位 `flushed_memtable_id` 表示，不为每张新表单独记一条。
- **group commit**（[笔记](notes/15-write-batch-and-checksums.md)）：
  - 并发的同步写入共用一次 fsync：一个原子变量记录已经追加到哪里；另外两个变量（已同步到哪里、有没有人在同步）由一把锁保护，配一个条件变量。fsync 在锁外做，等待的线程睡在条件变量上。
  - 16 个线程的同步写入从每秒约 270 次提高到约 2100 次。
- **leveled 的 trivial move**（[笔记](notes/12-leveled-compaction.md)）：上层的文件与下层没有重叠时只改 manifest，不复制数据。几个互不重叠的 L0 文件也能一起挪。顺序写入时，写放大从 1.87 降到 0.95。
- **单独的错误类型**：引擎返回手写的 `Error` 枚举（`Io`、`Corruption`、`IncompatibleOptions`、`InvalidArgument`），不用 `anyhow`。超过 65535 字节的 key 或 value 返回错误，而不是写出读不回来的文件。
- **SST 的格式版本**：SST 末尾带版本号。加校验和时升到版本 2，仍能读版本 1。
- **独立的 package**：引擎、基准驱动、命令行程序是同一个 workspace 里的三个 package。两个程序只能用引擎的 `pub` 接口，`hdrhistogram`、`anyhow` 不会变成引擎的依赖。

## 快速开始

工具链由 [rust-toolchain.toml](rust-toolchain.toml) 固定为 1.98.1，在本目录内运行命令时，rustup 会自动选中它。

```sh
cargo test                                   # 单元测试、真实进程崩溃测试
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

用命令行程序看文件怎样出现和消失（memtable 上限默认 4096 字节，写几十条就会冻结）：

```sh
cargo run -p lsm-cli -- target/demo-db       # 第二、三个参数可以选 memtable 上限与合并策略
```

```text
> fill 200          写 200 条
> status            冻结的表、各层的 SST、目录里的 SST 与 WAL 文件
> crash             不关闭就结束进程
```

再运行一次同一个命令，刚才写的数据都在（从 WAL 恢复）。输入 `help` 列出全部命令。

跑一次基准（必须用 `--release`）：

```sh
cargo run --release -p lsm-bench -- --help
cargo run --release -p lsm-bench -- --target engine --compaction leveled --wal on
```

## 代码地图

| 目录 | crate | 作用 | 读代码的入口 |
| --- | --- | --- | --- |
| [engine](engine) | `lsm` | 引擎 | [db.rs](engine/src/db.rs)（对外的句柄与后台线程）、[lsm_storage.rs](engine/src/lsm_storage.rs)（状态、读写、flush、打开与恢复）、[lsm_storage/compact.rs](engine/src/lsm_storage/compact.rs)（合并的执行）、[compaction/](engine/src/compaction)（三种策略）、[mem_table.rs](engine/src/mem_table.rs)、[wal.rs](engine/src/wal.rs)、[manifest.rs](engine/src/manifest.rs)、[table.rs](engine/src/table.rs)、[block.rs](engine/src/block.rs)、[iterators/](engine/src/iterators)、[stats.rs](engine/src/stats.rs) |
| [bench](bench) | `lsm-bench` | 基准驱动：固定负载、吞吐、延迟分位数、三种放大 | [main.rs](bench/src/main.rs)、[workload.rs](bench/src/workload.rs)、[runner.rs](bench/src/runner.rs)、[target.rs](bench/src/target.rs) |
| [cli](cli) | `lsm-cli` | 交互式命令行程序 | [main.rs](cli/src/main.rs)、[command.rs](cli/src/command.rs)、[execute.rs](cli/src/execute.rs) |
| [bench/results](bench/results) | | 每次基准运行的记录 | [记录方式与列表](bench/results/README.md) |
| [notes](notes) | | 设计笔记（中文） | [阅读索引与全景图](notes/README.md) |
| [docs](docs) | | 工程文档：模块、线程、锁、调用链与不变量 | [architecture.md](docs/architecture.md) |

单元测试写在被测代码所在文件末尾的 `tests` 子模块里，每个测试上方用一行注释说明检查的是哪条性质。

## 设计笔记

[notes/](notes/README.md) 按知识点编号，L01～L15 对应已经实现的部分，L16～L22 是 MVCC 的计划，目前只有大纲。可以先读这几篇：

- [全景图](notes/README.md#全景)：一次写入、点查、扫描、崩溃恢复各经过哪些部件。
- [L12 leveled compaction](notes/12-leveled-compaction.md)：每层的目标大小为什么从最底层往上算，部分合并、trivial move，以及写放大从哪里来。
- [L14 WAL](notes/14-wal.md)：四级持久化、WAL 与 memtable 一一对应时的两条不变量，以及怎样用真实进程崩溃来测试。
- [L15 写批次与校验和](notes/15-write-batch-and-checksums.md)：崩溃原子与可见原子、group commit 的三种结构、恢复模式的取舍。

## 基准

几个有代表性的结论，详细数字和运行方式见各条[记录](bench/results/README.md)：

- Bloom filter 让查不存在的 key 快 19～43 倍，文件大约 1%。
- 写放大：简单分层 4～6（其中约 2 来自穿过空层的复制），tiered 2～2.6，leveled 约 4.3。leveled 的读和空间放大接近简单分层。
- manifest 在每次状态变化时做一次 `F_FULLFSYNC`（约 3 毫秒），写入的最大延迟因此从几十微秒变成几毫秒。
- 开 WAL 不同步时，单线程写入慢五到七倍；每次都同步时每秒只有几百次。group commit 让 16 个线程的同步写入快约 8 倍。把 100 条写入攒成一批再同步，比逐条同步快 93 倍。
- 校验和让读慢 5～12%，文件大约 0.1%。

所有数字都是在一台 Apple M5 笔记本（内置 SSD、APFS）上测的，只适合在同一台机器上前后比较。

## 已知问题与限制

照实列出，按影响程度排序。

**正确性**

- **覆盖写时可能读到更旧的值**：crossbeam 的 `SkipMap::insert` 替换已有的 key 时，先删旧节点、再插新节点，中间有一段时间这个 key 不在表里。这时读者会往冻结的表和 SST 里找，读到更旧的值。实测可以复现。根本的解法是 MVCC：key 带上时间戳，跳表只插入、不替换。
- **写批次只做到崩溃原子**：并发的读者可能看到半批。可见原子要等写入带上序号。
- **出错之后不停写**：后台 flush 或合并持续失败时只打印错误，仍然接受写入；`close` 之后也仍然接受写入（不会落盘）。WAL 写到一半失败（例如磁盘满）会留下半条记录；manifest 的 `write` 成功而同步失败时，flush 线程会重试。这两种情况都会让下次打开时被当作损坏。正确的做法是像 RocksDB 的 background error 那样，出错后停止写入。

**持久性的验证范围**

- 只验证了进程崩溃（SIGKILL），页缓存还在。没有验证断电：需要模拟“只有同步过的字节会留下”，或者用 dm-flakey 这类工具。
- WAL 与 manifest 的恢复规则只有严格的一种：末尾不完整就截掉，中间损坏就拒绝打开。写入不同步时，断电可能在中间留下垃圾，导致数据库打不开。RocksDB 默认停在第一处损坏（point-in-time recovery）。

**性能与资源**

- 没有写入停顿：合并跟不上写入时，L0 会一直积压，冻结的表在内存里排队也没有上限。
- 一个 SST 最多 4 GiB：一张 memtable 写成一个文件，memtable 上限设得接近 4 GiB 时，这张表的 flush 一直失败、留在内存里。
- 开 WAL 后，不同步写入在 memtable 的写锁里排成一列（追加 WAL、插入跳表），多线程反而比单线程慢。
- manifest 的追加和 fsync 在状态大锁里做，写入的最大延迟有几毫秒。
- 合并单线程执行，后台线程每 50 毫秒轮询一次，flush 之后不会被叫醒。leveled 挑文件只看编号，不看与下层重叠的大小。
- Bloom filter 全部常驻内存；合并读输入时经过块缓存，会挤掉有用的块。
- manifest 没有 checkpoint，只增不减，打开时从头重放。

## 后续安排

方向大致如下，不设日期：

1. **MVCC**（对应 mini-lsm 第三周，笔记 L16～L22）：带时间戳的 key、提交时间戳与多版本、快照读、watermark 与旧版本回收、事务的私有工作区与原子提交、可串行化校验、compaction filter。上面三个问题会随之一起解决：半批可见、覆盖写读到旧值、写锁竞争。届时还会一起设计“分配序号、合并 `write`、并发插入跳表”，对照 RocksDB 的 pipelined write。
2. **补齐持久性**：出错后停止写入、`close` 之后拒绝写入，以及分层的验证：正常重开、进程内故障注入、真实进程崩溃、残尾与损坏、模拟断电。之后再加可选的宽松恢复模式。
3. **性能**：写入停顿、flush 之后叫醒合并线程、把 manifest 的写入移出大锁、合并读取不进块缓存。
4. **分布式**：共识层用 [raft-rs](https://github.com/tikv/raft-rs)，在这个引擎上组装一个固定三副本、多进程的 KV 服务。

## 致谢与许可证

骨架与章节顺序来自 Alex Chi 的 [mini-lsm](https://github.com/skyzh/mini-lsm)（Apache-2.0）。设计对照主要参考 RocksDB、LevelDB 与 GreptimeDB 的 mito2。

本项目以 [Apache License 2.0](LICENSE) 发布。
