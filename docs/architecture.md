# 架构

本文讲引擎的代码是怎样组织的：分哪些模块、谁依赖谁；运行时有哪些线程，各拿哪些锁；每个操作从入口函数一路调用到哪里；有哪些不变量必须保持。写给要读代码、改代码或审代码的人。

本文描述的是提交 `fe56ca1` 的代码。改动下文提到的结构、锁或调用顺序时，要一起改本文。

各部件为什么这样设计、有哪些别的做法，写在[设计笔记](../notes/README.md)里，本文只给出结论，并链接到对应的那一篇。数据在各部件之间怎样流动，见笔记的[全景图](../notes/README.md#全景)；内存里的每样东西由谁持有、什么时候释放，见[谁拿着什么](../notes/README.md#谁拿着什么)。

## 鸟瞰

一个数据库是一个目录，里面有三种文件：

| 文件 | 名字 | 内容 | 写入方式 |
| --- | --- | --- | --- |
| SST | `00012.sst` | 一个有序、不可变的数据文件 | flush 或合并时一次写完并同步，之后只读；被取代后整个删除 |
| WAL | `00013.wal` | 一张 memtable 收到的全部写入 | 只追加；那张表写成 SST 并记进 manifest 后删除 |
| manifest | `MANIFEST` | 各层有哪些 SST 的变更日志 | 只追加，每条追加完立刻同步 |

SST 与 memtable 共用一个编号计数器，所以 `.sst` 和 `.wal` 的编号不会重复。文件名里的编号只表示创建的先后，一个 SST 在哪一层由 manifest 决定。

workspace 里有三个 package：

| 目录 | package | 作用 | 依赖 |
| --- | --- | --- | --- |
| [engine](../engine) | `lsm` | 引擎本身（库） | `bytes`、`crossbeam-skiplist`、`moka`、`crc32fast`、`xxhash-rust`、`serde`、`serde_json` |
| [bench](../bench) | `lsm-bench` | 基准驱动（程序） | `lsm`、`hdrhistogram` |
| [cli](../cli) | `lsm-cli` | 交互式命令行（程序） | `lsm`、`anyhow` |

两个程序只通过引擎的 `pub` 接口使用它，它们的依赖不会进入引擎。

## 模块与依赖

引擎内部从下往上分成这几层，箭头指向被依赖的一方：

```text
 db                    对外的句柄 Db，启动并停止后台线程
  │
  ▼
 lsm_storage           状态、读写、冻结、flush、打开与恢复
 └─ compact            合并任务的执行（lsm_storage 的子模块）
  │
  ├──────────────┬──────────────┬───────────────┬──────────────┐
  ▼              ▼              ▼               ▼              ▼
 compaction     manifest       mem_table       lsm_iterator   iterators
 三种策略：      变更日志的      跳表 memtable    去墓碑、        归并、两路归并、
 只做决定        编码与重放      └─ wal          在上界停下      按层串接
  │                                                            │
  └────────────────────────────┬───────────────────────────────┘
                               ▼
                             table          SST 的读写、块缓存、Bloom filter
                               │
                               ▼
                             block          数据块的格式

 error、stats：所有模块都可以用
```

| 模块 | 负责 | 不负责 |
| --- | --- | --- |
| [`db`](../engine/src/db.rs) | `Db` 句柄；flush 线程与 compaction 线程的启动、唤醒、停止；`close` | 数据怎样组织 |
| [`lsm_storage`](../engine/src/lsm_storage.rs) | `LsmStorage` 与 `LsmStorageState`；写入、同步、冻结、flush、点查、扫描、打开与恢复；换状态的协议 | 起线程；决定合并什么 |
| [`lsm_storage/compact`](../engine/src/lsm_storage/compact.rs) | 按任务读输入、写新 SST、换状态、删旧文件；全量合并 | 决定合并什么 |
| [`compaction`](../engine/src/compaction.rs) 与 [`compaction/`](../engine/src/compaction) | 看一份 `LsmStorageState`，给出一个任务或者 `None` | 读写文件、拿锁、改状态 |
| [`manifest`](../engine/src/manifest.rs) | 记录的编码、追加并同步、残尾截断、重放成文件列表 | 打开 SST |
| [`mem_table`](../engine/src/mem_table.rs) | 跳表；"追加 WAL 再插入"的顺序；表上的游标 | 什么时候冻结 |
| [`wal`](../engine/src/wal.rs) | WAL 记录的编码、追加、group commit、恢复 | 属于哪张表、什么时候删 |
| [`table`](../engine/src/table.rs) | SST 的构建、打开、读块；块缓存；Bloom filter；校验和；格式版本 | 文件放在哪一层 |
| [`block`](../engine/src/block.rs) | 块内记录的格式、前缀压缩、块上的游标 | 块在文件里的位置 |
| [`iterators`](../engine/src/iterators.rs) | `StorageIterator` 接口；`MergeIterator`、`TwoMergeIterator`、`SstConcatIterator` | 墓碑与上界 |
| [`lsm_iterator`](../engine/src/lsm_iterator.rs) | 跳过墓碑，在扫描范围的上界停下 | 归并 |
| [`error`](../engine/src/error.rs) | `Error` 枚举：`Io`、`Corruption`、`IncompatibleOptions`、`InvalidArgument` | |
| [`stats`](../engine/src/stats.rs) | 原子计数器与快照，算三种放大 | |

依赖上的几条规则：

- **策略不碰磁盘**。`compaction` 的子模块只读 `LsmStorageState`，输出一个任务结构体。执行任务的代码在 `lsm_storage/compact.rs`。因此策略可以用手工搭的状态来测试，加一种策略也不用改执行的代码（[笔记 L09](../notes/09-compaction-execution.md)）。
- **块内格式只在 `block/` 里**。块之外只通过 `BlockBuilder::add`、`Block::encode`、`Block::decode` 和 `BlockIterator` 访问记录。
- **`compact` 是 `lsm_storage` 的子模块**，可以直接使用 `LsmStorage` 的私有字段和私有方法（例如 `write_ssts`、`commit_state`），不必为了拆文件把它们改成公开的。
- 例外：`wal` 用了 `block` 的长度上限 `MAX_KEY_LEN`、`MAX_VALUE_LEN`，以及 `manifest` 的 `sync_parent_dir`。

## 核心数据结构

### `LsmStorage`：引擎

```rust
pub struct LsmStorage {
    state: RwLock<Arc<LsmStorageState>>, // 指向当前状态
    state_lock: Mutex<Manifest>,         // 改变结构的操作在这里排队；manifest 在锁里
    flush_lock: Mutex<()>,               // 同一时刻只做一次 flush
    compaction_lock: Mutex<()>,          // 同一时刻只做一次合并
    path: PathBuf,
    next_sst_id: AtomicUsize,            // SST 与 memtable 共用的编号
    block_cache: Option<Arc<BlockCache>>,
    stats: Arc<Stats>,
    options: LsmStorageOptions,
}
```

`LsmStorage` 自己不起线程，所有方法都在调用方的线程里执行。`Db` 用 `Arc` 包住它，分给后台线程。

### `LsmStorageState`：某一时刻由哪些表组成

```rust
pub struct LsmStorageState {
    pub memtable: Arc<MemTable>,              // 可写的表，恰好一张
    pub imm_memtables: Vec<Arc<MemTable>>,    // 冻结的表，新的在前
    pub l0_sstables: Vec<Arc<SsTable>>,       // L0，新的在前，范围可以重叠
    pub levels: Vec<Vec<Arc<SsTable>>>,       // L1 往下各层；或者全部的 tier
}
```

**一份状态建好之后不再修改。** 要改变结构，就在拿到 `state_lock` 之后复制当前状态，改副本，再用写锁把指针换成副本（见[换状态的协议](#换状态的协议)）。读者取快照时只在复制 `Arc` 的一瞬间持有读锁，之后在自己的那份上慢慢读，不会看到它变化。复制只复制 `Arc` 指针，不复制表和文件。

各个列表的排列方式是读写两边共同遵守的约定，归并时靠它判断新旧：

| 列表 | 排列 | 谁会改它 |
| --- | --- | --- |
| `imm_memtables` | 新的在前；冻结插到开头，flush 从末尾取 | 冻结、flush |
| `l0_sstables` | 新的在前；flush 插到开头 | flush 加，合并按编号移除 |
| `levels`（分层） | 长度固定为 `max_levels`，`levels[0]` 是 L1；层内按 key 排好、互不重叠 | 只有合并 |
| `levels`（tiered） | `levels[0]` 最新；每个 tier 内按 key 排好、互不重叠；没有空的 tier | flush 插到开头，合并替换连续的几个 |

用 tiered 策略时 `l0_sstables` 始终是空的。状态是"分层"还是"tier"，由打开时选的策略决定，写进 manifest 的第一条记录，之后打开时会核对（`Shape`）。

### `Db`：对外的句柄

`Db` 持有 `Arc<LsmStorage>`、两个后台线程的 `JoinHandle`，以及给它们发信号的 `mpsc::Sender<Signal>`。它的读写方法直接转给 `LsmStorage`；只多做一件事：写入冻结了一张表时，给 flush 线程发 `Wake`。

## 线程

| 线程 | 名字 | 什么时候有 | 做什么 |
| --- | --- | --- | --- |
| 调用方的线程 | | 任意多个 | `put`、`delete`、`write_batch`、`sync`、`get`、`scan`；写满时就在这个线程里冻结；`force_full_compaction` 和 `close` 也在这里执行 |
| flush 线程 | `lsm-flush` | 一直有 | 收到 `Wake` 或等满 50 毫秒后，调用 `flush_all_imm_memtables` |
| compaction 线程 | `lsm-compaction` | 策略不是 `NoCompaction` 时 | 每 50 毫秒醒来一次，反复调用 `compact_once` 直到它返回 `false`；每做完一个任务检查一次有没有 `Stop` |

两个后台线程的循环在 [`db.rs`](../engine/src/db.rs) 的 `flush_loop` 与 `compaction_loop` 里。它们有几点共同的行为：

- **出错不退出**：失败时打印错误，下一轮重试。失败的那一步没有改变状态，数据还在内存里或者旧文件里。
- **只有两种结束方式**：收到 `Stop`，或者发送端已经被丢掉。
- **compaction 线程不会被叫醒**：`Db` 只给它发 `Stop`，所以 flush 之后最多要等 50 毫秒合并才开始。

关闭的顺序（`Db::close`）：

1. 停 compaction 线程，等它做完手上的任务；
2. 停 flush 线程；
3. 在调用方的线程里冻结可写的表，把冻结的表全部 flush。

先停 compaction 线程，是因为第 3 步会产生新的 L0 文件，不应再为它们开始新的合并。`Drop` 只做第 1、2 步，不 flush：flush 可能失败，而 `drop` 没法把错误交给调用方。

## 同步

### 锁的清单

| 锁 | 保护什么 | 谁拿 | 持有期间做的 I/O |
| --- | --- | --- | --- |
| `LsmStorage::state`（`RwLock`）读锁 | "当前状态是哪一份" | 取快照（一瞬间）；一次写入从取出可写的表，到追加 WAL、插入跳表完成 | 追加 WAL 的 `write`，不 fsync |
| `LsmStorage::state` 写锁 | 同上 | 换状态的最后一步 | 无 |
| `LsmStorage::state_lock`（`Mutex<Manifest>`） | 改变结构的操作排成一列；manifest 只能在锁里追加 | 冻结；flush、合并、全量合并的换状态 | 冻结时新建 WAL 并同步目录；换状态时追加 manifest 并 fsync |
| `LsmStorage::flush_lock` | 同一时刻只有一次 flush | `force_flush_next_imm_memtable` 的全过程 | 写 SST、fsync、同步目录、删 WAL |
| `LsmStorage::compaction_lock` | 同一时刻只有一次合并 | `compact_once`、`force_full_compaction` 的全过程 | 读输入、写 SST、fsync、同步目录、删旧 SST |
| `MemTable::write_lock` | 开了 WAL 时，WAL 里的顺序与插入跳表的顺序一致；没开 WAL 时，多条的批次整批先后生效 | `MemTable::put_batch` | 追加 WAL 的 `write` |
| `Wal::sync_state`（`Mutex<SyncState>`）与 `sync_done`（`Condvar`） | 已经同步到哪里、有没有人正在同步 | `Wal::sync_to` | 无：fsync 在锁外做 |
| `Db` 的两个 `Mutex<Option<JoinHandle>>` | 取走线程把手 | 停线程时 | 无 |

块缓存（`moka::sync::Cache`）与跳表（`crossbeam_skiplist::SkipMap`）自己保证并发安全，不需要外面加锁。计数器是原子变量。

### 加锁的顺序

```text
flush_lock ─┐
            ├──► state_lock ──► state（读或写）
compaction_lock ┘

state（读）──► MemTable::write_lock

Wal::sync_state：拿着它时不再拿任何锁
```

需要遵守的规则：

- **拿着 `state` 的读锁时，不能去拿 `state_lock`，也不能再拿一次 `state` 的读锁。** 冻结是先拿 `state_lock` 再拿 `state` 的写锁。std 的读写锁在有写者排队时可能不再放读者进来（macOS 与 Linux 上的实现都是这样），所以同一个线程第二次拿读锁也可能死锁。因此 `write` 先放开读锁，再去做同步（`sync_after_write` 里要取快照）和冻结（`try_freeze`）。
- **`flush_lock` 与 `compaction_lock` 互不嵌套。** flush 与合并可以同时进行，只在最后换状态时通过 `state_lock` 排队。
- **fsync 不在 `state` 的写锁里做。** manifest 的 fsync 在 `state_lock` 里，等它的只有冻结和别的换状态，读者取快照不受影响。

### 换状态的协议

冻结、flush、各种合并、全量合并都按同一个顺序改变结构，这是正确性的核心：

```text
1. 在锁外写好新文件（SST），每个文件 fsync         ← 读写照常进行
2. 有新文件时同步目录（sync_dir）
3. 拿 state_lock
4. 复制当前状态，在副本上改
5. 把这次的变化作为一条 VersionEdit 追加进 manifest 并 fsync   ← commit_state
6. 用 state 的写锁把指针换成副本                                ← commit_state
7. 放开 state_lock
8. 删除不再被引用的文件：旧的 SST，或者已经 flush 的那张表的 WAL
```

由这个顺序可以得到：

- **第 5 步失败时状态不变。** 第 1 步写出的文件没有任何记录提到，下次打开时当作残留删掉。
- **manifest 里记录的先后与状态生效的先后相同**，因为追加只能在 `state_lock` 里做。重放 manifest 得到的就是最后一份状态的文件列表。
- **第 8 步之前崩溃，只会留下多余的文件，不会缺文件。** 下次打开时，多余的 SST 被删掉，编号在高水位以下的 WAL 也被删掉。反过来先删后记的话，崩溃会丢数据。
- **读者手里的旧快照照样能读完。** 它的 `Arc<SsTable>` 一直开着文件；在 Unix 上删除只去掉文件名，最后一个打开它的人关闭后，文件系统才回收空间。

冻结只有第 3、4、6、7 步，外加在第 4 步之前新建 WAL。它不写 manifest：新表的 WAL 编号在打开时从目录里找回来。trivial move 没有第 1、2、8 步，manifest 里记成"去掉这几个文件，再把它们加进下层"。

## 各操作的调用链

下面的缩进表示调用关系，方括号里是这一步持有的锁。

### 写入

```text
Db::put / delete / write_batch(_opt)
└─ LsmStorage::put_opt / delete_opt / write_batch_opt     空 key、Put 的空 value：panic
   └─ LsmStorage::write
      ├─ 检查长度：超过 65535 字节 → Error::InvalidArgument，什么都不写
      ├─ [state 读] 取出可写的表
      │  └─ MemTable::put_batch
      │     └─ [MemTable::write_lock]
      │        ├─ Wal::append            整批编码成一条记录，一次 write_all，返回记录末尾的位置 wal_end
      │        └─ SkipMap::insert × n
      ├─ 计数：用户字节、WAL 字节
      ├─ sync 为真时：sync_after_write（见下）
      └─ try_freeze                    大小没到上限时直接返回
         └─ [state_lock] 再看一次当前的表 → force_freeze_memtable
            ├─ 分配编号，新建 memtable 和它的 WAL（create_new，并同步目录）
            └─ [state 写] 可写的表插到 imm_memtables 开头，换上新表
Db：冻结了就给 flush 线程发 Wake
```

`write` 返回时写入已经进了页缓存：进程崩溃不丢，断电可能丢。写入一旦插进跳表，别的线程就能读到。一个批次是 WAL 里的一条记录，崩溃后整批都在或者都不在；但并发的读者可能读到半批（[笔记 L15](../notes/15-write-batch-and-checksums.md)）。

### 同步

```text
LsmStorage::sync_after_write(memtable, wal_end)         同步写入
├─ 比这张表旧的每张冻结的表：MemTable::sync_wal → Wal::sync
└─ 这张表：MemTable::sync_wal_to(wal_end) → Wal::sync_to(wal_end)

LsmStorage::sync                                         Db::sync
└─ 可写的表与每张冻结的表：Wal::sync

Wal::sync_to(end)                                        group commit
├─ [sync_state] 已同步到 end 之后 → 返回，不做 fsync
├─ [sync_state] 有人正在 fsync → 在 sync_done 上睡下，醒来再看
├─ [sync_state] 标记"正在同步"，记下目前追加到的位置 target
├─ fsync（不拿锁）
└─ [sync_state] 已同步的位置改为 target，清掉标记，叫醒全部等待者
```

一次 fsync 会把开始之前所有已经追加的记录一起带下去，所以并发的同步写入可以共用一次 fsync（[笔记 L15](../notes/15-write-batch-and-checksums.md)）。比这张表旧的冻结表也要同步：否则断电后可能出现"新的写入在，旧的却丢了"。

### flush

```text
flush 线程：flush_all_imm_memtables          冻结列表不空就一直做
└─ force_flush_next_imm_memtable
   └─ [flush_lock]
      ├─ 取快照，最旧的冻结表在列表末尾
      ├─ write_ssts(表上的游标, 不限大小, 保留墓碑)    一张表写成一个 SST；空表不写
      │  └─ SsTableBuilder::build → FileObject::create（写入并 fsync）
      ├─ 有文件时 sync_dir
      ├─ [state_lock]
      │  ├─ 复制状态，从末尾拿掉这张表（断言就是刚才写的那张）
      │  ├─ 新 SST 插到 L0 开头（tiered：作为新 tier 插到 levels 开头）
      │  └─ commit_state：VersionEdit { added, placement, flushed_memtable_id: 表的编号 }
      └─ 删掉这张表的 WAL
```

flush 必须保留墓碑：更旧的 SST 里可能还有这个 key 的值，要靠墓碑盖住。`flushed_memtable_id` 是高水位：flush 总是从最旧的表开始做，所以编号不超过它的表都已经写成 SST（[笔记 L14](../notes/14-wal.md)）。

### 合并

```text
compaction 线程：compact_once
└─ [compaction_lock]
   ├─ 取快照，按策略生成任务：simple_leveled / tiered / leveled::generate_task
   │  没有任务 → 返回 false
   └─ run_*_task
      ├─ leveled 的 trivial move：只做 apply_leveled_result，不读写文件
      ├─ 写新文件：merge_upper_into_lower（简单分层、leveled）或 compact_tiered_task
      │  └─ write_ssts(归并游标, target_sst_size, 下层是不是最底层)
      ├─ 有新文件时 sync_dir
      ├─ apply_*_result：[state_lock] 复制状态，按任务改，commit_state
      └─ remove_sst_files：删掉输入的旧文件
```

任务里的文件列表来自生成任务的那一刻。执行期间 flush 可能往 L0（或者 tier 列表的开头）加了新文件，所以换状态时：

- **L0 按编号移除输入**，合并期间新 flush 出的文件原样留下；
- **L1 往下的层与 tier 只有合并会改**，而同一时刻只有一个合并，所以这些层此时应当和任务里记的完全相同。`apply_*_result` 用断言检查这一点；对不上就 panic，因为继续执行会覆盖别人的改动。

只有输出是最底层（tiered 是包含最旧的 tier）时，才能丢掉墓碑。各策略怎样挑文件，见[笔记 L10～L12](../notes/10-simple-leveled-compaction.md)；`force_full_compaction` 也走同样的四步，只是把全部文件合并成最底层。

### 点查

```text
Db::get → LsmStorage::get
├─ 取快照
├─ 可写的表 → 冻结的表（新的在前）：MemTable::get
├─ L0（新的在前）：get_from_sst
└─ L1、L2…… 或各个 tier：find_sst_index 找到唯一可能的文件 → get_from_sst

get_from_sst
├─ key 不在首末 key 之间 → 跳过
├─ Bloom filter 说没有 → 跳过，计数
└─ SsTableIterator::new_and_seek_to_key
   └─ SsTable::read_block：先查块缓存 (SST 编号, 块下标)，没有再读文件、核对 crc32、解码
```

遇到这个 key 的第一条记录就停下：空的 value 是墓碑，返回 `None`。

### 扫描

```text
Db::scan → LsmStorage::scan
├─ 取快照
└─ 按从新到旧拼游标：
   LsmIterator                                 跳过墓碑，在上界停下
   └─ TwoMergeIterator
      ├─ TwoMergeIterator
      │  ├─ MergeIterator<MemTableIterator>    可写的表 + 冻结的表
      │  └─ MergeIterator<SsTableIterator>     与范围相交的 L0 文件
      └─ MergeIterator<SstConcatIterator>      每层（或每个 tier）一个串接游标
```

同一个 key 出现在几个来源里时，归并游标给出排在前面（更新）的那一个。返回的游标拿着快照里各张表和各个 SST 的 `Arc`，用多久它们就活多久。扫描只固定了"有哪些表"，没有固定数据：快照里的可写表在被冻结之前还会接收写入，游标之后的新 key 会被扫到（[笔记 L06](../notes/06-read-path.md)）。

### 打开与恢复

```text
Db::open
├─ LsmStorage::open
│  ├─ open_manifest
│  │  ├─ 没有 MANIFEST：目录里已有 .sst 或 .wal → Error::Corruption；否则新建，写 Init(Shape)
│  │  ├─ MANIFEST 是空的：补写 Init
│  │  └─ Manifest::recover（截掉不完整的最后一条）→ replay → 与选项核对形状
│  ├─ 逐个打开 manifest 里的 SST；分层时每层按首 key 排序
│  ├─ remove_unlisted_ssts       删掉目录里不在 manifest 中的 .sst
│  ├─ recover_memtables           编号不超过高水位的 .wal 删掉；其余按编号重放，都成为冻结的表
│  └─ 新的可写表：编号取 manifest 的 next_sst_id 与重放出的最大编号 + 1 中较大的
└─ 启动 flush 线程（和 compaction 线程）
```

重放出的冻结表由 flush 线程在第一轮写成 SST，之后删掉它们的 WAL。哪些情况会被当作损坏而拒绝打开，见[笔记 L13](../notes/13-manifest.md) 与 [L14](../notes/14-wal.md)。

## 不变量

下面每一条都由代码或断言保证，改代码时不能破坏：

1. **状态不可变**：一份 `LsmStorageState` 发布之后不再修改，只能整份替换。
2. **新旧顺序**：同一个 key 的各个版本里，可写的表最新，然后依次是冻结的表（列表靠前的更新）、L0（靠前的更新）、L1、L2……（tiered 是靠前的 tier 更新）。点查、扫描、合并都靠这个顺序决定谁胜出。
3. **层内有序**：L1 往下每层、每个 tier 都是一个 sorted run，按 key 排好、相邻文件的范围不重叠。leveled 换状态后用 `is_sorted_run` 断言检查。
4. **一批写入只进一张表**：写入期间持有 `state` 的读锁，冻结要拿写锁，所以一批不会分到两张表里。
5. **WAL 与表一一对应**：WAL 里记录的顺序就是插入跳表的顺序。
6. **先记后删**：一个文件要等 manifest 里已经没有记录引用它，才能删除；一个 WAL 要等高水位越过它的编号，才能删除。
7. **编号不重复**：`next_sst_id` 只增不减；manifest 里出现的编号都小于它记下的下一个编号，打开时检查这一点。
8. **墓碑只在最底层丢掉**：`drop_tombstones` 只在输出是最底层（或包含最旧的 tier）时为真。
9. **改 L1 往下的层只有合并**：flush 只往 L0（或 tier 列表的开头）加文件。

## 出错时的行为

| 在哪里出错 | 返回 | 之后的状态 |
| --- | --- | --- |
| key 或 value 太长 | `InvalidArgument` | 什么都没写 |
| 空 key、`Put` 的空 value | panic | 调用方的错误，不算运行时错误 |
| 追加 WAL | `Io` | 跳表没变；文件末尾可能留下半条记录 |
| 冻结时新建 WAL | `Io` | 状态不变，还是原来那张可写的表 |
| flush 或合并写新文件 | `Io` | 状态不变；写了一半的文件下次打开时删掉；后台线程下一轮重试 |
| 追加 manifest | `Io` | 状态不变 |
| 删除旧文件 | `Io` | 新状态已经生效；旧文件下次打开时删掉 |
| 读 SST 时校验和不对 | `Corruption` | 这次读失败 |
| 合并期间别的操作改了它的输入 | panic | 程序的错误：说明违反了不变量 9 |

后台线程出错后只打印错误，引擎照样接受写入；`close` 之后也照样接受写入。这两点和几种会让下次打开报损坏的情况，列在 README 的[已知问题](../README.md#已知问题与限制)里。

## 架构上的限制

这里只列和结构有关的几条，完整的列表见 README 的[已知问题](../README.md#已知问题与限制)。

- **没有目录锁**：两个进程同时打开同一个目录，不会被发现。
- **manifest 的 fsync 在 `state_lock` 里**：冻结要排在它后面，所以写入的最大延迟会有几毫秒。
- **合并单线程，不会被叫醒**：每 50 毫秒轮询一次，没有写入停顿，L0 和冻结的表都可能无限积压。
- **开了 WAL 后，所有写入在 `MemTable::write_lock` 里排成一列。**
- **没有序号**：写入不带时间戳，所以读者可能读到半批，覆盖写时可能短暂地读到更旧的值。计划用 MVCC 解决（[笔记 L16 起](../notes/16-timestamped-keys.md)）。

## 改代码时从哪里下手

| 要做的事 | 要改的地方 |
| --- | --- |
| 加一种合并策略 | 在 `compaction/` 下新建一个模块，提供参数、任务和 `generate_task`；在 `CompactionOptions` 里加一项；在 `compact.rs` 里加 `run_*_task` 与 `apply_*_result`；如果状态的形状不同，还要改 `Shape` 与 `apply_edit` |
| 改块内的格式 | 只改 `block/` 的三个文件 |
| 改 SST 的格式 | `table.rs`、`table/builder.rs`：把 `SST_FORMAT_VERSION` 加一，并让 `SsTable::open` 照样能读旧版本 |
| 改 WAL 或 manifest 的记录格式 | `wal.rs` 或 `manifest.rs` 的编码与解码。旧文件必须还能读：manifest 新加的字段用 `#[serde(default)]` |
| 加一个改变结构的操作 | 按[换状态的协议](#换状态的协议)的顺序：在 `state_lock` 里复制、修改，通过 `commit_state` 发布，删文件放在最后 |
| 加一个统计项 | `stats.rs` 加计数器和快照字段，在发生的地方调用 `add_*`；基准驱动在 `bench/src/main.rs` 里报告 |
| 加一个对外的方法 | `LsmStorage` 上实现，`Db` 上转发；要唤醒后台线程的话在 `Db` 里发信号 |

## 测试在哪里

- **单元测试**：写在被测代码所在文件末尾的 `tests` 子模块里，每个测试上方用一行注释说明它检查的性质。策略的测试只用手工搭的状态，不碰磁盘。
- **并发与 group commit**：`wal.rs` 与 `lsm_storage.rs` 的测试里有只在测试中启用的钩子（`make_syncs_slow`、`hold_fsyncs` 等），用来把一次 fsync 停在半路，确定地制造出交错。
- **真实进程崩溃**：[`engine/tests/crash.rs`](../engine/tests/crash.rs) 用 SIGKILL 杀掉一直在写的子进程，检查重新打开后读到的恰好是前若干次写入的结果。
- **基准**：[`bench`](../bench) 跑固定的负载，结果记录在 [`bench/results`](../bench/results/README.md)。

运行方式见 README 的[快速开始](../README.md#快速开始)。
