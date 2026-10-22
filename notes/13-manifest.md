# manifest

状态：已有正文 · 代码：[manifest.rs](../engine/src/manifest.rs) 的 `ManifestRecord`、`Shape`、`VersionEdit`、`Placement`、`Manifest::create`、`Manifest::recover`、`Manifest::append`、`encode_record`、`decode_records`、`RecoveredLayout`、`replay`、`apply_edit`、`remove_ids`；[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `open`、`open_manifest`、`remove_unlisted_ssts`、`list_sst_ids`、`shape_of`、`version_edit`、`commit_state`、`force_flush_next_imm_memtable`；[lsm_storage/compact.rs](../engine/src/lsm_storage/compact.rs) 的 `apply_full_compaction_result`、`apply_simple_leveled_result`、`apply_tiered_result`、`apply_leveled_result`

## 位置

[全景](README.md#全景)里的 MANIFEST。前面十二篇做完了数据怎样流动：写入、flush、读、compaction。从这一篇开始补**重启与崩溃**：本篇让重启后知道哪些 SST 有效、各在哪一层；[L14](14-wal.md) 的 WAL 让 memtable 里还没落盘的写入不丢；[L15](15-write-batch-and-checksums.md) 处理残尾与损坏。只有 manifest、没有 WAL 时，正常关闭（关闭时把内存里的数据全部写成 SST）之后重启数据都在，崩溃时 memtable 里的数据会丢。

## 与 B+Tree 阶段的对应

| B+Tree（单文件） | LSM |
| --- | --- |
| meta 页：根页号、空闲页表，说明"哪些页有效" | manifest：每一层有哪些 SST，说明"哪些文件有效" |
| 页号 | 文件编号 |
| 新页写好、同步之后才让 meta 指向它；meta 换完之后旧页才回收 | 新 SST 写好、同步之后才写进 manifest；记录写完之后旧文件才删除 |
| 双 meta 交替写，打开时选校验和对得上、版本较新的那份 | 只往末尾追加记录，打开时从头重放，末尾不完整的一条当作没写过 |
| 空闲页的回收 | 删除文件，交给文件系统 |

提交顺序与同步的一般原理见存储笔记《提交顺序与同步》、《打开与故障恢复》；目录为什么要单独同步见存储笔记《写入、同步与错误边界》的“父目录的同步”一节。

## 问题：为什么不能直接看目录

数据库目录里有一堆 `.sst`，但从文件本身推不出状态：

1. **层号不在文件里。** trivial move 时文件不动，只有层号变了。
2. **编号大不代表数据新。** 合并要写一段时间，期间 flush 也在取编号：合并的输出拿到 5 和 7，同时 flush 出的 L0 文件拿到 6。6 的数据比 7 新，编号却更小，按编号排新旧会让旧值盖住新值。
3. **目录里有不该算数的文件。** 崩溃时可能留下写了一半的新 SST，或者状态已经换了、旧文件还没删完。后一种最危险：合并到最底层时丢掉了墓碑，旧的输入文件里还留着被删 key 的值，把它们当作有效，删掉的 key 会复活。
4. **下一个编号**要比用过的都大。

所以要有一份单独的记录：哪些文件有效、各在哪层、层内的顺序。

## 记什么：四种记法

| 记法 | 做法 | 好处 | 代价 |
| --- | --- | --- | --- |
| 整份快照 | 每次换状态把各层的编号列表整份写进临时文件，同步，rename 替换，再同步目录 | 简单，打开时不用重放 | 每次 flush 与合并都写全量 |
| 记任务（mini-lsm） | 追加 `Flush(id)`、`Compaction(task, 输出编号)`，重放时再执行一遍换状态 | 复用换状态的代码 | 重放依赖策略的代码不变；换状态要用到的信息重放时也必须拿得到（leveled 要按首 key 排序，文件还没打开，所以 mini-lsm 有 `in_recovery` 分支） |
| 记结果的增量（RocksDB 的 VersionEdit、mito2 的 `RegionEdit`） | 每条记"去掉哪些、加上哪些、放在哪里、下一个编号" | 重放只是集合的增删，与策略无关 | 要自己设计记录；有顺序的列表（L0、tier）要说清楚放在哪个位置 |
| 日志加 checkpoint | 在增量日志的基础上，日志变长后写一份全量，之后的日志接着它记 | 日志不会无限变长 | 多一种文件与切换的过程 |

本项目用增量，没有 checkpoint。

### 顺序怎样表达

增量记法要回答"加上的文件放在哪里"：

- **L0**：一次 flush 只加一个文件，放在最前面。按记录的先后，后加的在前。
- **L1 往下的层**：每层是按首 key 排好、互不重叠的一列，不用记位置，重放完、打开文件之后按首 key 排一次就与原来相同。RocksDB 把首末 key 记进记录，因为它可以不打开文件（`max_open_files` 限制打开的个数），仍要知道每个文件的范围。
- **tier**：被合并的几个 tier 总是连在一起的一块。重放时先找"第一个被去掉的文件所在的 tier"，把这一块换成新 tier；没有去掉文件（flush）时放在最前面；合并的输出为空时只拿掉。也可以把 tier 的下标写进记录，但那样记录就依赖"追加的先后与生效的先后一致"，以后改锁时多一处要证明。

### manifest 是一个退化的 LSM

manifest 已经是追加写的，结构与 LSM 一一对应：日志对应 WAL，内存里的文件列表对应 memtable，"加上文件 7"对应 put，"去掉文件 3"对应墓碑，重放对应重建 memtable，checkpoint 对应 compaction。

它不需要 SST、分层与 Bloom filter，因为数据量小、而且每次都要全部读进来：一个文件的元数据约一百字节，几万个文件也只有几 MB；读路径要知道每个文件的范围，打开时必须拿到完整的列表，不存在"只查一小部分元数据"。所以它的"memtable"永远装得下全部，"compaction"退化成写一份全量快照。

元数据真的放不进一台机器的内存时，工业系统确实把它存进另一个同类的结构：Bigtable 的 METADATA 表本身是 Bigtable 的表，METADATA 的位置记在 root tablet 里，root tablet 的位置记在 Chubby 的一个文件里，三层之后停下；Iceberg 的元数据是 `metadata.json` → manifest list → 若干 manifest 文件组成的树。递归总要停在一个足够简单的东西上，RocksDB 停在 `CURRENT` 文件：内容只有当前 manifest 的文件名。

### 为什么与 WAL 分开

| | WAL | manifest |
| --- | --- | --- |
| 记什么 | 用户的每一次写入 | 文件列表的每一次变化 |
| 频率与大小 | 每次写入一条，量与用户写入相同 | 每次 flush 或合并一条，一秒几条，很小 |
| 谁写 | 前台的写入线程 | 后台的 flush 与 compaction，在换状态时 |
| 同步 | 可选：每条同步、攒一批同步、不同步 | 每条都必须同步 |
| 保留多久 | 对应的 memtable flush 之后整段删除 | 到下一次 checkpoint，记的是整个数据库的历史 |

合成一个日志：旧的 WAL 删除之前要先把其中的文件变化挪到新日志里；打开时为了几十条文件变化要扫完全部用户写入；同步方式只能选一种；前台写入与后台换状态抢同一个文件。所以 LSM 普遍分开，再由 manifest 记下"WAL 写到哪里已经落进 SST"把两者连起来：RocksDB 的 `log_number`，mito2 的 `flushed_entry_id`、`flushed_sequence`，mini-lsm 的 `NewMemtable(id)`。PostgreSQL 只有一个 WAL，建表、删表时的文件创建与删除也写进 WAL；它定期 checkpoint 限制重放长度，而且文件的增删远比 LSM 少。

## 顺序：不变量与崩溃点

> **manifest 里列出的文件一定完整且已同步；文件从 manifest 里去掉之前不能删。**

反过来不要求：盘上可以有 manifest 没提到的文件。这与内存管理里"宁可泄漏，不能悬空"相同：manifest 指向一个不存在的文件是悬空指针，打开失败或读到半截内容；盘上多出一个没被引用的文件是泄漏，打开时当作残留删掉，数据都对。两个方向的先后因此相反：

- **加文件**：先写好文件并同步（文件与目录），再写记录。
- **删文件**：先写记录（把它去掉），再删文件。删除不必同步目录：没落盘时文件"复活"成孤儿，下次打开再删。

flush 与合并都是四步：写新 SST 并同步 → 追加一条记录并同步 → 换内存里的状态 → 删旧文件。

| 崩溃在 | 盘上 | 打开时 |
| --- | --- | --- |
| 写新 SST 的中途 | 半个新文件，记录里没有 | 删掉，等于这次没做 |
| 记录只写了一半 | 新文件完整，最后一条记录不完整 | 丢掉残尾、删掉新文件，等于这次没做 |
| 记录写完、旧文件没删完 | manifest 已是新状态，旧文件还在 | 删掉旧文件 |

两条细则：

- **一次换状态只写一条记录。** 合并既加文件又删文件，增删放在同一条里。拆成"先一条加、再一条删"的话，崩溃在两条之间时新旧文件同时有效，又是删掉的 key 复活。一条记录要么整条生效，要么整条当作没写过。
- **内存状态在记录之后换。** 先换内存、再追加失败，内存说"有文件 8"、盘上没有，接着删掉旧文件，下次打开就悬空了。记录写失败时返回错误、内存状态不动，新写的文件成了孤儿。

## 加锁：记录的先后与生效的先后

追加放在 `state_lock` 里、在 `state` 的写锁外：

1. 拿 `state_lock`（状态只在这把锁里改变，此刻读到的就是最新的）。
2. 复制当前状态，改好新状态，算出记录。
3. 追加并同步。
4. 拿写锁换指针，马上放开。

fsync 在写锁里的话，所有读者取快照都要等这次 fsync。在 `state_lock` 里的代价是冻结也拿这把锁，要等。

放在锁外，flush 与合并各自追加、再各自拿锁换状态，记录在文件里的先后与生效的先后可以相反。按编号增删、两者改的东西不相交时，交换顺序碰巧结果相同；但这是一条要逐对证明的性质，很容易被以后的改动打破：tier 的记录改成按下标写，两条一交换就指向不同的 tier；checkpoint 从内存写全量时，如果正好落在"某次 flush 已追加、还没换状态"的那一刻，快照里没有这个文件，记录又在快照之前被丢弃，重启后它被当作孤儿删掉。放在锁里，两者的先后从构造上相同，不用证明。

RocksDB 不想在数据库的大锁里 fsync：写 MANIFEST 时放开大锁，用一个排队的队列（`manifest_writers_`）保证记录按队列顺序写入、也按同样的顺序生效，还能把几条 VersionEdit 攒成一次写入。它多付一套队列的复杂度，保住的是同一条性质。

## 下一个编号

编号的作用是给新文件起名，不与已有的文件重名。孤儿按"在不在 manifest 里"判断，与编号大小无关：孤儿可能比记下的都大（写了一半的新文件），也可能更小（没删完的旧输入）。

恢复下一个编号有两种办法：记进每条记录，或者打开时取"manifest 里最大的编号 + 1"。打开时先删孤儿，再开始分配，两种都不会撞上已有的文件。但有了 WAL 之后，后者不够：memtable 与 SST 共用一套编号、每张 memtable 有自己的 WAL 文件时，还没 flush 的 memtable 有 WAL、没有 SST，manifest 里查不到它的编号，"最大 + 1"会再发出这个编号。所以 RocksDB 在 VersionEdit 里记 `next_file_number`。本项目每条记录带上追加时计数器的值：分配编号不拿锁，但已经分配出去的编号都比这个值小。

## 打开时

1. 读 manifest，逐帧解出记录，末尾不完整的一条截掉。
2. 从空状态开始逐条重放，得到每层的编号列表与下一个编号；形状与选项对不上时拒绝。
3. 打开这些 SST；分层时每层按首 key 排序。
4. 删掉目录里不在列表中的 SST。
5. 从记下的编号开始分配。

几种边界情况：

| 目录里 | 处理 | 理由 |
| --- | --- | --- |
| 没有 manifest，也没有 SST | 新建，写下第一条（形状） | 新的数据库 |
| 没有 manifest，却有 SST | 拒绝打开，文件不动 | 这些文件不是按 manifest 管理的，不能当作残留删掉 |
| manifest 一条记录也没有 | 补写第一条，当作新的 | 新建之后、第一条写完之前崩溃；之后的记录都在第一条之后，不可能有 SST 被记下，目录里的 SST 都是残留 |
| manifest 记下的 SST 不在目录里 | 拒绝打开（损坏） | 悬空，说明有人删了文件或顺序被打破 |
| 记下的编号不小于下一个编号 | 拒绝打开（损坏） | 以后会分配出重名的编号 |

### 形状与换策略

不合并、简单分层、leveled 的状态都是"L0 加 `max_levels` 层"，tiered 是一列个数会变的 tier。用另一种形状打开，结构就对不上，所以第一条记录写下形状（分层时连同 `max_levels`），打开时核对：

- 分层与 tiered 互换：拒绝。
- `max_levels` 变小：manifest 里有放不下的层，拒绝；变大不会出错，但最底层换了，原来的最底层成了中间层，并进去时不再丢墓碑。本项目两种都拒绝。
- 不合并、简单分层、leveled 之间：允许。前提是 leveled 不会把 L0 跳过一个有数据的上层并进下面的基准层（简单分层从 L1 往下填，换成 leveled 打开时 L1 常有数据），见 [L12 的优先级](12-leveled-compaction.md#优先级)。

RocksDB 也把形状相关的选项记在文件里核对：打开时 `num_levels` 比已有文件所在的层小会报错。

## 文件格式与残尾

每条记录是一帧：

```
长度（8 字节，大端）│ 记录的 JSON（"长度"个字节）│ JSON 的 crc32（4 字节，大端）
```

- **长度**：记录首尾相接，读的时候靠它找到下一条的开头。
- **JSON**：`serde_json` 把记录编码成文本，如 `{"Edit":{"removed":[2],"added":[3],"placement":{"Level":1},"next_sst_id":4}}`；`cat MANIFEST` 能大致看懂，调试方便。
- **crc32**：JSON 能解析出来不代表内容没坏。`"removed":[2]` 的 `2` 被改成 `3`，仍是合法的记录，意思却变成了"去掉 3 号"，重放得到错误的状态而不报错；crc 能发现这种改动。

逐帧解的时候有三种检查，各管一件事：

| 检查 | 发现什么 | 处理 |
| --- | --- | --- |
| 剩下的字节够不够长度说的那么多 | 最后一条只写了一半 | 停下，截掉 |
| crc 对不对得上 | 内容不是当初写的样子 | 是最后一条：残尾，截掉；后面还有记录：损坏，报错 |
| JSON 解不解得出来 | 不是这个引擎的记录 | 报错 |

为什么最后一条坏了可以丢、中间的不行：记录只往末尾追加，每条同步完、`append` 返回之后才换状态、才写下一条。崩溃只能弄坏正在写的最后一条，它的 `append` 没有返回，状态没换、旧文件没删，丢掉它等于这次换状态没做过。中间一条坏了、后面还有记录，说明它当初写完了，是之后被改坏的（磁盘损坏、别的程序写了文件、代码 bug）：跳过它，后面依赖它的记录会去掉不存在的文件、留下早已删掉的文件；从它截断，后面那些已经执行过的删除就成了悬空。两种都会不报错地得到错误的状态，只能停下。文件长度完整、最后一条 crc 却对不上，也可能是残尾：有的文件系统断电时先把文件长度改大了，内容还没写进去。RocksDB 重放 WAL 的 `kTolerateCorruptedTailRecords` 就是这种"末尾容忍、中间报错"的做法，`kAbsoluteConsistency` 则任何损坏都报错。

截掉残尾要在追加新记录之前做：否则新记录接在半条记录后面，下次打开时读到半条就停了，后面完好的记录全读不到。

长度来自文件，是不可信的输入：可能是任意值。`长度 + 12` 用普通加法时，长度接近上限会溢出：Rust 的 debug 构建 panic，release 构建与 C、Go 一样悄悄绕回一个很小的数，"剩下的够不够"这一步轻易通过，接着按错误的长度切片。用 `usize::try_from` 与 `checked_add`，溢出时当作残尾。

## 本项目的做法

- **模块**：[manifest.rs](../engine/src/manifest.rs)。记录是 `ManifestRecord::Init(Shape)` 与 `ManifestRecord::Edit(VersionEdit)`；`VersionEdit { removed, added, placement, next_sst_id }`，`Placement` 是 `L0`、`Level(n)`、`Tier`。`Manifest` 只有一个打开的文件：`create`（`create_new` 不覆盖，同步文件与父目录）、`recover`（读出全部，`decode_records` 逐帧解，残尾用 `set_len` 截掉并同步）、`append`（编码一帧，写到末尾，同步）。重放是纯函数 `replay`：第一条必须是 `Init`，之后逐条 `apply_edit`（对 `(形状, 放法)` 做 `match`，四个分支；`remove_ids` 遇到不存在的编号报损坏），得到只含编号的 `RecoveredLayout`。
- **锁里放着 manifest**：`state_lock` 从 `Mutex<()>` 改成 `Mutex<Manifest>`。不拿锁就拿不到 manifest、没法追加，"记录的先后与状态的先后一致"由类型保证。冻结也拿这把锁，只是排队。
- **换状态**：`version_edit` 把 SST 换成编号、填上当前的下一个编号；`commit_state` 先 `append`、成功之后才换指针。flush 与四个 `apply_*` 都是同一个模式：拿锁 → `snapshot()` 复制 → 算记录（要在文件被移进新状态之前算） → 原来的换状态代码 → `commit_state`。原来是先拿写锁、在锁里复制，现在写锁留到最后一刻。空表的 flush 不写文件，不记。trivial move 记成"去掉这几个文件、把它们放进下层"。四个 `apply_*` 改为返回 `Result`，调用方多一个 `?`；删旧文件仍在之后。
- **打开**：`LsmStorage::open` → `open_manifest`（三种情况）→ 打开 SST（manifest 里有、目录里没有时把 `NotFound` 换成损坏）→ 分层时 `sorted_by_first_key` → `remove_unlisted_ssts`（先检查编号都小于下一个编号，再删目录里不在列表中的 `数字.sst`，其他文件不动）→ `next_sst_id` 接着记下的值。新的错误种类 `Error::IncompatibleOptions` 表示选项与数据库的形状不符。
- **命令行程序**：不再拒绝已有 SST 的目录；用另一种形状打开时报"选项与数据库不符：数据库的形状是 Levels { max_levels: 4 }，选项要求的是 Tiers"。
- **测试**：编码与解码（任意位置截断、最后一条坏、中间一条坏、不是记录的 JSON、长度是 `u64::MAX`）；文件的新建、追加、截掉残尾后接着追加、中间损坏时不动文件；重放的每种放法与每种损坏；重启后 L0 顺序与数据不变、新编号不重名、没 flush 的数据丢失、孤儿被删而别的文件不动、旧输入残留时删掉的 key 不复活、换形状被拒绝、分层策略之间可以互换、空 manifest、有 SST 没 manifest、记下的 SST 不见了；四组策略参数随机写入与合并后重启，状态与数据相同，再全量合并、重启；leveled 把文件插到层中间之后重启顺序不变；随机多轮测试每一轮都核对"重放 manifest 得到的列表 = 当前状态"；`Db` 层四个线程写入、后台 flush 与合并同时进行，关闭后重开状态相同。变异检查 15 个，第一轮漏了 2 个（随机数据下各层总是被整层替换，测不出"打开时不排序"；合并之后 L0 已空，全量合并什么也不做，测不出"全量合并记错层"），补了测试之后全部被发现。没有测到的：追加失败时状态不变（要能注入写入错误）。
- **测试的时限**：每次换状态多一次 fsync，整套测试从约 9 秒变成约 13 秒；macOS 的 `sync_all` 是 `F_FULLFSYNC`，所有测试一起跑时一次要几十毫秒，要同步几百次的随机测试超过了 5 秒的时限。这类测试改用 [test_util.rs](../engine/src/test_util.rs) 的 `run_with_slow_deadline`（30 秒），卡住时照样超时。
- **Rust 工程**：依赖用 `cargo add -p lsm serde --features derive`、`serde_json`、`crc32fast` 加进引擎这个 crate；`derive` 为什么要单独打开见 Rust 笔记《features 与 cfg 条件编译》。`version_edit` 的参数 `removed: impl Iterator<Item = &'a Arc<SsTable>>` 让调用方直接传 `iter().chain(...)`、`iter().flatten()`、`iter::empty()`，不必先收集成 `Vec`；为什么要写 `'a` 见 Rust 笔记《impl Trait：隐藏具体实现类型》。`Shape` 与 `Placement` 派生了 `Copy`，`match (layout.shape, edit.placement)` 复制出两个值，分支里可以随意修改 `layout`。

## 效果

基准记录：[manifest：吞吐与放大基本不变，写入的最大延迟从几十微秒变成几毫秒](../bench/results/2026-10-20-manifest.md)（接电源，与 L13 之前的同一提交交替运行）。

- **字节可以忽略。** 一次运行（约 1.3 GB 的磁盘写入）只有 60～120 条记录、5～12 KB，写放大不变；读路径没有改动，读的差别在浮动之内。
- **代价在 fsync 的次数与位置。** macOS 的 `sync_all` 是 `F_FULLFSYNC`，这台机器上约 3 毫秒。每次换状态多一次，写完之后等后台的时间多出约"记录数 × 3 毫秒"。
- **最大延迟从几十微秒变成 4～8 毫秒。** 写满 memtable 的那次写入要冻结，冻结要拿 `state_lock`，正好碰上 flush 或合并在锁里 fsync 时就要等它。p99.9 不变：碰上的只是一百万次里的几次。
- **吞吐**：简单分层、tiered、leveled 的随机写入差 0～6%；顺序写入下 leveled 几乎全是 trivial move，原来只改内存，现在每次一个 fsync，低一成多；memtable 1 MB、不合并时冻结最频繁，低三成。临时去掉 manifest 的同步后，吞吐与最大延迟都回到之前的水平，确认了原因。
- 这就是开场时接受的"fsync 在大锁里"的代价，改进方向见交接文档的欠账：把 fsync 移出 `state_lock`（RocksDB 的 `manifest_writers_` 队列，并把几条记录攒成一次 fsync），或者让冻结不必等 manifest。

## 工业对照

**mini-lsm**（[manifest.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/manifest.rs)）：帧的格式相同（8 字节长度、JSON、crc32），中间一条校验和不对时报错，末尾不完整时截掉。记录是任务本身：`Flush(id)`、`NewMemtable(id)`、`Compaction(task, 输出编号)`，重放时调用策略的换状态函数，所以 leveled 的换状态有 `in_recovery` 参数（文件还没打开，先不按首 key 排序）。`add_record` 的第一个参数是 `&MutexGuard<()>`，只是为了让没拿 `state_lock` 的调用编译不过，与本项目把 manifest 放进锁里是同一个目的。

**RocksDB**：

- MANIFEST 是一串 VersionEdit：新加的文件（层号、编号、大小、首末 key、序列号范围）、删掉的文件（层号、编号）、`log_number`（比它小的 WAL 都已落进 SST）、`next_file_number`、`last_sequence`。打开时重放得到当前的 Version。
- `CURRENT` 文件只有一行：当前 MANIFEST 的文件名。MANIFEST 超过 `max_manifest_file_size` 时新开一个，先写一份全量快照，再写临时文件、rename 替换 `CURRENT`、同步目录。
- 写 MANIFEST 时放开数据库的大锁，`manifest_writers_` 队列保证记录的顺序，并把排队的几条攒成一次写入（group commit）。
- 删除旧文件由后台按"已经不被任何 Version 引用"判断（`ObsoleteFiles`），打开时也会扫目录删掉不在 MANIFEST 里的文件。

**LevelDB**：与 RocksDB 相同的 MANIFEST、VersionEdit、`CURRENT`。每次同步 MANIFEST 时顺带同步一次数据库目录，新 SST 的目录项靠这一次落盘。

### 导读：mito2 的 manifest

mito2 的数据放在对象存储上，对象不能追加，所以"日志"是一个个单独的文件。路径都在 GreptimeDB v1.2.1 的 [`src/mito2/src/manifest/`](https://github.com/GreptimeTeam/greptimedb/tree/v1.2.1/src/mito2/src/manifest/) 下。

1. **记录长什么样**：[action.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/manifest/action.rs) 的 `RegionMetaAction`（`Change`、`Edit`、`Remove`、`Truncate` 等）与 `RegionEdit`：`files_to_add`、`files_to_remove` 都是完整的 `FileMeta`（其中有层号、时间范围、大小），另有 `flushed_entry_id`、`flushed_sequence`（WAL 写到哪里已经落进 SST）。一次更新是一个 `RegionMetaActionList`，几条 action 一起生效，相当于本项目的"一次换状态一条记录"。
2. **重放**：同一文件的 `RegionManifestBuilder::apply_edit`。文件按 `file_id` 放进一个 `HashMap`，没有顺序问题：层号与时间范围都在 `FileMeta` 里。被去掉的文件记进 `removed_files` 并带上时间戳，之后由 GC 延迟删除，而不是换状态后立刻删。
3. **文件怎样命名**：[storage.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/manifest/storage.rs) 的 `delta_file`（`{version:020}.json`）、`checkpoint_file`，以及 `LAST_CHECKPOINT_FILE`（`_last_checkpoint`，指向最新的 checkpoint，作用相当于 `CURRENT`）。
4. **打开**：[manager.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/manifest/manager.rs) 的 `RegionManifestManager::open`：先读最新的 checkpoint，从它的下一个版本开始取出全部增量文件，逐个解码、逐条 apply。
5. **更新**：同一文件的 `update`：先 `save` 增量文件，再把 action 用到内存里的 manifest 上，顺序与本项目的"先记、再换"相同。
6. **checkpoint**：[checkpointer.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/manifest/checkpointer.rs)。距上一次 checkpoint 超过 `checkpoint_distance` 个版本（`manifest_checkpoint_distance` 默认 10）时，在后台写一份全量，之后可以删掉更早的增量文件。

与本项目对照：都是增量记法，都先持久化再生效；mito2 每条增量一个文件、有 checkpoint、删除延迟交给 GC，本项目是一个追加的文件、没有 checkpoint、换状态后立刻删。

## 常见错误

- **先写记录、后写文件，或者先删文件、后写记录。** manifest 指向不存在的文件，打开失败或读到半截内容。
- **新 SST 只同步了文件、没同步目录，就写进 manifest。** 断电后记录在、文件名不在，同样悬空。
- **一次合并拆成"加"与"删"两条记录。** 崩溃在两条之间时新旧文件同时有效，丢掉的墓碑盖住的值复活。
- **先换内存状态、再追加记录。** 追加失败时内存与盘上不一致，接着删掉的旧文件下次打开时悬空。
- **追加放在锁外。** 记录的先后与生效的先后可以相反，正确性要靠"任意两条记录可交换"，以后的改动容易打破。
- **fsync 放在状态的写锁里。** 每次换状态，所有读者取快照都要等一次 fsync。
- **打开时按编号推新旧或层号。** 合并的输出编号可能比同时 flush 出的 L0 文件大，内容却更旧。
- **打开时不删孤儿，或者先分配编号再删。** 写了一半的新文件占着一个编号，新 flush 用 `create_new` 建文件时重名失败。
- **把目录里没有 manifest 管理的 SST 也当孤儿删掉。** 别的程序或旧版本的文件被悄悄删除；应当拒绝打开。
- **残尾不截掉就追加。** 新记录接在半条记录后面，下次打开读不到。
- **中间损坏也当作残尾截掉，或者跳过。** 已经执行过的删除成了悬空，或者重放得到错误的状态。
- **长度直接相加。** 长度来自文件，可能接近上限，溢出后绕回一个小数。
- **分层的层在重放时按记录顺序拼接、打开后不排序。** 层内不再按 key 排好，串接游标读错；随机数据下各层常被整层替换，测试不容易发现。
- **用另一种形状打开不检查。** tiered 的 tier 列表被当作固定的层，或者层数变少时文件放不下。

## 资料

- mini-lsm 教程第二周第 5 章：[week2-05-manifest.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week2-05-manifest.md)；代码 [manifest.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/manifest.rs)。
- RocksDB Wiki：MANIFEST；WAL Recovery Modes。
- 存储笔记：《文件格式与元数据校验》、《提交顺序与同步》、《打开与故障恢复》、《写入、同步与错误边界》的“父目录的同步”一节。
- 基准记录：[manifest 的前后对比](../bench/results/2026-10-20-manifest.md)。

返回 [本组目录](README.md)。
