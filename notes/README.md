# LSM 引擎的实现笔记

本组用 Rust 写一个 LSM 存储引擎，以 mini-lsm 为骨架对照，按它的三周顺序推进。这里只放 LSM 具体实现的内容；换一种引擎仍然成立的知识（文件与持久化、页缓存、提交与恢复、并发）在 [存储笔记](../../storage/notes/README.md) 中，需要时链接过去。LSM 的整体思路先读 [B+Tree 与 LSM](../../storage/notes/06-storage-comparisons/03-btree-and-lsm.md)。

每篇标题下有一行状态：已有正文 / 部分已整理 / 未教学。部分已整理的篇在大纲中把未展开的项标"（未展开）"。

## 起步

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L01 | [基准方案与三种放大的测量](01-benchmark-plan-and-amplification.md) | 部分已整理 | 负载、引擎计数器、三种放大的取数、内存基线 | 无对应章节 |

## 结构与格式

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L02 | [memtable 与跳表](02-memtable-and-skiplist.md) | 未教学 | 并发有序表、冻结与切换、内存中的墓碑 | 第一周第 1 章 |
| L03 | [迭代器接口与多路归并](03-iterators-and-merge.md) | 未教学 | 迭代器接口、堆归并、同 key 取新、范围扫描 | 第一周第 2 章 |
| L04 | [数据块格式](04-block-format.md) | 未教学 | 记录编码、偏移数组、块内查找 | 第一周第 3 章 |
| L05 | [SST 格式](05-sst-format.md) | 未教学 | 数据块、块元信息、文件末尾、block cache | 第一周第 4 章 |
| L06 | [读路径](06-read-path.md) | 未教学 | 两路归并、跨来源的点查与扫描、状态快照 | 第一周第 5 章 |
| L07 | [写路径](07-write-path.md) | 未教学 | flush 到 L0、触发与后台线程、按 key 范围过滤 | 第一周第 6 章 |
| L08 | [Bloom filter 与 key 前缀压缩](08-bloom-filter-and-prefix-compression.md) | 未教学 | Bloom filter、前缀压缩、基准对比 | 第一周第 7 章 |

## compaction 与持久化

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L09 | [compaction 的执行](09-compaction-execution.md) | 未教学 | 归并执行、保留与丢弃、状态切换、旧文件删除 | 第二周第 1 章 |
| L10 | [简单分层 compaction](10-simple-leveled-compaction.md) | 未教学 | 文件数触发、层间比例触发、compaction 线程 | 第二周第 2 章 |
| L11 | [tiered compaction](11-tiered-compaction.md) | 未教学 | 有序段、三种触发条件、放大的取舍 | 第二周第 3 章 |
| L12 | [leveled compaction](12-leveled-compaction.md) | 未教学 | 目标大小、基准层、优先级、选文件 | 第二周第 4 章 |
| L13 | [manifest](13-manifest.md) | 未教学 | 记录内容与编码、写入时机、恢复 SST 集合 | 第二周第 5 章 |
| L14 | [WAL](14-wal.md) | 未教学 | 记录编码、与 memtable 的对应、重放、同步策略 | 第二周第 6 章 |
| L15 | [写批次与校验和](15-write-batch-and-checksums.md) | 未教学 | 写批次、四处校验和、损坏与残尾 | 第二周第 7 章 |

## MVCC

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L16 | [带时间戳的 key](16-timestamped-keys.md) | 未教学 | 排序规则、编码、各层的改动 | 第三周第 1 章 |
| L17 | [提交时间戳与多版本](17-commit-timestamps-and-versions.md) | 未教学 | 提交时间戳、保留全部版本、只返回最新版本 | 第三周第 2 章 |
| L18 | [快照读](18-snapshot-read.md) | 未教学 | 读时间戳、多版本点查与扫描、时间戳的恢复 | 第三周第 3 章 |
| L19 | [watermark 与旧版本回收](19-watermark-and-gc.md) | 未教学 | watermark 的维护、compaction 中的回收 | 第三周第 4 章 |
| L20 | [事务工作区与原子提交](20-transaction-workspace-and-atomic-commit.md) | 未教学 | 私有工作区、读自己的写、提交、原子写 WAL | 第三周第 5 章 |
| L21 | [可串行化校验](21-serializable-validation.md) | 未教学 | 读写集合、提交时校验、记录回收 | 第三周第 6 章 |
| L22 | [compaction filter](22-compaction-filters.md) | 未教学 | 接口、按前缀删除 | 第三周第 7 章 |

## 代码、进度与资料

- [代码与运行方式](../README.md)、[基准结果记录](../bench/results/README.md)。
- [实现与验证状态](../../docs/status.md)、[学习进度](../../docs/learning-progress.md)、[完整清单](../../docs/curriculum.md#l)。
- [项目总目录](../../docs/index.md)、[学习路线](../../docs/roadmap.md)、[本阶段的起点与已定决定](../../docs/rust-lsm-handoff.md)。
- [参考源码](../../references/README.md)：mini-lsm 固定在 tag v202608，[教程目录](../../references/mini-lsm/mini-lsm-book/src/SUMMARY.md)。
- Rust 语言本身的知识在 [[rust-learning/notes/README|Rust 笔记]]，按需查阅。
