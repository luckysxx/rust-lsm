# 写批次与校验和

状态：已有正文 · 代码：[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `WriteBatchRecord`、`write_batch`、`write_batch_opt`、`write`、`sync_after_write`、`sync`；[mem_table.rs](../engine/src/mem_table.rs) 的 `put_batch`、`sync_wal`、`sync_wal_to`；[wal.rs](../engine/src/wal.rs) 的 `Wal::append`、`Wal::sync_to`、`Wal::sync`、`encode_batch`、`encoded_len`、`decode_batches`、`decode_body`；[table.rs](../engine/src/table.rs) 的 `verify_checksum`、`has_checksums`、`SsTable::open`、`read_block_from_file`；[table/builder.rs](../engine/src/table/builder.rs) 的 `append_checksum`、`finish_block`、`build`；[db.rs](../engine/src/db.rs) 的 `write_batch`、`write_batch_opt`；[stats.rs](../engine/src/stats.rs) 的 `wal_syncs`

## 位置

[全景](README.md#全景)里的 ①（写入）与 ③（SST 文件）。重启与崩溃的第三篇，mini-lsm 第二周的最后一章。[L13](13-manifest.md) 让文件列表在重启后能恢复，[L14](14-wal.md) 让内存里的写入在进程崩溃后能恢复。本篇做三件事：

- **写批次**：写入的单位从一对 key 与 value 变成一批，崩溃之后一批要么全在、要么全不在。
- **group commit**：几个线程同时做同步写入时共用 fsync。
- **SST 的校验和**：WAL 与 manifest 早就有 crc32，SST 的数据块、索引、filter 补上。

第三周的 MVCC 会给每一批分配一个时间戳；本篇留下的几个问题（读者看到半批、覆盖写的空档、不同步写入的锁竞争）都要靠它解决。

## 写批次

### 两种原子性

一个操作常常要改几个 key（转账改两个账户；事务提交时把一堆写入一次交给引擎）。只有单条的 `put` 时，崩溃可能留下前一半。写批次要的保证分两层：

| | 意思 | 本篇 |
| --- | --- | --- |
| 崩溃原子 | 重启之后整批都在，或者整批都不在 | 做到：整批是 WAL 里的一条记录 |
| 可见原子 | 别的线程读的时候，不会看到半批 | 没做：要等序号（时间戳） |

崩溃原子用的是存储笔记里"单一发布点"的办法（[原子写批次](../../storage/notes/05-storage-recovery/06-atomic-write-batches.md)）：整批编码成一条记录，外层帧的长度与校验和保证一条记录要么完整、要么作为残尾被截掉，不需要新的机制。拆成几条记录就不行：崩溃时可能只写进了前几条，重放出半批。

可见原子要等第三周：每批分一个时间戳，整批插完才推进"已发布的时间戳"，读者只看已发布的部分。mini-lsm 这一章也明确只做接口，不保证可见原子。

还有一条现成就满足的：**一批不会跨两个 WAL 文件**。`write` 全程持有 `state` 的读锁，冻结要写锁，所以整批追加进哪张表的 WAL，就整批插入哪张表；一批即使超过 memtable 的上限，也是写完才冻结。

### 文件格式：一批是一条记录

```
一条： 长度（4 字节）│ 内容（"长度"个字节）│ 内容的 crc32（4 字节）
内容： 第 0 对 │ 第 1 对 │ …          （至少一对）
一对： key 的长度（2 字节）│ key │ value 的长度（2 字节）│ value
```

外层不变，只改里层。里层有两种排法：

| | 排法 | 旧 WAL（L14 的单条记录） |
| --- | --- | --- |
| 甲 | 数量（4 字节）│ 第 0 对 │ 第 1 对 │ … | 读不了：旧记录开头是 key 的长度，会被当成数量 |
| 乙 | 第 0 对 │ 第 1 对 │ …，解到正好用完 | 正好是只有一对的批次，字节完全相同 |

乙不需要数量：外层的长度已经划出了一批的边界。解码时一对一对往下解，最后一对的字节不够、或者剩下零星几个字节凑不成一对，都是损坏；内容为空也是损坏（空批次不写记录）。

甲开头那块地方的真正用处是**批次头**：放整批共用的信息。RocksDB 的 `WriteBatch` 开头是 8 字节的起始序号加 4 字节的条数：整批共用一个起始序号，第 i 条的序号是"起始序号 + i"，要知道条数才能推出这一批用到了哪个序号。mini-lsm 第三周把时间戳跟在每个 key 后面，不需要批次头。以后真要批次头，可以在帧里加一个类型字节区分新旧格式。

### 跳表没有批次写

`MemTable::put_batch` 在 `write_lock` 里整批追加一次 WAL，再一对一对插入跳表：

```rust
let _guard = self.write_lock.lock().unwrap();
let wal_end = wal.append(batch)?;      // 整批：一条记录、一次 write
for &(key, value) in batch {
    self.insert(key, value);           // 跳表：一对一对插入
}
```

crossbeam 的 `SkipMap` 没有一次插入多对的接口，工业实现也没有：LevelDB 与 RocksDB 把批次写进 memtable 时，同样是遍历批次、一条一条调用 memtable 的 `Add`（`WriteBatchInternal::InsertInto`）。**一批的原子性不是跳表给的**，崩溃原子来自 WAL，可见原子来自序号。

crossbeam 的一次插入（`base.rs` 的 `insert_internal`）分三步：从最高层往下查出每一层的前驱与后继；建一个随机高度的节点，在第 0 层用一次 CAS 把它挂上；再把上面几层一层层挂上。**第 0 层的 CAS 成功就是这一对"发生"的时刻**，从这一刻起读者能看到它；上面几层只让以后查得更快。

所以一批插到一半时：

- 点查先查 a、再查 z 的读者，可能看到"a 是新的、z 是旧的"。
- 扫描可能看到 z 却看不到 a，尽管 a 先插入：游标在 a 被插入之前已经走过了 a 的位置，等它走到 z 时 z 已经插好了。读者看到的半批**不一定是批次的前缀**。

### 不开 WAL 时也要排队

L14 让没有 WAL 的写入不拿 `write_lock`，几个线程可以同时插入。对单条写入这没问题；对多对的批次，两批同时写同几个 key 会一对一对交错：`[a=x, b=x]` 与 `[a=y, b=y]` 写完之后可能是 `a=y, b=x`。这不只是"读到半批"，而是**写完之后**的状态也不是任何一种先后次序的结果。所以没有 WAL 时，多于一对的批次也拿 `write_lock`，单条写入仍然不排队。

### 覆盖写的空档（已知问题）

crossbeam 的 `insert` 遇到已有的 key，先给旧节点打删除标记，再重新查找位置、挂上新节点，中间有一段时间这个 key 在表里不存在。单看跳表只是"查不到"；放进 LSM 就成了 bug：memtable 查不到时，`get` 接着往冻结的表与 SST 里找，于是读到**更旧的值**。

实测：SST 里 k = old，memtable 里 k = new，一个线程反复覆盖写 k = new，另一个线程反复 `get`，33 万次读里 11 万次读到 old；单看跳表，反复覆盖同一个 key 时九成的读找不到它。读者已经读到过 new，再读又变回 old。这从 L02 用 `SkipMap::insert` 起就存在，批次让"同一个 key 写多次"更常见。

RocksDB 与 LevelDB 的 memtable 从不替换节点：key 带着序号，每次写入都是一个新节点，读的时候取序号最大的那个。第三周的 MVCC 给 key 加上时间戳，就自然变成这样。在那之前也可以把 value 换成能原地替换的格子（例如 `ArcSwap`），覆盖时不删节点。

## group commit

### 问题

L14 的同步写入，每个写者都自己调一次 `sync_all`（macOS 上是 `F_FULLFSYNC`，约 3～4 ms）。线程多了，这些同步排成一列：

| 线程数 | 每秒次数 | p50 |
| --- | --- | --- |
| 1 | 357 | 2.99 ms |
| 4 | 309 | 12.96 ms |
| 8 | 291 | 27.98 ms |

吞吐不涨，延迟按线程数成倍增加。但 fsync 刷的是**整个文件**里还没落盘的字节，不只是调用者自己那几十个字节。第 2 个写者排队时，它的记录早已写进页缓存，前面那次 fsync 只要是在它追加之后才开始的，就已经把它带下去了，它自己那一次是白做的。group commit 就是让一次 fsync 覆盖多个写者。

### 三种结构

| | 做法 | 例子 | 每次写入（包括不同步的）多付的代价 |
| --- | --- | --- | --- |
| A | 写者排队，队首当 leader：把此刻排着的批次合成一条，追加一次、同步一次、插入 memtable，再唤醒其余写者 | LevelDB `DBImpl::Write` 与 `BuildBatchGroup`；RocksDB `WriteThread` | 拿一次队列的锁；不是 leader 的要等唤醒 |
| B | 专门的写线程守着通道：取出此刻排着的全部请求，一起追加、一起同步，再逐个回复 | BadgerDB；mito2 的 region worker | 每次都要交接给写线程再等回复 |
| C | 追加照旧，只合并 fsync：要同步的写者先看自己的记录是不是已经被别人的 fsync 带下去了 | PostgreSQL 提交时刷 WAL（`XLogFlush`） | 不变 |

A 里后来的写者不会并进正在进行的那次写入：leader 在锁里收好一组就放开锁去写、去同步，这期间新来的写者照常入队等待；leader 做完后唤醒新的队首，它成为下一任 leader，一次带走这段时间攒下的全部写者。三种结构都靠"上一次 I/O 进行期间新来的写者攒在一起"来合并，区别只在不同步的写入要付多少代价。

本项目选 C：一次 fsync 约 3 ms、一次 `write` 约 1 µs，代价几乎都在 fsync 上；memtable 允许并发插入，A 里"反正要排成一列"的理由不成立；不同步的写入每秒几十万次，A 与 B 的交接（几微秒到几十微秒）会明显拖慢它。代价是 `write` 系统调用没有合并，每个写者仍各调一次。

### 只合并 fsync：两个位置

每个 WAL 记两个字节位置：

| | 意思 | 谁改 | 用什么保护 |
| --- | --- | --- | --- |
| `appended` | 已经 `write` 进页缓存到第几个字节 | 追加的人，在 `write_lock` 里，`write` 返回之后 | 原子变量：同步的人读它不必拿 `write_lock` |
| `synced` | 已经确认落盘到第几个字节 | 做 fsync 的人 | `Mutex`，和"有没有人正在 fsync"的标记放在一起，见下一节 |

不变量：`synced ≤ appended`；`[0, synced)` 断电也不丢，`[synced, appended)` 只在页缓存里。追加时，写者记下自己这条记录的末尾 `my_end`（`Wal::append` 的返回值，就是追加之后的 `appended`）。同步时：

```text
拿同步锁
if synced >= my_end: 返回           ← 排队期间别人那次 fsync 已经带上了我
target = appended                    ← 必须在 fsync 之前读
fsync
synced = target
放锁
```

这是"拿锁之后复查"的套路，与冻结时的 `try_freeze` 相同。上面这样拿着锁做 fsync 是能工作的第一版，它的问题见下一节。两个容易写错的地方：

- **`target` 要在 fsync 之前读**：fsync 进行期间才追加的记录不一定被它带下去。fsync 之后才读，`synced` 会越过这些记录，它们的写者随后看到 `synced ≥ my_end` 直接返回，以为落盘了，其实没有。
- **比较的是自己的 `my_end`，不是当前的 `appended`**：写入一直在进行时 `appended` 总在增长，排队的写者拿到锁后总会发现"还有没同步的"，每人再做一次 fsync，又退回到排成一列。

三个写者、每条 100 字节的例子：

| 时刻 | 发生了什么 | appended | synced |
| --- | --- | --- | --- |
| 1 | W1 追加，`my_end = 100` | 100 | 0 |
| 2 | W1 拿到同步锁：0 < 100，`target = 100`，开始 fsync | 100 | 0 |
| 3 | fsync 期间 W2（`my_end = 200`）、W3（`my_end = 300`）追加，在同步锁上排队 | 300 | 0 |
| 4 | W1 的 fsync 结束，`synced = 100` | 300 | 100 |
| 5 | W2 拿到锁：100 < 200，`target = 300`，fsync | 300 | 100 |
| 6 | 结束，`synced = 300` | 300 | 300 |
| 7 | W3 拿到锁：300 ≥ 300，**不做 fsync** | 300 | 300 |

### 等 fsync 的人睡在哪里：锁的粒度

第一版拿着同步锁做 fsync，这把锁同时干两件事：保护 `synced`，以及让"同一时刻只有一个人 fsync"。于是一个写者想知道"我是否已经落盘"，必须先拿到这把锁，而这把锁可能正被下一次 fsync 占着。

背景是线程怎样等一把锁。线程有三种状态：运行（在 CPU 上）、就绪（能运行，排队等 CPU）、阻塞（等某件事，调度器不选它）。常见的锁是内存里的一个整数：拿锁先用一条原子指令"0 改成 1"，成功就拿到了，不进内核；失败就进内核，挂到以这个地址为键的等待队列上阻塞。放锁时改回 0，有人在等就请内核叫醒一个。被叫醒的线程只是回到"就绪"，真正跑起来要几微秒到几十微秒，跑起来之后还要再抢一次；这期间一个正在运行的线程可以直接把锁拿走。锁不保证先来先得：硬要按先后交给睡着的那个，锁就得空着等它醒来，吞吐会差很多。

放到 group commit 上：fsync #1 结束、放锁时，等着的人里有已经被 #1 带下去的 B，也有没被带下去的 C。C 先抢到锁，就拿着锁做 fsync #2，几毫秒；B 已经落盘，却要等到 #2 做完才能拿到锁、发现自己早就好了。后果有两个：B 的延迟多出一次 fsync；B 回来得晚，赶不上 #2 开始之前追加下一条，每次 fsync 带走的人变少。线程越多，这种插队越常见：16 个线程时每次 fsync 只带走约 4 个写入，理论值是 8（见下一节）。

改法是缩小锁的粒度：锁只保护两个数，fsync 在锁外做，等的人睡在**条件变量**上。

```rust
struct SyncState {
    synced: u64,    // 确认落盘到哪里，只增不减
    syncing: bool,  // 有没有人正在 fsync，同一时刻最多一个
}

pub fn sync_to(&self, end: u64) -> Result<bool> {
    let mut state = self.sync_state.lock().unwrap();
    loop {
        if state.synced >= end { return Ok(false); }        // 已经落盘
        if !state.syncing { break; }                         // 没人在 fsync：我来
        state = self.sync_done.wait(state).unwrap();         // 有人在 fsync：睡下，醒来再看
    }
    state.syncing = true;
    let target = self.appended.load(Ordering::SeqCst);      // 仍在 fsync 之前读
    drop(state);                                             // fsync 期间不拿锁
    let result = self.fsync();
    let mut state = self.sync_state.lock().unwrap();
    state.syncing = false;
    if result.is_ok() { state.synced = target; }
    drop(state);
    self.sync_done.notify_all();                             // 叫醒全部，各自复查
    result.map(|()| true)
}
```

条件变量是和一把锁配对的等待队列，自己不记任何数据；条件放在那把锁保护的数据里（这里是 `synced` 与 `syncing`）。`wait` 收走锁的守卫，放锁并睡下（两件事一步完成，通知不会落在中间的空隙里），被叫醒后重新拿锁才返回，所以醒来时又拿着锁，可以安全地复查。醒来一定要复查：允许没人通知也醒来，而且从被叫醒到重新拿到锁之间，别人可能已经改了状态（例如另一个醒来的人先当了 leader）。条件变量本身见 [[rust-learning/notes/09-concurrency/09-Condvar-与条件等待|Condvar 与条件等待]]。

同一个时刻，两种写法的区别：

| fsync #1 刚结束 | 第一版：拿着锁做 fsync | 条件变量 |
| --- | --- | --- |
| B、C 此前睡在哪 | 锁的等待队列 | 条件变量的等待队列 |
| C 抢到锁之后 | 拿着锁做 fsync #2 | 设 `syncing`、读 `target`、放锁，再做 fsync #2 |
| 已落盘的 B | 抢不到锁，等到 #2 做完 | 锁只被 C 拿了一瞬间，B 很快拿到，看到 `synced ≥ end`，返回 |

用 `notify_all` 而不是 `notify_one`：一次 fsync 可能让好几个人同时落盘，只有他们自己知道各自的末尾，只叫醒一个，被叫醒的未必是已落盘的人。代价是没落盘的人也被叫醒、看一眼又睡回去。

工业上是同一个坑：PostgreSQL 的 `XLogFlush` 用 `LWLockAcquireOrWait` 拿 `WALWriteLock`，锁被占着时只等它放开、自己不去拿，放开之后先复查自己的记录是否已经刷下去，没有才去抢锁；LevelDB 给每个写者一个自己的条件变量，leader 做完之后逐个唤醒同组的写者。

测试要确定地造出"C 正在做 fsync #2、B 已落盘"的时刻，靠 `sleep` 去凑测不稳。做法是一个只在测试里存在的"fsync 许可"：打开后每次 fsync 都要先拿到许可，没有就停在半路，测试决定放行几次（实现本身也是锁加条件变量）。让 C 比 B 先开始等：拿着锁做 fsync 的写法里，放锁时内核先叫醒先来的 C，B 就被挡住，测试一定失败。

### 为什么只到线程数的一半

一次 fsync 只带走它**开始之前**已经追加的记录。它进行期间又有一部分线程追加、排队，只能等下一次。于是线程分成两拨轮流：一拨正在 fsync，另一拨在排队。8 个线程时每次 fsync 大约带 4 个写入，每个写入大约等两次 fsync 的时间。A 也一样。PostgreSQL 的 `commit_delay` 是故意先等一小会儿，让更多人赶上这一趟。

### 几个 WAL 文件

同步写入的承诺是"这次写入，以及它之前完成的全部写入，都已落盘"。写完之后表可能已经被冻结，所以 `write` 在读锁里留住这张表的 `Arc` 与 `my_end`，再在读锁外调用 `sync_after_write`：

- 自己写进的那张表：同步到 `my_end`（`sync_wal_to`）。
- 比它旧的冻结的表：整个同步。里面可能还有更早完成、没同步过的写入；多半已经同步过，`synced == appended`，不做 fsync。
- 比它新的表：不用管。冻结要等这次写入放开读锁，新表里的写入都晚于这次。

`sync()`（调用方显式要求同步）对每张表同步到当前的 `appended`。L14 的一项欠账顺带解决：冻结的表的 WAL 同步过之后不再每次都 `sync_all`。

打开已有的 WAL 时 `synced` 从 0 开始：读出的记录可能只在页缓存里（上次进程崩溃时还没同步），第一次同步照样 fsync。

### fsync 失败

fsync 失败时返回错误，`synced` 不变，后面排队的写者会自己再试一次。这不安全：Linux 上 fsync 报过一次错之后，那些脏页可能已经被标成干净，再 fsync 会"成功"，数据却没落盘（PostgreSQL 2018 年的 fsyncgate）。正确的做法是 fsync 失败之后停止写入，与"后台出错停写"是同一项欠账。

### 不同步写入的锁竞争

开了 WAL 之后，`write_lock` 把"追加 WAL、插入表"排成一列，不同步的写入多线程反而更慢：

| 每秒万次 | 1 线程 | 4 线程 | 8 线程 |
| --- | --- | --- | --- |
| 不写 WAL | 155 | 280 | 324 |
| 写 WAL、不同步 | 53 | 28 | 27 |

锁每次只持有约 1 µs，但抢不到锁的线程要睡下再被唤醒，这一来一回比 1 µs 贵。锁里有两件事：同一个文件的 `write` 本来就有先后，只能靠攒成一次 `write` 减少次数（A 的做法）；插入跳表可以挪出锁外，前提是 key 带序号、顺序写在数据里，再加上"序号更小的都插完才推进已发布的序号"（RocksDB 的 `allow_concurrent_memtable_write`、pipelined write）。第三周加时间戳时一起做。

### 放弃持久性换速度

不要求断电不丢时，写入可以快得多，各种系统按"丢多少"排成一列：

| 做法 | 例子 | 断电最多丢多少 |
| --- | --- | --- |
| 每次同步 | RocksDB `sync = true`；PostgreSQL 默认 | 不丢 |
| 应用攒批，一批同步一次 | 写批次加 `sync` | 不丢 |
| 后台定期同步，写入不等 | Redis `appendfsync everysec`；PostgreSQL `synchronous_commit = off` | 大约一个周期 |
| 不同步，交给内核 | RocksDB 默认；本项目默认 | 内核还没写回的部分 |
| 先攒在进程内存里 | RocksDB `manual_wal_flush` | 进程崩溃也会丢 |
| 不写 WAL | RocksDB `disableWAL`；mito2 表选项 `skip_wal`；本项目 `enable_wal: false` | memtable 里的全部 |

不写 WAL 的系统，持久性由别处负责：Flink 的 RocksDB 状态后端靠 checkpoint 加上游重放，Kafka Streams 的状态存储靠 changelog topic，mito2 可以把 WAL 放进 Kafka（remote WAL），Raft 之上的状态机靠 Raft log。判断能不能放弃，就问"崩溃之后丢掉的那部分能从哪里找回来"。批量导入更进一步，连 memtable 都绕过：RocksDB 的 `SstFileWriter` 在外面写好 SST，`IngestExternalFile` 直接挂进 LSM。

## SST 的校验和

### 保护哪一段，存在哪里

版本 2 的格式：

```
┌──────┬─────┬──────┬─────┬───┬──────┬─────┬────────┬─────┬──────────┬────────────┬────────┬──────┐
│ 块 0 │ crc │ 块 1 │ crc │ … │ 索引 │ crc │ filter │ crc │ 索引起点 │ filter 起点 │ 版本 2 │ LSM1 │
└──────┴─────┴──────┴─────┴───┴──────┴─────┴────────┴─────┴──────────┴────────────┴────────┴──────┘
```

每个 crc 是紧挨在它前面那一段的 crc32，4 字节。七条记录、三个块的例子：

| 段 | 内容 | 校验和 |
| --- | --- | --- |
| 块 0 | `[0, 32)` | `[32, 36)` |
| 块 1 | `[36, 68)` | `[68, 72)` |
| 块 2 | `[72, 84)` | `[84, 88)` |
| 索引 | `[88, 122)` | `[122, 126)` |
| filter | `[126, len - 20)` | `[len - 20, len - 16)` |
| 末尾 16 字节 | 88、126、2、`LSM1` | 没有 |

块的范围仍由索引划出（下一个块的起点、最后一个块到索引的起点），只是最后 4 个字节是校验和；块的目标大小只算内容，不算校验和（mini-lsm 也这样，好让之前的测试不变）。

**每块跟着自己，还是集中放在末尾**：跟着自己，读一个块只读这一段，块缓存没命中时也只多读 4 个字节；集中放在末尾，打开时要把所有块的校验和读进内存，或者每次读块多一次读。RocksDB 每个块后面跟 5 字节（1 字节压缩类型、4 字节校验和），LevelDB 相同。

### 只算自己那一段

filter 是接在已有字节后面写的（`Bloom::encode` 往缓冲区末尾追加）。校验和如果算整个缓冲区，读的一侧只拿到 filter 那一段，永远对不上。所以构建器在每一段之前记下起点，`append_checksum(section_start)` 只算 `[section_start, 末尾)`；块、索引、filter 都走它。故意从 0 算时，44 个测试失败。

### 先核对，再解码

`verify_checksum` 把一段切成"内容 + 4 字节"，对上了才把内容交给 `Block::decode`、`decode_block_meta`、`Bloom::decode`；损坏的字节不进解码器，也不进块缓存。一段短得放不下 4 字节时直接报损坏，不越界。

**末尾 16 字节不带校验和**：魔数与版本号各自核对；两个起点先查范围（起点不倒置、不进入末尾），指错了位置，切出来那一段的校验和就对不上。把文件的每一个字节逐个翻转，打开或读块时全部报告损坏，没有一次读出错的内容，也没有 panic。

版本 1 没有这些校验和：改坏一个 value 的字节，读出来就是错的值，没有任何报错。结构检查（[SST 格式](05-sst-format.md)里的魔数、起点范围、索引自洽）只能发现"不像一个 SST"，发现不了"内容被改了但格式仍然合法"。

### 版本 2 与读旧版本

`SST_FORMAT_VERSION` 升到 2，打开时认 1 到 2。`SsTable` 多一个 `format_version` 字段，读块时据此决定要不要核对；构建器有一个只在测试里用的 `with_format_version(1)`，用来造旧文件。compaction 读旧文件、写出新文件，旧格式随合并自然消失。L13 之后有了需要兼容的旧文件，所以保留读版本 1 的代码（[SST 格式](05-sst-format.md#文件的组成)里的决定）。

### 各个文件的校验和

| 文件 | 校验单位 | 损坏时 |
| --- | --- | --- |
| WAL | 每条记录（一批）一个 crc32 | 末尾不完整或最后一条不对：截掉；中间一条不对：拒绝打开 |
| MANIFEST | 每条记录一个 crc32 | 同上 |
| SST | 每个数据块、索引、filter 各一个 crc32 | 打开时索引、filter 不对：拒绝打开；读块时某个块不对：这次读返回错误 |

## 损坏与恢复模式

WAL 与 manifest 的规则是：末尾坏了当残尾截掉，中间坏了拒绝打开。这对"每次都同步"是对的：同步过的记录不该坏，坏了说明有别的问题，拒绝打开比悄悄丢数据好。但默认不同步时，断电留下的未必是一段完整的前缀：内核把页缓存写回磁盘不保证按追加的顺序，WAL 里没同步的那一段可能是"第 10 条在、第 9 条是垃圾"，这时数据库会打不开，而坏掉的那段本来就是允许丢的。

RocksDB 用 `WALRecoveryMode` 让用户选：

| 模式 | 遇到损坏 | 适合 |
| --- | --- | --- |
| `kAbsoluteConsistency` | 任何损坏都报错 | 每次同步、不能丢 |
| `kTolerateCorruptedTailRecords` | 只容忍末尾（本项目现在的规则） | 每次同步 |
| `kPointInTimeRecovery`（默认） | 停在第一处损坏，后面的全部丢弃 | 不同步：恢复到损坏之前的一致状态 |
| `kSkipAnyCorruptedRecords` | 跳过坏的继续读 | 抢救数据 |

停在第一处损坏，前面是一段完整的前缀，不会出现"新的在、旧的没了"。代价是同步过的记录真被位翻转时，后面的数据被悄悄丢掉。本项目先保持严格的规则：断电还没有验证（要能模拟"只有同步过的字节留下"），到时再一起加恢复模式。mini-lsm 的附加题是另一个方向：SST 校验失败时以只读的安全模式打开，读出没坏的部分。

## 本项目的做法

- **接口**：`WriteBatchRecord<T: AsRef<[u8]>> { Put(T, T), Delete(T) }`，key 与 value 可以是 `&[u8]`、`Vec<u8>`、`Bytes`。`LsmStorage::write_batch`、`write_batch_opt` 与 `Db` 上的同名方法；`put_opt`、`delete_opt` 变成只有一条的批次。空的 key、`Put` 的空 value 仍是 panic。
- **写入**：`write` 先把整批的长度都检查完（有一条超长就返回 `InvalidArgument`，什么都不写），空批次不写记录（`sync` 时照样同步）；读锁里留住表的 `Arc`、`put_batch` 拿到 `my_end`；计数按整批；`sync` 时 `sync_after_write`；最后试着冻结。
- **WAL**：`Wal` 多了 `appended: AtomicU64`、`sync_state: Mutex<SyncState>`（`synced`、`syncing`）与 `sync_done: Condvar`；`append` 收一批、返回末尾；`sync_to(end)` 是 group commit 的核心，fsync 在锁外做，返回这次有没有真的做 fsync；`sync()` 是同步到当前的 `appended`；真正的 fsync 包在私有的 `fsync()` 里，测试可以让它变慢、停在半路（许可）、数它的次数。`recover` 返回 `Vec<WalBatch>`，批次的边界不变。
- **单条写入不走写批次的路径**：`put_opt`、`delete_opt` 直接把栈上的 `[(key, value)]` 交给 `write`。经过 `write_batch_opt` 要在堆上建一个 `Vec`，不写 WAL 时一次写入约 0.3 µs，这次分配让它慢两成。
- **统计**：`wal_syncs` 记真正做过的 fsync，基准报告单列一栏。
- **SST**：见上一节。
- **测试**：WAL 的批次格式（单对批次与旧格式字节相同、截在任何字节都是整批在或不在、内容为空或尾部零星字节是损坏）；批次的语义（同一个 key 后面的覆盖前面的、有一条超长整批不写、空批次、超过上限仍在一张表、重开之后整批都在、写了一半的批次一对都不在、不开 WAL 时并发的批次不交错）；group commit（已同步过的不再 fsync、没有新追加不 fsync、恢复后的第一次照样 fsync、fsync 期间追加的要再 fsync 一次、8 个线程同步 10 次而 fsync 不到一半、只等自己那条记录、同步写入顺带同步更旧的冻结表）；SST（逐字节的版本 2 布局、每个校验和只算自己那一段、每个字节翻转都被发现、改 value 后仍能解码的块被校验和发现、索引与 filter 的校验和、版本 1 照样能读、版本 1 发现不了被改的 value、读版本 1 写出版本 2）。
- **并发的测试怎样测得出**：group commit 的测试用只在 `cfg(test)` 下存在的"慢 fsync"（每次停 20 ms），让别的线程在这段时间里追加、排队；"`target` 要在 fsync 之前读"用另一个线程等到 fsync 开始之后再追加一条，确定地造出那个时机；"已落盘的人不被下一次 fsync 挡住"用 fsync 许可。变异检查：不复查、fsync 之后才读 `target`、`synced` 只设成自己的 `end`、恢复时当作已同步、不同步更旧的表、用当前的 `appended` 去比、拿着锁做 fsync、`notify_one`、醒来不再复查，各有测试失败。fsync 失败时清掉 `syncing`、不推进 `synced` 这条路径没有测（要能注入 fsync 失败）。
- **基准驱动**：`--batch-size N`（写负载每次调用写 N 条，引擎用写批次，延迟按每次调用计），报告多一栏 WAL 的 fsync 次数。

## 效果

基准记录：[写批次、group commit 与 SST 校验和](../bench/results/2026-10-22-write-batch-and-checksums.md)（电池供电，与 L14 的二进制交替运行）。

- **group commit**：同步写入，每次 fsync 带走约"线程数 ÷ 2"个写入；16 个线程时每秒 2086～2152 次，改动前 264～274 次（8 倍），p99 从 67～70 ms 降到 13～15 ms。单线程不变。
- **第一版与条件变量**：拿着锁做 fsync 的第一版，16 个线程时每次 fsync 只带走约 4 个、每秒 1100～1300 次；8 个线程时 p99 22～36 ms，条件变量版 8～11 ms。
- **写批次**：同步时 100 条一批每秒写入 2.67 万条，一条一条只有 288 条（fsync 按批平摊）；不同步时单线程从 51 万条到 130 万条；8 个线程一条一条写只有 26 万条（锁竞争），10 条一批就到 96 万条，p99 从 608 µs 降到 22 µs。
- **单条写入**：不写 WAL、写 WAL 不同步，1 与 8 个线程都在浮动之内。
- **SST 校验和**：读慢 5～12%（4 KB 的块算一次 crc32 约 351 ns，readrandom 每次点查从文件读 0.75 个块，约 7%）；块缓存命中时不核对；文件大 0.1%。

## 工业对照

**mini-lsm**（[week2-07-snacks.md](../../references/mini-lsm/mini-lsm-book/src/week2-07-snacks.md)）：`write_batch` 收 `WriteBatchRecord<T: AsRef<[u8]>>`，`put` 与 `delete` 转给它，这一章不保证原子可见。校验和：每个块后面 4 字节 crc32；索引与 filter 各带 crc32（filter 的校验和只算 filter 自己）；WAL 每条记录带 crc32；manifest 每条 JSON 前加长度、后加 crc32。设计问题里问到"只提供写批次、由单一写线程从 mpsc 通道接收"（BadgerDB 的做法，即上面的 B）。

**RocksDB**：

- `WriteBatch` 的内部表示 `rep_`：8 字节起始序号、4 字节条数，然后一条条记录（类型、key、value）。整批是 WAL 里的一条记录。插入 memtable 时 `WriteBatchInternal::InsertInto` 逐条插入，每条用"起始序号 + i"。
- `WriteThread`：写者 `JoinBatchGroup` 排队，队首 `EnterAsBatchGroupLeader` 收集一组，写 WAL、需要时同步一次；`allow_concurrent_memtable_write` 时同组的写者各自并发插入 memtable；`enable_pipelined_write` 让写 WAL 与插入 memtable 分成两段流水。
- 块的尾巴：1 字节压缩类型、4 字节校验和，校验和覆盖块的内容与压缩类型。`ChecksumType` 可选 CRC32c、xxHash、xxHash64、XXH3 等；`ReadOptions::verify_checksums` 默认打开。
- `WALRecoveryMode` 见上一节。

**LevelDB**：`DBImpl::Write` 是 A 的原型：写者进 `writers_` 队列，队首调用 `BuildBatchGroup` 把后面的批次合进来（上限约 1 MB；队首的批次很小时上限只比它多 128 KB，免得小写入的延迟被大批次拖长；不同步的 leader 不带要同步的写者），放开 mutex 写 log、同步、插入 memtable，再逐个标记完成并唤醒。

**PostgreSQL**：`XLogFlush` 就是 C：拿 `WALWriteLock` 之后复查"已刷到的位置"是否已经越过自己的提交记录，越过了就不再刷。`commit_delay` 与 `commit_siblings` 让刷之前先等一小会儿，多带几个事务。

### 导读：mito2 的写入请求

路径在 `references/greptimedb/src/mito2/src/` 下。mito2 是 B：每个 region 按表编号与 region 编号取模分到一个 region worker（`region_id_to_index`），一个 worker 负责一组 region 的全部请求。

1. **攒一批请求**：[worker.rs](../../references/greptimedb/src/mito2/src/worker.rs) 的 `run`。先等到一个请求，再用 `try_recv` 最多攒到 `worker_request_batch_size` 个，然后 `handle_requests` 一起处理。对应 C 里"fsync 期间排队的写者"，区别是它攒的是整个请求，连 WAL 的写入也合并了。
2. **一次写 WAL**：[worker/handle_write.rs](../../references/greptimedb/src/mito2/src/worker/handle_write.rs) 的 `handle_write_requests`。先检查内存压力（拒绝或暂停写入，即写入停顿），再按 region 准备写入上下文；然后每个 region 的写入 `add_wal_entry` 放进同一个 `WalWriter`，一次 `write_to_wal`；成功后按返回的 `last_entry_ids` 推进各 region 的条目编号，失败则这一批全部失败。对应本项目的 `write` 与 `Wal::append`。
3. **跳过 WAL**：同一段里 `region_ctx.skip_wal()` 为真的 region 不写 WAL：表选项 `skip_wal`（`store-api/src/mito_engine_options.rs` 的 `SKIP_WAL_KEY`），或者 WAL 的 provider 是 `Noop`（[region_write_ctx.rs](../../references/greptimedb/src/mito2/src/region_write_ctx.rs) 的 `skip_wal`）。
4. **之后写 memtable**：写完 WAL 再把每个 region 的写入放进 memtable，序号在 WAL 之前分配，所以插入不必和 WAL 在同一把锁里。

可以先跳过的：bulk 请求、写入停顿与拒绝的细节、各阶段的计时。与本项目对照：mito2 一个 worker 一个线程，天然排成一列，合并 WAL 写入不需要额外的锁；本项目的写者各自在调用线程里写，只合并 fsync。

## 常见错误

- **一批拆成几条 WAL 记录。** 崩溃时可能只留下前几条，重放出半批。
- **WAL 里层加了数量字段却没有版本标记。** 旧文件的第一个长度被当成数量。
- **先写了批次的前几对，才发现后面有一条超长。** 长度要在写任何东西之前全部检查完。
- **以为跳表能"一次插入一批"。** 一对一对插入，读者可能看到半批，而且不一定是前缀。
- **不开 WAL 时多对的批次不排队。** 并发的两批交错，写完之后的状态不是任何先后次序的结果。
- **group commit 在 fsync 之后才读要落盘的位置。** fsync 期间追加的记录被当成已落盘。
- **拿当前追加到的位置去比，而不是自己记录的末尾。** 写入不断时每人都再做一次 fsync，退回到排成一列。
- **同步写入只同步自己那张表。** 更旧的冻结表里还有没同步的写入，断电后"新的在、旧的没了"。
- **打开旧 WAL 时把读出的记录当成已同步。** 那些字节可能只在页缓存里。
- **fsync 失败后重试一次就当作成功。**
- **拿着保护 `synced` 的锁做 fsync。** 已经落盘的人要先拿到锁才知道自己好了，被下一次 fsync 挡住；线程一多，合并的效果与尾延迟都变差。
- **等条件变量时用 `if` 而不是循环，或者只 `notify_one`。** 醒来时状态可能已经变了；只叫醒一个，被叫醒的未必是该醒的人。
- **为了接口统一，让单条写入也付批次的代价。** 一次堆分配在 0.3 µs 的写入路径上就是两成。
- **校验和算整个缓冲区，而不是这一段。** 往已有缓冲区后面追加的 filter、索引最容易出错。
- **先解码再核对校验和。** 损坏的字节进了解码器，可能 panic 或读出错的长度。
- **核对之前先用长度切片。** 长度字段本身可能坏了，切片之前要检查范围。
- **改了格式却没有升版本号，或者升了版本号却不再认旧版本。**

## 资料

- mini-lsm 教程第二周第 7 章：[week2-07-snacks.md](../../references/mini-lsm/mini-lsm-book/src/week2-07-snacks.md)。
- 已学内容中相关的篇：[原子写批次](../../storage/notes/05-storage-recovery/06-atomic-write-batches.md)、[文件格式与元数据校验](../../storage/notes/05-storage-recovery/03-file-format-and-meta-validation.md)、[提交顺序与同步](../../storage/notes/05-storage-recovery/04-commit-order-and-sync.md)、[WAL](14-wal.md)、[SST 格式](05-sst-format.md)。
- RocksDB Wiki：Write Batch With Index、WAL Recovery Modes、Pipelined Write、Block-based Table Format。源码（v9.10.0）：`db/write_thread.cc`、`db/write_batch.cc`、`table/block_based/block_based_table_builder.cc`。
- LevelDB：`db/db_impl.cc` 的 `DBImpl::Write`、`BuildBatchGroup`。
- PostgreSQL：`src/backend/access/transam/xlog.c` 的 `XLogFlush`；文档 WAL Configuration（`commit_delay`、`synchronous_commit`）。
- crossbeam-skiplist 0.1.3：`base.rs` 的 `insert_internal`。

返回 [本组目录](README.md)。
