# LSM 引擎的实现笔记

本组用 Rust 写一个 LSM 存储引擎，以 mini-lsm 为骨架对照，按它的三周顺序推进。这里只放 LSM 具体实现的内容；换一种引擎仍然成立的知识（文件与持久化、页缓存、提交与恢复、并发）在 [存储笔记](../../storage/notes/README.md) 中，需要时链接过去。LSM 的整体思路先读 [B+Tree 与 LSM](../../storage/notes/06-storage-comparisons/03-btree-and-lsm.md)。

每篇标题下有一行状态：已有正文 / 部分已整理 / 未教学。部分已整理的篇在大纲中把未展开的项标"（未展开）"。

## 全景

各篇笔记讲的是下面这张图里的一个部件。读某一篇之前，先在这里找到它的位置。

```
                 写入 put / delete
                       │
          ①追加        ▼         ②插入
   ┌──────────┐   ┌─────────────────────┐
   │   WAL    │   │   可写的 memtable    │        内存
   └──────────┘   └─────────────────────┘
        磁盘                │ ③写满：冻结，换一张新的
                            ▼
                  ┌─────────────────────┐
                  │  冻结的 memtable …   │        内存
                  └─────────────────────┘
                            │ ④后台 flush：写成一个 SST
   ─────────────────────────┼──────────────────────────────
                            ▼
       L0   [SST] [SST] [SST]        彼此的 key 范围可以重叠     磁盘
                            │ ⑤后台 compaction：归并进下一层
                            ▼
       L1   [SST][SST][SST][SST]     层内 key 范围不重叠
                            ▼
       L2   [SST][SST][SST][SST][SST][SST] …
                                              
       MANIFEST：记录每一层现在有哪些 SST                        磁盘
```

**一次写入**：先追加到 WAL（①），再插入可写的 memtable（②），此时就算成功。之后的事都在后台：memtable 写满后冻结，换一张新的接收写入（③）；冻结的那张被顺序写成一个 SST 文件放进 L0，记入 MANIFEST，对应的 WAL 随后删除（④）；L0 的文件多了，compaction 把它们与下一层归并，逐层下沉（⑤）。

**一次点查**：从新到旧依次找，遇到值或墓碑就停。顺序是可写的 memtable、冻结的 memtable（新的在前）、L0 的各个 SST（新的在前）、L1、L2……。在每个 SST 里，先看 key 是否落在它的首末 key 之间，再问 Bloom filter，然后用索引找到数据块，最后在块内查找。

**一次范围扫描**：上述每个来源各出一个按 key 有序的迭代器，归并成一个有序的结果；同一个 key 取最新的，墓碑不输出。

**崩溃之后**：内存里的 memtable 没了，磁盘上留下 SST、MANIFEST 和 WAL。读 MANIFEST 得到每一层有哪些 SST，重放 WAL 重建 memtable，不在 MANIFEST 里的文件是没做完的残留，删掉。

**磁盘上的样子**：一个 LSM 数据库是一个目录，不是一个文件。SST（Sorted String Table，有序字符串表）是其中一个个独立的普通文件，写完不再修改，被 compaction 取代后整个删除。

```
db/
├── 00007.sst        L1 的一个 SST
├── 00008.sst        L1 的一个 SST
├── 00011.sst        L0 的一个 SST
├── 00012.sst        L0 的一个 SST
├── 00013.wal        当前可写 memtable 的 WAL
└── MANIFEST         哪些 .sst 有效、各在哪一层
```

文件名里的编号只是创建的先后，不表示层；一个文件属于哪一层由 MANIFEST 记录。与单文件的 B+Tree 相比：页号换成了文件编号，meta 页换成了 MANIFEST，空闲页的回收换成了删除文件，交给文件系统。

| 部件 | 没有它会怎样 | 笔记 |
| --- | --- | --- |
| memtable | 每次写入都要直接改磁盘上的有序结构，回到随机写 | [L02](02-memtable-and-skiplist.md) |
| 迭代器与归并 | 数据分散在多个来源，无法给出一个有序的结果；flush 与 compaction 也无从下手 | [L03](03-iterators-and-merge.md) |
| 数据块、SST | memtable 写满后无处可去 | [L04](04-block-format.md)、[L05](05-sst-format.md) |
| 读路径 | 落盘之后的数据读不到 | [L06](06-read-path.md) |
| 写路径（flush） | 内存无限增长 | [L07](07-write-path.md) |
| Bloom filter | 查一个不存在的 key 要翻遍每个文件 | [L08](08-bloom-filter-and-prefix-compression.md) |
| compaction | 文件越积越多，读越来越慢，被覆盖和删除的数据永远占着空间 | [L09](09-compaction-execution.md)～[L12](12-leveled-compaction.md) |
| MANIFEST | 重启后不知道哪些文件有效、各在哪一层 | [L13](13-manifest.md) |
| WAL | 进程崩溃，memtable 里尚未落盘的写入全部丢失 | [L14](14-wal.md) |
| 写批次、校验和 | 多条写入不能一起成功或失败；文件损坏发现不了 | [L15](15-write-batch-and-checksums.md) |
| MVCC | 读者看不到一个固定时刻的快照，也没有事务 | L16～L22 |

