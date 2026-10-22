# LSM 引擎的实现笔记

本组笔记讲怎样用 Rust 写一个 LSM 存储引擎，以 mini-lsm 为骨架对照，按它的三周顺序推进。这里只放 LSM 具体实现的内容；换一种引擎仍然成立的知识（文件与持久化、页缓存、提交与恢复、并发）在另一个仓库 [go-btree-kv](https://github.com/luckysxx/go-btree-kv) 的[笔记](https://github.com/luckysxx/go-btree-kv/blob/main/notes/README.md)里（随一个 Go 写的 CoW B+Tree 引擎），文中写作“存储笔记”加链接。“Rust 笔记《标题》”“知识库笔记《标题》”指作者另外的笔记，尚未公开。这些引用只是延伸阅读，各篇不读它们也能看懂。

每篇标题下有一行状态：已有正文（讲解完整）、部分已整理（有些项只列了要点，标"（未展开）"）、未教学（只有大纲，对应的功能还没有实现）。

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
| 一个 WAL `Wal` | 它的 memtable | 直接拥有（`Option` 字段） | 建表时（`open`、冻结），或打开时从文件重建 | memtable 释放时关掉文件；文件本身在 flush 记下高水位之后删除 |

读这张表的办法：

- **直接拥有**：只有一个主人，随主人一起释放。
- **`Arc`、`Bytes`**：几方共同拿着，最后一方放手时释放。表里"谁拿着"一栏有几项，释放的条件就是这几项全部放手。
- **借用**：不拿着，只是看；被借的东西先没了就不能再看，编译器会检查。

用 `Arc` 的三处（状态、memtable、块）是同一个理由：引擎活得久，但它手里的东西会被换掉，而换掉的那一刻可能还有读者在用。这与 B+Tree 里旧页等读者结束后才回收是同一件事，见存储笔记[页回收与读者](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/07-page-reclamation-and-readers.md)；`Arc` 的引用计数相当于自动的读者登记。类型怎样表达这些关系见 Rust 笔记《Arc：跨线程共享所有权》。

## 起步

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L01 | [基准方案与三种放大的测量](01-benchmark-plan-and-amplification.md) | 部分已整理 | 负载、引擎计数器、三种放大的取数、内存基线 | 无对应章节 |

## 结构与格式

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L02 | [memtable 与跳表](02-memtable-and-skiplist.md) | 部分已整理 | 为什么有序与并发、跳表、墓碑的表示、大小估算、数据的归属；冻结与切换、状态快照与两把锁；mini-lsm 与 GreptimeDB 对照 | 第一周第 1 章 |
| L03 | [迭代器接口与多路归并](03-iterators-and-merge.md) | 部分已整理 | 游标、多路归并与堆、同 key 取新、墓碑的先后、分层；memtable 游标与自引用；归并（当前来源放在堆外）、去墓碑与引擎扫描的实现；mini-lsm 与 GreptimeDB 对照 | 第一周第 2 章 |
| L04 | [数据块格式](04-block-format.md) | 部分已整理 | 为什么切成块、块与 SST 和 memtable 的关系、两层索引、与页的对照、大小为何可以不固定、块满与过大的记录、块内定位、格式、LevelDB 的 restart point 对照；构建器、编解码与块上游标的实现、二分的写法；mini-lsm 与 GreptimeDB 对照 | 第一周第 3 章 |
| L05 | [SST 格式](05-sst-format.md) | 部分已整理 | 文件的组成与索引、构建、落盘的步骤、`pread` 与 mmap、错误类型、打开与读块、按末 key 找块、SST 的游标、对文件内容的检查、块缓存（为什么缓存块、LRU 与 TinyLFU、按字节计容量）、块的压缩 | 第一周第 4 章 |
| L06 | [读路径](06-read-path.md) | 部分已整理 | 状态里的 L0、跨来源的点查、两路归并、范围的两端、读放大、按 key 范围排除文件；状态快照与文件的生命周期未展开 | 第一周第 5 章 |
| L07 | [写路径](07-write-path.md) | 部分已整理 | flush 做什么、顺序与加锁、后台线程、关闭、写入为什么暂时不返回错误；写入停顿未展开 | 第一周第 6 章 |
| L08 | [Bloom filter 与 key 前缀压缩](08-bloom-filter-and-prefix-compression.md) | 已有正文 | Bloom filter 的原理与误判率推导、k 个位置的取法、不会漏报的三条要求、存放与点查路径、扫描用不上；前缀压缩的原理、参照第一个 key 与上一个 key 的取舍、还原 key 的代价、为以后改格式留余地；mito2 按列建 filter 的导读；两次基准 | 第一周第 7 章 |

## compaction 与持久化

| 编号 | 笔记 | 状态 | 主要范围 | mini-lsm |
| --- | --- | --- | --- | --- |
| L09 | [compaction 的执行](09-compaction-execution.md) | 已有正文 | 只增不减的代价、层（L0、sorted run、为什么要 L0）、墓碑何时可以丢、读 L1 与串接游标、扫描为什么要归并、执行步骤与丢失更新、删除旧文件与 Unix 语义、输出切分、tiered 与 leveled；mito2 换状态与删文件的导读；基准 | 第二周第 1 章 |
| L10 | [简单分层 compaction](10-simple-leveled-compaction.md) | 已有正文 | 为什么要自动、分层与三种放大、简单分层的规则与优先级、策略与执行分开（任务、只给一个任务）、多层状态的表示、执行与换状态、并行合并的约束、后台线程（两种等待、关闭顺序）、穿过空层的复制；mito2 的 picker 与调度器导读；基准 | 第二周第 2 章 |
| L11 | [tiered compaction](11-tiered-compaction.md) | 已有正文 | leveled 为什么写得多、只合并大小相近的 run、三种触发与参数、一定会停下、正确性（相邻、前缀、墓碑、合并期间的 flush）、按文件数模拟与真实引擎的差距、不用 L0 的理由、tier 与 SST、两种策略的分支放在哪里；mito2 TWCS 找 sorted run 的导读；基准 | 第二周第 3 章 |
| L12 | [leveled compaction](12-leveled-compaction.md) | 已有正文 | 简单分层的三处粗糙、目标大小从最底层往上算（`base_level_size` 的含义与取值、为什么不从上往下、目标是软的）、优先级与分数、部分合并（L0 拿全部、按整体跨度找重叠、省的是峰值空间与任务时长）、挑哪个文件、换状态时整块插入、trivial move（多个 L0 文件一起挪、为什么挪动时才排序）、写放大从哪里来、分支还是 trait、L0 单独放的代价；mini-lsm、RocksDB、mito2 对照；基准 | 第二周第 4 章 |
| L13 | [manifest](13-manifest.md) | 已有正文 | 记录内容与编码、写入时机、恢复 SST 集合 | 第二周第 5 章 |
| L14 | [WAL](14-wal.md) | 已有正文 | 持久化的四级与同步策略、混用时的前缀保证、WAL 与 memtable 的一一对应与两条不变量、高水位作废、编号取两者中较大的、重放、真实进程崩溃的测试 | 第二周第 6 章 |
| L15 | [写批次与校验和](15-write-batch-and-checksums.md) | 已有正文 | 崩溃原子与可见原子、一批是 WAL 的一条记录（旧格式兼容）、跳表没有批次写、group commit 的三种结构与"只合并 fsync"、条件变量与锁的粒度、SST 格式版本 2 的校验和、恢复模式的取舍 | 第二周第 7 章 |

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

## 代码与资料

- [代码与运行方式](../README.md)、[基准结果记录](../bench/results/README.md)。
- [已知问题与限制](../README.md#已知问题与限制)、[后续安排](../README.md#后续安排)。
- 参考源码的链接都指向固定的版本：mini-lsm 是 tag v202608（[教程目录](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/SUMMARY.md)），GreptimeDB 是 tag v1.2.1（存储引擎在 [`src/mito2`](https://github.com/GreptimeTeam/greptimedb/tree/v1.2.1/src/mito2)）。
