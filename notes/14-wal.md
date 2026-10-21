# WAL

状态：已有正文 · 代码：[wal.rs](../engine/src/wal.rs) 的 `Wal::create`、`Wal::recover`、`Wal::append`、`Wal::sync`、`encode_entry`、`encoded_len`、`decode_entries`、`decode_body`；[mem_table.rs](../engine/src/mem_table.rs) 的 `MemTable::new`、`create_with_wal`、`recover_from_wal`、`put`、`sync_wal`、`insert`；[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `WriteOptions`、`put_opt`、`delete_opt`、`write`、`sync`、`force_freeze_memtable`、`force_flush_next_imm_memtable`、`open`、`recover_memtables`、`list_wal_ids`、`new_memtable`、`wal_path`；[manifest.rs](../engine/src/manifest.rs) 的 `VersionEdit::flushed_memtable_id`、`RecoveredLayout::flushed_memtable_id`、`replay`；[db.rs](../engine/src/db.rs) 的 `put_opt`、`delete_opt`、`sync`；真实进程崩溃的测试 [tests/crash.rs](../engine/tests/crash.rs)

## 位置

[全景](README.md#全景)里的 ①：写入先追加到 WAL，再插入 memtable。重启与崩溃的第二篇。[L13](13-manifest.md) 之后，重启能找回各层的 SST，正常关闭（关闭时把内存里的表全部写成 SST）不丢数据；但进程崩溃时，可写的表与全部冻结的表都在内存里，一起丢掉。写入快于 flush 时冻结的表会排队（[L10](10-simple-leveled-compaction.md) 的基准），一次崩溃丢掉的可能是好几张表。本篇让已经完成的写入在进程崩溃后仍然在；下一篇 [L15](15-write-batch-and-checksums.md) 做写批次（几条写入一起成功或失败）与损坏的细分。

## 与 B+Tree 阶段的对应

| B+Tree（单文件，写时复制） | LSM |
| --- | --- |
| 数据结构就在磁盘上：一次提交走一遍"数据页 → sync → meta → sync" | 数据结构在内存里，SST 要等表写满才一次性生成；单条写入要能恢复，只能在 memtable 之外按到达的顺序再写一份 |
| 一次提交要写好几页，所以把多次写入攒成一个事务 | 一次写入只追加一条几十字节的记录，顺序写，最便宜 |
| meta 页换完之前，新页都不算数 | manifest 记下"哪张表已写成 SST"之前，它的 WAL 都不能删 |
| 打开时选一个有效的 meta，高水位以上的页视为不存在 | 打开时重放 manifest 得到高水位，高水位以上的 WAL 重放，以下的删掉 |

一般原理见 [提交顺序与同步](../../storage/notes/05-storage-recovery/04-commit-order-and-sync.md)、[打开与故障恢复](../../storage/notes/05-storage-recovery/05-open-and-crash-recovery.md)；数据库 WAL 与 Raft 日志的区别见 [[_knowledge/数据库/05-预写日志与复制日志|预写日志与复制日志]]。用 Raft 的话说：日志是事实，memtable 是把日志应用之后的状态机；重放就是从头应用一遍日志。

## 什么时候算写成功：持久化的四级

"写下去"可以停在四个不同的地方，各自扛得住的故障不同。下面的代价是在一台 Apple M5、APFS 上，每条 124 字节，用 C 直接调系统调用量的：

| 写到哪一级 | 每条的代价 | 进程崩溃 | 断电、内核崩溃 |
| --- | --- | --- | --- |
| 进程内的缓冲区（如 `BufWriter`） | 几十纳秒 | 丢 | 丢 |
| 页缓存（每条一次 `write`） | 约 0.97 µs | 不丢 | 丢 |
| `fsync` | 约 25 µs | 不丢 | macOS 上可能丢：`fsync` 不要求磁盘把自己的缓存写下去 |
| `F_FULLFSYNC`（Rust 的 `File::sync_all` 在 macOS 上就是它） | 约 3.8 ms | 不丢 | 不丢 |

另一个数：对没有新内容的文件做 `F_FULLFSYNC` 约 0.8 µs，文件系统发现没有脏数据就直接返回。这个"空同步很便宜"不是所有文件系统都保证的。

引擎一次写入本身（插进跳表）约 0.25 µs。所以：每条一次 `write`，单线程写入就慢好几倍；每条一次 `F_FULLFSYNC`，只剩每秒两三百次。

### 同步策略

| 做法 | 保证 | 谁这样做 |
| --- | --- | --- |
| 只写进缓冲区，调用方显式 `sync` | `sync` 返回之前完成的写入不丢，之后的连进程崩溃都可能丢 | mini-lsm（`BufWriter`） |
| 每次写入都 `write` 进页缓存，不同步 | 进程崩溃不丢，断电丢最近的 | RocksDB 默认（`WriteOptions::sync = false`） |
| 每次写入都同步 | 断电也不丢 | RocksDB 的 `sync = true` |
| 定时同步 | 断电最多丢一个周期 | Redis 的 `appendfsync everysec` |

每次都同步的代价靠 **group commit** 摊薄：同时到达的几个写者排成一组，由领头的写一次、同步一次，所有人一起返回。单线程时没有可以一起等的人，摊不薄。

数据的重要性不同：订单不能丢，访问计数丢几条可以接受。所以把选择交给每次写入（RocksDB 的 `WriteOptions`），再提供一个显式的 `sync()`：先做一批不同步的写入，最后同步一次。

要扛住进程崩溃，底线是每次写入都至少 `write` 进页缓存；只放在进程里的缓冲区，WAL 对进程崩溃就没有用了。

### 混用时的前缀保证

> **一次同步成功，它之前完成的全部写入都要落盘。**

只同步当前可写表的 WAL 不够：

```
写 x（不同步）→ 进了 5.wal 的页缓存
冻结：换上表 6（6.wal）
写 y（同步）  → 只同步了 6.wal，返回成功
断电 → y 在，比它早的 x 没了
```

同一个文件里不会这样（同步 y 时 x 一起落盘，文件按前缀落盘），跨文件就会。重启后的状态是"y 在、x 不在"，历史上从没出现过。要堵住它，更旧的 WAL 也要同步过，有两种排法：

| 排法 | 冻结时 | 同步时 | 谁这样做 |
| --- | --- | --- | --- |
| 同步时把还在内存里的每一张表的 WAL 都同步一遍 | 不同步 | 同步全部；已经同步过的、没有新内容的文件很快 | RocksDB（`logs_` 是"还没同步完的 WAL 加当前的 WAL"，同步时逐个同步，同步完的从中拿掉） |
| 冻结时同步旧表的 WAL | 换完表之后、仍持有让冻结排队的锁时同步旧 WAL | `sync()` 先拿这把锁，等正在进行的冻结做完，再只同步当前的表 | mini-lsm |

两种都对，差别在代价落在哪里：前一种冻结不变慢；后一种同步只碰一个文件，但冻结多一次同步，`sync()` 还要和冻结、flush 抢同一把锁。

错的排法是"冻结时先同步旧 WAL、再换表"：同步完到换表之间，别的线程还在往旧表里写，这几条没同步；换表后有人在新表做一次同步写入并返回，断电后又是新的在、旧的没了。把同步挪到换表之后（此时已经没有人往旧表里写）就没有这个空档。

## WAL 与 memtable

每张 memtable 一个 WAL 文件，文件名是表的编号（`00007.wal`）。一个 WAL 的一生：

```
建出来：打开时为新表建，或者冻结时为换上的新表建
  │  这张表可写：每次写入往末尾追加
  ▼
冻结：不再追加；换上的新表有自己的新 WAL
  │  只剩"可能被同步"
  ▼
flush：表写成 SST、同步 → manifest 记下"它已写成 SST"并同步 → 删掉这个 WAL
```

所以 WAL 只管还在内存里的数据：数据进了 SST 之后，后面怎么合并都与 WAL 无关。

也可以所有表共用一个连续的日志、按段切分（RocksDB 多个 column family 共用一个 WAL；mito2 所有 region 共用一个日志库，按条目编号作废）。每表一个文件的好处是作废就是删文件，判断"哪些还有用"只看文件编号。

### 先写 WAL，再插入表

反过来，读者可能已经读到这个值，接着崩溃、值没了：别人看见过的写入凭空消失，比"写入失败"更糟。"先写完"指写到所选的那一级；选了只写进缓冲区，读到的值崩溃后仍可能没了，这是选弱一级保证时接受的代价，真正的分界线是同步返回的那一刻。

还有两条要守住：

> **WAL 里的先后 = 插入表的先后**（至少对同一个 key）。

追加 WAL 与插入表是两步。两个线程同时写同一个 key：

```
A：追加 WAL(k=1)
B：追加 WAL(k=2)
B：插入表(k=2)
A：插入表(k=1)
```

崩溃前读到 1，重放 WAL 之后是 2，重启前后不一致。两种办法：

- **一把锁同时包住两步。** 追加本来就要排队（只有一个文件），锁里多出来的只是一次插入。代价是同步写入如果也在锁里，其他写者跟着等，这就是 group commit 要解决的。
- **每次写入一个序号**，表按序号决定谁新，插入的先后不再重要（RocksDB、mito2；本项目的 MVCC 阶段）。

> **两步落在同一张表上。**

两步之间如果发生了冻结，写入进了旧表的 WAL、却插进了新表，重启后它在旧表里，比新表的写入还旧。写入全程持有状态的读锁、冻结要拿写锁时，两步拿到的一定是同一张表；mini-lsm 把 WAL 放进 `MemTable`，由 `put` 自己先写 WAL 再插入，也是为了这一条。

## 作废：manifest 怎样记住哪些 WAL 还有用

| 记法 | 做法 | 谁这样做 |
| --- | --- | --- |
| 每张新表一条记录 | 建表时记 `NewMemtable(id)`，flush 时记"表 id 已写成 SST" | mini-lsm |
| 高水位 | flush 的记录带上"写成 SST 的表的编号 H"，编号不超过 H 的 WAL 都没用了 | RocksDB 的 `log_number`、mito2 的 `flushed_entry_id` |

高水位成立的前提是 **flush 从最旧的表开始，一张一张做**：表的编号按冻结的先后递增，先 flush 的总是编号小的，"不超过 H 的都已写成 SST"才成立。flush 的顺序可以打乱时（先 flush 表 9、表 7 还在内存里），单个高水位不够，要记"还活着的最小编号"（RocksDB 多个 column family 各自 flush 时就这样，取各自 `log_number` 的最小值），或者每张表一条记录。

高水位不记新表：冻结时不用写 manifest，省掉一次 manifest 的同步。代价是 manifest 不知道全部编号，打开时要从目录里补上（见 [编号](#编号)）。

几条细则：

- **先记高水位、后删文件**，与 [L13 的"宁可泄漏，不能悬空"](13-manifest.md#顺序不变量与崩溃点)相同。删之前崩溃，重启时它在高水位以下，同样被删掉；反过来，删了文件、记录还没写下时崩溃，这张表的写入就丢了。
- **空表也要记。** 表是空的、没有写出 SST，但它的 WAL 也要作废，所以照样记一条只带高水位的记录。不记的话，这个空文件每次打开都被重放一遍，永远删不掉。
- **高水位只会往上走。** 重放时发现它倒退或重复，说明记下的历史自相矛盾（顺序被打乱、文件被拼接、两个进程同时写同一个目录）；不检查的话，已经写成 SST 的表的 WAL 会被重放，旧值盖住更新的 SST。

## 编号

表的编号与 SST 的编号从同一个计数器取：建表时取一个（WAL 用它起名），flush 写 SST 时再取一个。也可以让 SST 沿用表的编号（mini-lsm：`6.wal` flush 之后就是 `6.sst`）：

| | flush 时新取一个号 | 沿用表的编号 |
| --- | --- | --- |
| 目录里看得出对应关系 | 看不出，要查 manifest 的高水位记录 | 一眼对上 |
| SST 的编号与创建的先后 | 一致 | 不一定：表 6 还在内存里时合并写出 7、8，表 6 后 flush 却得到 6 |
| flush 的写文件代码 | 与合并共用 | 要能指定编号 |

"编号与创建先后一致"在挑"最旧的文件"时有用（leveled 挑编号最小的文件）。新取号也让 SST 的编号与 flush 的顺序无关；但改成乱序 flush 时，高水位仍要换成上一节的另一种记法。

打开时下一个编号要取两者中较大的：

> **下一个编号 = max(manifest 记下的下一个编号, 还在的 WAL 里最大的编号 + 1)**

两边各知道一部分编号：manifest 记着 flush 与合并分给 SST 的（随记录写进去），还在的 WAL 是冻结时分给表的（不进 manifest）。两个方向都会出现：

| 情况 | manifest 的下一个编号 | 最新的 WAL + 1 | 只看一边的后果 |
| --- | --- | --- | --- |
| 一直没 flush 就崩溃：表 1、2、3 都还在 | 1（只有第一条记录） | 4 | 只看 manifest：新表又去建 `00001.wal`，文件已存在，打不开 |
| flush 之后崩溃：表 6 还在，表 5 写成了 SST 7，记录里下一个编号 8 | 8 | 7 | 只看 WAL：计数器从 7 往后分，发出已经用过的 SST 编号 |
| 正常关闭，没有 WAL | 8 | — | 只能看 manifest |

新表的编号还必须**大于**全部重放出的表，不只是不重名：它比某张冻结的表小的话，之后 flush 它时高水位倒退；高水位已经越过它的话，它的 WAL 下次打开会被当作已经写成 SST 而删掉。RocksDB 恢复 WAL 时也做同一件事（`MarkFileNumberUsed`，注释说上一次运行分配了这个 log 编号之后可能没再写 MANIFEST）。

## 打开时

1. 重放 manifest，得到各层的 SST、下一个编号、高水位。
2. 删掉不在 manifest 里的 SST（L13）。
3. 列出目录里的 `.wal`，按编号排序：不超过高水位的删掉；其余的从旧到新逐个重放，各自重建成一张表。
4. 重建的表作为冻结的表，新的在前；再换上一张编号更大的空表（开了 WAL 时连同它的新 WAL）。
5. 后台照常 flush 这些表，记下高水位、删掉 WAL。

重放时只插入表，不再追加 WAL（同一条写入写两遍）。排序不能省：`read_dir` 给出的顺序没有保证，而冻结的表要求新的在前，顺序错了会读到旧值。

### 重建回内存，还是当场 flush

| | 重建成冻结的表（本项目、mini-lsm） | 打开时直接写成 SST（RocksDB 默认，`avoid_flush_during_recovery = false`） |
| --- | --- | --- |
| 打开之后 | 与崩溃前一样：几张冻结的表等后台 flush | 数据都在 L0，旧 WAL 已删 |
| 打开的耗时 | 只读 WAL、插进内存 | 要写 SST、同步、写 manifest |
| 代价 | 这段时间里内存与 WAL 都还占着 | 可能产生很小的 SST（RocksDB 的注释专门提到） |

两种都先经过 memtable：WAL 按写入的先后排、同一个 key 可能出现多次，SST 要求按 key 排好、每个 key 一次，这一步排序与去重正是 memtable 做的。

两种都可以反复做：无论在哪一步崩溃，"已经记进高水位的不重放，没记的一定重放"都成立，再打开一次得到同样的结果。

为什么不让最新的那张重建出的表接着当可写的表：它崩溃前就是可写的表，可能已经接近上限；所有重建的表走同一条路（冻结 → flush → 记高水位 → 删 WAL），打开的逻辑最简单。

几种边界情况：

| 目录里 | 处理 |
| --- | --- |
| 有 WAL 却没有 manifest | 拒绝打开，文件不动：不是这个引擎按 manifest 管理的文件 |
| 重放出一张空表 | 照样放进冻结列表：它的 WAL 要靠 flush 记下高水位才能删 |
| WAL 中间损坏 | 拒绝打开，文件不动 |
| 这次用"不开 WAL"的选项打开，目录里有上次留下的 WAL | 照样重放，数据不丢；新表没有 WAL |

## 文件格式

```
一条： 长度（4 字节）│ 内容（"长度"个字节）│ 内容的 crc32（4 字节）
内容： key 的长度（2 字节）│ key │ value 的长度（2 字节）│ value
```

value 为空是墓碑。分两层：外层与 manifest 的帧相同，只管"这一段完整、没被改过"，残尾的规则也相同（剩下的不够一条：截掉；最后一条校验和不对：截掉；中间一条不对：报错）；里层只管 key 与 value 怎么排，校验和对得上、里层却解不出来（长度对不上、有多余的字节）是损坏。一条记录要放多对 key 与 value（写批次）时只改里层。外层的长度对 16 字节 key、100 字节 value 来说多占 4 字节：一条 128 字节，用户数据 116 字节。

manifest 用 JSON 是因为一次运行只有几十条、要能读懂；WAL 一次运行一百万条，用二进制。key 与 value 的长度字段是 2 字节，与数据块相同，超长的写入在入口就返回错误，到不了编码这一步。

整条记录拼好之后用一次 `write` 写出。顺带的一个观察：进程被 SIGKILL 时，几十字节的一次 `write` 不会只写进去一半（它不会被信号打断在中途），所以**进程崩溃几乎不留残尾**；残尾主要来自断电（页缓存里的页落了一部分），或者一次写得很大、内核分几次写完。WAL 写到一半出错（磁盘满）则可能留下半条，之后再追加，它就成了中间的损坏，这是"写入出错后要停止写入"的又一个理由。

## 本项目的做法

- **模块**：[wal.rs](../engine/src/wal.rs)。`Wal` 只有一个打开的文件，自己不带锁：`create`（`create_new`，同步父目录）、`recover`（读出全部，`decode_entries` 逐帧解，残尾 `set_len` 截掉并同步）、`append`（`encode_entry` 拼好一帧，`(&File).write_all` 一次写出，不同步）、`sync`（`sync_all`）。`encoded_len` 算一条的长度，给计数器用。
- **memtable**：`MemTable` 多了 `id`、`wal: Option<Wal>`、`write_lock`。`put` 返回 `Result`：有 WAL 时在 `write_lock` 里先 `append` 再插入，没有 WAL 时直接插入、不拿锁。`sync_wal` 不拿锁，同步期间别的线程照常写。`recover_from_wal` 用 `insert`（只插表、累加大小，不碰 WAL）把记录插回去，再把打开着的 WAL 放进表里。
- **写入**：`put`、`delete` 返回 `Result<bool>`（有没有冻结一张表），`put_opt`、`delete_opt` 多一个 `WriteOptions { sync }`。`write` 的顺序：检查长度（超过 65535 字节返回 `Error::InvalidArgument`，什么都不做）→ 读锁里写入表 → 计数（用户字节、WAL 字节）→ 需要时 `sync` → 试着冻结。`sync` 取一份快照，把可写的表与每一张冻结的表的 WAL 都同步一遍（RocksDB 的排法）。空 key、空 value 仍是 panic：那是调用方违反约定，见 [L07](07-write-path.md#写入为什么暂时不返回错误)。
- **同步在读锁外**：`sync` 里的 `snapshot` 还要拿一次读锁。同一个线程持着读锁再拿读锁，遇上排队的写者会死锁：std 的 `RwLock` 在 macOS 与 Linux 上都是写者优先，有人在等写锁时新的读请求也要排队。见 [[rust-learning/notes/09-concurrency/06-RwLock-与读写访问|RwLock 与读写访问]]。
- **冻结**：`force_freeze_memtable` 返回 `Result`：从计数器取号，开了 WAL 时建新表与它的 WAL（连同目录的同步），都在拿写锁之前；建不成时状态不变。不同步旧表的 WAL。
- **flush**：空表且没有 WAL 时只换状态；其余都记一条，`added` 是 `sst.into_iter().collect()`（`Some` 得到一个元素、`None` 得到空），`flushed_memtable_id` 是这张表的编号；`commit_state` 成功、放开 `state_lock` 之后删 WAL。
- **manifest**：`VersionEdit` 加 `flushed_memtable_id: Option<usize>`，带 `#[serde(default, skip_serializing_if = "Option::is_none")]`：L14 之前的记录没有这一项，读出来是 `None`；合并的记录不写这一项，与原来的格式一样。`replay` 取最后一条 flush 的记录作高水位，倒退或重复时报损坏。
- **打开**：`open_manifest`（有 SST 或 WAL 却没有 manifest 时拒绝）→ 打开 SST、删孤儿 → `recover_memtables`（`list_wal_ids` 排序，高水位以下删掉，其余 `recover_from_wal`，最后倒过来）→ 下一个编号取两者中较大的 → 新表。
- **对外**：`Db` 的 `put`、`delete` 返回 `Result<()>`，多了 `put_opt`、`delete_opt`、`sync`。`close` 仍把全部表写成 SST，所以正常关闭之后下次打开不用重放。命令行程序默认开 WAL，`status` 列出 `.wal`，新命令 `sync`、`crash`（`abort` 结束进程，不关闭数据库）。基准驱动加 `--wal off|on|sync`，报告单列 WAL 的字节。
- **测试**：编码的逐字节格式、最长的 key 与 value、从每个位置截断、最后一条与中间一条坏、内容长度不对、很大的长度、截掉残尾后接着追加；memtable 带与不带 WAL；冻结换 WAL、按编号接起来正好是全部写入、编号不与 SST 重名、超长被拒且什么都没写、计数器与文件大小一致；flush 记下高水位并删 WAL、空表也作废 WAL；不 flush 就重启（全部重建、新旧顺序不变、新编号越过还在的 WAL）、重建的表照常 flush、高水位以下与正好等于高水位的 WAL 不重放、残尾、WAL 中间损坏、关掉 WAL 重开、有 WAL 没 manifest；`Db` 不 close 就释放后重开。
- **并发的测试怎样才测得出**：两个线程同时写同一个 key 时，"追加完、还没插入"的间隙只有约 100 ns，去掉 `write_lock` 之后并发测试照样通过。改成用 `Barrier` 让几个线程对齐后同时写同一个 key，并在测试里打开一个只在 `cfg(test)` 下存在的开关（每次追加之后停 0～50 µs，长短由记录内容的校验和决定），去掉锁的版本每次都失败。"持着读锁调用 `sync`"的死锁要求另一个线程正好在几微秒的窗口里开始排队，测试碰不上，只能靠代码审查。
- **真实进程崩溃**（[tests/crash.rs](../engine/tests/crash.rs)）：测试程序把自己作为子进程再启动一次，子进程开着 WAL 不停写入（memtable 2 KB、后台合并，几千次写入里有上百次冻结、flush 与合并），每完成一次打印序号；父进程读到足够多的序号后 SIGKILL，再打开同一个目录，检查数据恰好是"前 M 次写入"的结果、M 不小于读到的最后一个序号：完成了的写入一条不少，也没有"后面的在、前面的没了"的空洞。不同步、每次同步、连续崩溃三轮都测。断电没有测。

## 效果

基准记录：[WAL 的代价](../bench/results/2026-10-21-wal.md)（电池供电，`--wal off` 与 `on` 交替运行）。

- **不合并时看 WAL 本身**：写入从每秒 315 万次降到 56 万次，p50 从 0.25 µs 变成 1.33 µs，多出的正好是一次 `write`；写放大从 0.97 变成 2.08，多出的 1.10 正好是 128 ÷ 116；读不变。
- **有合并时形状也变了**：简单分层的写放大从 5.63 变成 7.34，其中只有 1.10 是 WAL。写入慢下来之后合并跟得上了（写完之后等后台从 1.19 秒降到 0.16 秒），数据不再最后整批并到最底层，而是一层层往下推、每层重写一遍；停下时中间层还有文件，读也慢了一些，空间放大从约 1.0 变成 1.3～1.6。比较写入路径本身的代价，要用不合并的那组。
- **每次都同步**：每秒 362 次，p50 2.99 ms，比只 `write` 慢约 1650 倍。
- **冻结又慢了一点**：建新 WAL 要同步一次目录，在 `state_lock` 里，写入的最大延迟多了约 1 毫秒。

## 工业对照

**mini-lsm**（[wal.rs](../../references/mini-lsm/mini-lsm/src/wal.rs)、[lsm_storage.rs](../../references/mini-lsm/mini-lsm/src/lsm_storage.rs)）：每张表一个 `<id>.wal`，记录是 `key_len | key | value_len | value | crc32`（没有外层的长度）；`Wal` 里是 `Mutex<BufWriter<File>>`，`put` 只写进缓冲区，`sync` 才 `flush` 加 `sync_all`，所以没调用 `sync` 时进程崩溃也会丢。manifest 每建一张表记一条 `NewMemtable(id)`，flush 之后 SST 沿用表的编号；冻结时换完表、放开写锁之后同步旧表的 WAL，`sync()` 先拿 `state_lock` 再只同步当前的表。打开时重放的表全部放进冻结列表，跳过空的；有 WAL 时 `close` 只同步、不 flush。

**RocksDB**：

- `WriteOptions::sync` 每次写入自己选；`disableWAL` 可以整个不写；`manual_wal_flush` 时写入只进 RocksDB 自己的缓冲区，调用方 `FlushWAL` 才写进页缓存。
- group commit：写者排进写线程的队列，领头的把一组写批次合成一条写进 WAL，需要同步时同步一次，同组的写者一起返回。
- 同步时遍历 `logs_`（还没同步完的 WAL 与当前的 WAL）逐个同步；第一次要求同步时顺带同步 WAL 所在的目录（`need_log_dir_sync`），从不同步的用户不付这次代价。
- manifest 的 `log_number`：比它小的 WAL 都已落进 SST。恢复时跳过它们，并用 `MarkFileNumberUsed` 让编号计数器越过还在的 WAL。
- `avoid_flush_during_recovery` 默认关：打开时把重放的数据直接 flush 成 SST。`WALRecoveryMode` 决定遇到损坏怎么办：`kTolerateCorruptedTailRecords`（末尾容忍）、`kAbsoluteConsistency`（任何损坏都报错）、`kPointInTimeRecovery`（停在第一处损坏，之后的都不要）、`kSkipAnyCorruptedRecords`。
- WAL 的每条记录是一个写批次，带序号；memtable 按序号决定新旧，插入的先后不影响结果。

**LevelDB**：一张表一个 log 文件；换表时关掉旧 log，不同步它；带 `sync` 的写入只同步当前的 log。所以它的 `sync` 只保证这一次写入本身，不保证更早的、在旧 log 里还没同步的写入，也就是上面"混用时的前缀保证"那个空档。

### 导读：mito2 的 WAL

mito2 不自己写 WAL 文件：所有 region 共用一个日志库（单机时是 raft-engine，分布式时是 Kafka），每条记录有一个递增的条目编号（entry id）。路径都在 `references/greptimedb/src/mito2/src/` 下。

1. **写入**：[wal.rs](../../references/greptimedb/src/mito2/src/wal.rs) 的 `WalWriter`。`add_entry` 把一个 region 的一批写入编码成一条，带上 region 与条目编号，先放进 `entries`；`write_to_wal` 把攒下的几条用 `append_batch` 一次交给日志库。一次写入请求里几个 region 的记录一起写，相当于 group commit。对应本项目的 `Wal::append`，差别是它按批写、本项目每条写一次。
2. **flush 记下高水位**：[flush.rs](../../references/greptimedb/src/mito2/src/flush.rs) 构造 `RegionEdit` 时，`flushed_entry_id` 取这次 flush 所用的版本数据里记的最后一个条目编号（`version_data.last_entry_id`），`flushed_sequence` 取已提交的最后一个序号，然后 `update_manifest` 写进 manifest。对应本项目 `force_flush_next_imm_memtable` 里的 `edit.flushed_memtable_id = Some(memtable.id())` 与 `commit_state`。差别：mito2 一次 flush 这个 region 的全部冻结表，所以高水位直接是"写到哪一条了"；本项目一次一张，高水位是表的编号。
3. **作废**：[worker/handle_flush.rs](../../references/greptimedb/src/mito2/src/worker/handle_flush.rs) 的 `handle_flush_finished`：先把 edit 用到内存里的版本上，再调用 `wal.obsolete(region_id, flushed_entry_id, …)`，让日志库把这个 region 编号不超过它的条目标为可删。顺序与本项目的"先记高水位、后删 WAL"相同；它删的是条目而不是文件，空间由日志库回收。
4. **重放**：[region/opener.rs](../../references/greptimedb/src/mito2/src/region/opener.rs) 的 `replay_memtable`。从 `flushed_entry_id + 1` 开始读这个 region 的条目；读到不超过 `flushed_entry_id` 的旧条目时，按选项报错或者告警跳过；每条里的写入照常写进 memtable，并沿用 WAL 里记的序号（"respect the sequence in WAL during replay"）。对应本项目的 `recover_memtables` 与 `recover_from_wal`：先按高水位过滤，再插回表。可以先跳过的：`bulk_entries` 与补列的那一段（时序数据的批量写入与表结构变更），以及最后的计时与日志。
5. **重放进哪里**：它把重放的写入写进当前可写的 memtable（`region_write_ctx.write_memtable()`），不另建冻结的表；靠序号决定新旧，所以不用关心重放出来的表排在哪里。

与本项目对照：都是"manifest 记高水位，打开时重放高水位之上的部分，先记后删"。差别来自共用的日志库与序号：作废是按条目编号截断而不是删文件；并发写入的先后靠序号而不是锁；重放直接写进可写的表。

## 常见错误

- **只写进进程里的缓冲区就返回成功。** 进程一崩溃，缓冲区里的写入就没了，WAL 对进程崩溃没有作用。
- **只同步当前表的 WAL。** 更旧的、冻结的表的 WAL 里还有没同步的写入，断电后"新的在、旧的没了"。
- **冻结时先同步旧 WAL、再换表。** 两步之间别的线程还在往旧表里写，这几条没同步。
- **先插入表、再写 WAL。** 读者看到的值崩溃后消失。
- **追加 WAL 与插入表之间没有锁、也没有序号。** 并发写同一个 key 时 WAL 的先后与表里的先后不同，重启前后读到不同的值。这种交错很少发生，普通的并发测试测不出来。
- **持着状态的读锁去同步或调用会再拿读锁的函数。** 写者优先的读写锁上，同一个线程再拿一次读锁会和排队的写者互相等；同步几毫秒，也会让冻结一直等。
- **先删 WAL、再记高水位。** 中间崩溃时这张表的写入丢了。
- **空表 flush 时不记高水位。** 它的 WAL 永远删不掉，每次打开都重放。
- **乱序 flush 却用单个高水位。** 还在内存里的表的 WAL 被当作已写成 SST 删掉。
- **重放时用 `put`。** 同一条写入又追加一遍 WAL。
- **重放不排序，或者冻结列表不倒过来。** 新旧颠倒，读到旧值。
- **高水位是 `None` 时一个 WAL 都不重放。** `None` 的意思是还没 flush 过，全部都要重放。
- **新表的编号只看 manifest，或者只看 WAL。** 两个方向都可能撞号。
- **重放旧表时绕过了大小的累加。** 重建的表大小是 0，按大小统计内存时算错。

## 资料

- mini-lsm 教程第二周第 6 章：[week2-06-wal.md](../../references/mini-lsm/mini-lsm-book/src/week2-06-wal.md)；代码 [wal.rs](../../references/mini-lsm/mini-lsm/src/wal.rs)。
- RocksDB Wiki：Write Ahead Log、WAL Recovery Modes、WAL Performance。源码（v9.10.0）：`db/db_impl/db_impl_write.cc` 的 `WriteToWAL`，`db/db_impl/db_impl_open.cc` 的 `RecoverLogFiles`，`include/rocksdb/options.h` 的 `avoid_flush_during_recovery`。
- 存储笔记：[提交顺序与同步](../../storage/notes/05-storage-recovery/04-commit-order-and-sync.md)、[打开与故障恢复](../../storage/notes/05-storage-recovery/05-open-and-crash-recovery.md)、[父目录的同步](../../storage/notes/01-file-persistence/04-write-sync-and-errors.md#父目录的同步)。
- 基准记录：[WAL 的代价](../bench/results/2026-10-21-wal.md)。

返回 [本组目录](README.md)。