知识点的顺序与数据流过的顺序不同：先在内存里把读写做通（L02、L03），再做磁盘格式（L04、L05），接通读与写（L06、L07），然后整理（L08～L12），最后才补上重启与崩溃（L13、L14）。所以在 L13 之前，引擎重启后数据会丢，这是顺序使然，不是遗漏。

## 谁拿着什么

全景图画的是数据怎样流动；这张表列的是内存里每样东西由谁拿着、什么时候出现、什么时候释放。随知识点推进补充。

| 东西 | 谁拿着 | 用什么拿 | 什么时候出现 | 什么时候释放 |
| --- | --- | --- | --- | --- |
| 引擎 `LsmStorage` | 使用引擎的程序 | 直接拥有 | `open` | 程序丢掉它时 |
| 一份状态 `LsmStorageState` | 引擎的当前状态指针；每次点查、扫描取的快照 | `Arc` | `open`；每次冻结复制出新的一份 | 被新状态换下，并且拿着它的快照都用完之后 |
| 快照（函数里的局部变量） | 一次 `get` 或 `scan` 调用 | `Arc` | 调用开始时 | 调用返回时 |
| 一张 memtable | 列着它的每一份状态；建在它上面的每个游标 | `Arc` | `open`；每次冻结换上的新表 | 没有任何状态列着它（flush 之后），并且它上面的游标都用完之后 |
| memtable 里的一条 key 或 value | 表；`get` 返回的值；游标记住的当前记录 | `Bytes` | 写入时拷贝一次 | 表释放，并且交出去的 `Bytes` 都用完之后 |
| memtable 的游标 | 归并游标的堆 | 直接拥有 | `scan` | 走到头被移出堆，或整个扫描的游标被丢掉时 |
| 扫描返回的游标（去墓碑、归并两层） | `scan` 的调用方 | 直接拥有 | `scan` | 调用方丢掉它时 |
| 游标 `key()`、`value()` 返回的字节 | 调用方 | 借用 `&[u8]` | 调用时 | 不归调用方管；下一次 `next` 之后不能再用 |
| 一个块 `Block` | 建在它上面的每个游标；之后还有块的缓存 | `Arc` | 构建器封口，或从字节解码 | 最后一个拿着它的游标或缓存放手时 |
| 块的构建器 | 调用方（之后是 SST 的构建器） | 直接拥有 | 新开一个块时 | 封口时：数据交给做好的块，构建器不再存在 |
| 状态读写锁的读守卫 | 取快照的那一行；一次写入 | 局部变量 | 取快照；写入开始 | 复制完指针；这条记录写进表之后 |

读这张表的办法：

- **直接拥有**：只有一个主人，随主人一起释放。
- **`Arc`、`Bytes`**：几方共同拿着，最后一方放手时释放。表里"谁拿着"一栏有几项，释放的条件就是这几项全部放手。
- **借用**：不拿着，只是看；被借的东西先没了就不能再看，编译器会检查。

用 `Arc` 的三处（状态、memtable、块）是同一个理由：引擎活得久，但它手里的东西会被换掉，而换掉的那一刻可能还有读者在用。这与 B+Tree 里旧页等读者结束后才回收是同一件事，见 [页回收与读者](../../storage/notes/05-storage-recovery/07-page-reclamation-and-readers.md)；`Arc` 的引用计数相当于自动的读者登记。类型怎样表达这些关系见 [[rust-learning/notes/09-concurrency/04-Arc：跨线程共享所有权|Arc：跨线程共享所有权]]。

## 起步

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L01 | [基准方案与三种放大的测量](01-benchmark-plan-and-amplification.md) | 部分已整理 | 负载、引擎计数器、三种放大的取数、内存基线 | 无对应章节 |

## 结构与格式

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L02 | [memtable 与跳表](02-memtable-and-skiplist.md) | 部分已整理 | 为什么有序与并发、跳表、墓碑的表示、大小估算、数据的归属；冻结与切换、状态快照与两把锁；mini-lsm 与 GreptimeDB 对照 | 第一周第 1 章 |
| L03 | [迭代器接口与多路归并](03-iterators-and-merge.md) | 部分已整理 | 游标、多路归并与堆、同 key 取新、墓碑的先后、分层；memtable 游标与自引用；归并、去墓碑与引擎扫描的实现；mini-lsm 与 GreptimeDB 对照 | 第一周第 2 章 |
| L04 | [数据块格式](04-block-format.md) | 部分已整理 | 为什么切成块、块与 SST 和 memtable 的关系、两层索引、与页的对照、大小为何可以不固定、块满与过大的记录、块内定位、格式、LevelDB 的 restart point 对照；构建器、编解码与块上游标的实现、二分的写法；mini-lsm 与 GreptimeDB 对照 | 第一周第 3 章 |
| L05 | [SST 格式](05-sst-format.md) | 部分已整理 | 文件的组成与索引、构建、落盘的步骤、`pread` 与 mmap、错误类型、打开与读块、按末 key 找块、SST 的游标、对文件内容的检查；block cache 未展开 | 第一周第 4 章 |
| L06 | [读路径](06-read-path.md) | 部分已整理 | 状态里的 L0、跨来源的点查、两路归并、范围的两端、读放大；状态快照与文件的生命周期未展开 | 第一周第 5 章 |
| L07 | [写路径](07-write-path.md) | 部分已整理 | flush 做什么、顺序与加锁、后台线程、关闭；写入停顿、按 key 范围过滤未展开 | 第一周第 6 章 |
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
