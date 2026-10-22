# compaction 的执行

状态：已有正文 · 代码：[lsm_storage/compact.rs](../engine/src/lsm_storage/compact.rs) 的 `force_full_compaction`、`apply_compaction_result`、`compact`；[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `write_ssts`、`get`、`scan`；[iterators/concat_iterator.rs](../engine/src/iterators/concat_iterator.rs) 的 `SstConcatIterator`、`find_sst_index`

## 位置

[全景](README.md#全景)的第 ⑤ 步：把 L0 的文件合并进下一层。L02～L08 做完了从写入到 L0 文件的全部部件，但到这一篇之前文件只增不减。这一篇回答"给定一组要合并的文件，怎样合并、换进状态、删掉旧文件"，以手动触发、全部文件合成一层（full compaction）的形式实现。"什么时候合并、挑哪些文件、分几层"是 L10～L12 的策略；把"现在有哪些文件、各在哪一层"写到磁盘上是 [L13 manifest](13-manifest.md)。

## 与 B+Tree 阶段的对应

| B+Tree | LSM |
| --- | --- |
| 覆盖与删除就地进行，找到那一页改掉；空出来的页进空闲列表 | 覆盖与删除只写新记录，旧数据先不管；compaction 把推迟的整理批量补上 |
| 旧页要等登记过的读者都结束才能回收（存储笔记[页回收与读者](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/07-page-reclamation-and-readers.md)） | 旧文件要等拿着旧状态的读者都结束才能真正释放；Unix 上可以先删文件名，把"等读者"交给操作系统 |
| 写完新页再切换 meta，切换是一步 | 写完新文件再换状态指针，换状态是一步 |

## 问题：文件只增不减

- **读越来越慢。** 乱序写入时 L0 每个文件的范围都几乎覆盖全部 key。点查有 Bloom filter 也仍要对每个文件做一次范围检查、问一次 filter；扫描用不上 filter，要在每个范围重叠的文件上各建一个游标、各读一个块，再归并。文件数翻倍，这些开销也翻倍。
- **空间只占不还。** 覆盖写之后旧版本仍在旧文件里；删除写的是墓碑，被删的值和墓碑都永远占着空间。只删不写的负载里，删得越多占得越多。
- **其他资源跟着涨。** 每个文件一个打开的文件描述符、一份常驻内存的索引与 filter。

## 层

**层（level）是对 SST 文件的分组，记在引擎的状态里**（以后写进 manifest），不是目录，也不体现在文件名上；文件编号只表示创建的先后。

- **L0**：flush 直接产出的文件，每个是一张 memtable 的全部内容。一张 memtable 装的是一段时间内写入的所有 key，所以 L0 的文件之间范围互相重叠，只能**按新旧**排列、查找时逐个去看。
- **L1 及以下**：compaction 产出的文件。一层的所有文件合起来是一个排好序、互不重叠的序列，叫 **sorted run**，相当于一个大的有序文件被切成几段。查一个 key 在一层里最多只看一个文件；层内文件按 **key** 排列，不表示新旧。
- **层与层之间越往下越旧**：compaction 只把数据从上层搬到下层，所以上层的任何一条都比下层的新。点查按"memtable → 冻结的 memtable → L0（新的在前）→ L1 → L2 …"的顺序，在第一个找到的地方停下。通常每层比上一层大约 10 倍，数据随时间一层层下沉。

**为什么要有 L0。** L1 是一个整体有序的序列，往里放数据就要与范围重叠的部分合并、重写。每次 flush 都直接并进 L1，一张几 MB 的 memtable 可能要重写几百 MB 的 L1，写放大极大，flush 也变慢。L0 是缓冲区：flush 只快速顺序地写一个文件，攒够几个再一起合并，重写的成本由这一批分摊。

## compaction 做什么

```
L0  #5 [b=3, d=删除]           ← 最新
    #4 [a=1, b=2, c=9]
L1  #1 [a=0, c=7, d=4, e=5]    ← 最旧
          │ 归并：同一个 key 只留最新的版本；最底层丢掉墓碑
          ▼
L1  #6 [a=1, b=3, c=9, e=5]
```

1. **归并所有输入**：L0 的文件按新旧排好放进归并游标，L1 用串接游标接在后面。归并游标对同一个 key 只给出最新来源的那一条（[迭代器与归并](03-iterators-and-merge.md)），所以旧版本在这一步就被丢掉了。
2. **丢掉墓碑**（只在条件满足时，见下一节）。
3. **按目标大小切分写出**：输入有序，按顺序写、写满一个文件就换下一个，切出的文件自然按 key 排好、互不重叠。
4. **换状态**：从 L0 拿掉输入，把新文件放进 L1。
5. **删除旧文件**。

除了切分、换状态与删除，用到的零件都已经有：归并游标（L03）、SST 构建器（L05）、"锁外写文件、锁内换状态"的做法（L07）、读者通过 `Arc` 拿着旧文件（L06）。

## 墓碑什么时候可以丢

只要还可能有更旧的版本**没参与这次合并**，就不能丢墓碑：

```
L1 里有 d=4，L0 里有 d 的墓碑。只合并 L0 的几个文件并丢掉墓碑：
合并结果里没有 d → 查 d：L0 没有，往下查 L1，找到 d=4，被删除的值复活了
```

几种判断办法：

| 做法 | 评价 |
| --- | --- |
| 只在合并到最底层（下面没有任何层）时丢 | 正确且没有开销；全量 compaction 就是这种情况 |
| 合并时去下面每一层点查这个 key | 精确，但每个墓碑都要对每一层做一次点查，墓碑多时开销很大 |
| 看下面没参与合并的层里，有没有文件的 key 范围覆盖这个 key | 只用内存里的首末 key，几乎没有开销；判断"可能有"时多留一条墓碑不会出错。RocksDB 用这种（`KeyNotExistsBeyondOutputLevel`） |

有了快照读（L16 之后）还多一个条件：还有读者拿着一个旧时刻的快照时，它可能需要看到删除之前的值，旧版本与墓碑都不能随便丢。

## 读 L1

**点查**：L1 的文件互不重叠，按"第一个末 key 不小于 key 的文件"二分找到唯一的候选，只查它。找到的文件不一定有这个 key：key 可能落在它与前一个文件之间，比它的首 key 还小，所以仍要做范围检查。

**扫描**：所有来源**同时**往前走、按 key 交错输出，不是"先扫完 L0 再扫 L1"。扫描要按 key 从小到大边读边给，同一段 key 分散在每一层里，只能让每个来源各出一个游标，每一步取当前最小的。归并把两个要求分成主次：**主要按 key**，保证输出有序、可以随时停下；**key 相同时按新旧**，只输出最新的版本。按来源依次读则既无法保证顺序，又要把整个范围读进内存排序、并记住哪些 key 已经输出过。点查可以从新往旧，是因为它只找一个 key、找到就停。

```
去墓碑与上界
└── 两路归并（左边胜出）
    ├── 两路归并（左边胜出）
    │   ├── 归并：每张 memtable 一个游标
    │   └── 归并：L0 每个文件一个游标
    └── 串接：L1 的文件按 key 一个接一个
```

**串接游标**：一层的文件按 key 排好、互不重叠，前一个文件的 key 全都小于后一个的，所以一个读完接下一个，结果就是有序的，不需要堆。它只在走到某个文件时才打开那个文件，不会一开始就给每个文件读一个块；定位时只打开目标所在的文件。

## 执行的步骤与并发

```
拿 compaction 的锁（同一时刻只做一次 compaction）
    记下此刻 L0 与 L1 的文件（这次的输入）
    归并、写新文件、同步目录          ← 慢；读、写、冻结、flush 都照常进行
    拿 state_lock，在当前状态的副本上：
        从 L0 按编号拿掉输入，其余留下
        L1 整层换成新文件
    换指针                            ← 一瞬间
    删除旧文件
```

**合并期间 flush 照常进行。** compaction 写文件要花一段时间，这期间后台可能又 flush 出一个文件放到 L0 的最前面。它不在这次的输入里，数据也比新的 L1 新，换状态之后必须留在 L0。

**丢失更新。** 最自然的写法是错的：用开始时取的快照，改成"L0 清空、L1 换新"，再整份换上去。这样 flush 后来加的文件不是被删掉，而是被**覆盖**掉了：compaction 用的是过时的副本，换上去时把别人的改动一起抹掉。这与两个线程各自"读、改、写回"同一个计数器是同一类错误。正确的做法有两条：

- **换状态时以当前状态为准**：拿到锁之后重新复制当前状态，在它上面改。
- **按编号只拿掉输入，不清空整层。**

"拿锁、复制当前状态、改、换指针"必须和 flush 换状态的那一段互斥，两者用同一把 `state_lock`；写文件这些慢的部分放在锁外，flush 不会被 compaction 卡住。

**L0 与 L1 的规则正好相反。**

- **L0 必须按编号拿掉**，因为合并期间 flush 会往里加文件。挑 L0 的文件时还不能只拿新的、留下旧的：旧版本留在上层，会盖住搬到下层的新版本。要么拿全部，要么至少拿最旧的那一批，保证留在 L0 的都比搬下去的新。
- **L1 在全量 compaction 里可以整层替换**：只有 compaction 改 L1，同一时刻又只有一个 compaction。更一般地，合并 L0 时**不必让 L1 全部参加**，只需拉进与输入范围重叠的那些文件；不重叠的文件不会与输入里的 key 相同，留着不会破坏"层内不重叠"。每次只重写与新数据相交的部分，写放大小得多，这是 leveled compaction 的核心做法（L12）。那时 L1 也必须按编号拿掉。

本项目在换状态时加了两个断言，把这些前提写成会被检查的条件：从 L0 拿掉的文件个数等于输入的个数；此刻的 L1 与输入是同样的几个对象（`Arc::ptr_eq` 逐个比较地址）。前提被破坏时立即报错，而不是悄悄丢数据。

## 删除旧文件

换上新状态之后，拿着旧快照的扫描可能还在读旧文件。数据块不在内存里，游标每走到一个新块都要去文件里 `pread`，所以问题是：文件被删除之后还能不能读。

**Unix 上的删除（`unlink`）只删掉名字。** 目录里记的是"文件名 → inode"，inode 才是文件本身。只要还有进程打开着这个文件，inode 与它的数据就不会被释放，持有描述符的一方照常能读；最后一个描述符关闭后，文件系统才回收空间。本项目的 `SsTable` 从创建起一直拿着打开的文件，最后一个 `Arc` 放手时才关闭，所以换状态之后立刻删除旧文件是安全的。这个结论建立在三个前提上：

1. **`SsTable` 一直拿着描述符。** 为了省描述符改成"用到时才打开"（RocksDB 的 `max_open_files` 会关掉不常用的文件），已经删掉的文件就再也打不开了。
2. **在 Unix 上。** Windows 默认不允许删除一个打开着的文件；对象存储（S3 一类）也没有"删了名字还能读"的语义。
3. **磁盘空间要等读者读完才回收。** 一个很长的扫描会让一批已删除的文件一直占着空间：`ls` 看不到，`df` 显示空间没减少，`lsof` 能看到它们标着 `(deleted)`。

LevelDB、RocksDB 不依赖这一点：它们记录每个文件被哪些"版本"（相当于本项目的状态快照）引用着，没有任何版本再引用某个文件时才删除它。这就是 B+Tree 阶段"旧页等读者结束再回收"的做法。

有了 [manifest](13-manifest.md) 之后还有一个顺序：新状态先写进 manifest 并同步，然后才能删旧文件；否则中途崩溃，重启后 manifest 里登记的还是已经被删掉的旧文件。

## 输出切成多个文件

每个输出文件写到目标大小就封口（RocksDB 默认 64 MB，与 memtable 一样大），而不是整层写成一个大文件：

- 以后的策略每次只挑一层里的**一部分**文件往下合并，文件小才能只重写一小段。
- 合并完成之前输入的旧文件不能删，新旧两份同时占着磁盘；每次合并的数据量越小，这个峰值越低。

切分时只在真的加入了一条记录之后判断"写够了没有"，否则刚换上的空构建器也可能被判成写够，写出一个空文件；跳过墓碑时同样要让游标前进。

## 另一种组织方式：tiered

不分出"每层一个 sorted run"，只保留一列按新旧排列的 sorted run，每次合并**相邻的几个**、结果放回原位，也是正确的：只要每次合并的是连续的一段，列表的顺序就始终等于数据的新旧。跳着合并（例如合并 `#7` 与 `#4`、留下中间的 `#5`）则不行，合并结果放在哪里都会颠倒新旧。

这就是 **tiered compaction**（RocksDB 叫 universal，Cassandra 叫 size-tiered）。它通常合并**大小相近**的几个 run：新 flush 的小文件攒够几个合成中等的，中等的再合成大的。越旧的数据越少被重写，所以写放大低；代价是同时存在好几个互相重叠的 run，点查与扫描都要看每一个，旧版本也留在不同的 run 里，空间放大高。

| | leveled（每层一个 sorted run） | tiered |
| --- | --- | --- |
| 写放大 | 高：数据每下沉一层，都要和下一层重叠的部分一起重写 | 低：每条数据只被重写少数几次 |
| 读放大 | 低：每层最多看一个文件 | 高：每个 run 都要看 |
| 空间放大 | 低：旧版本很快被清掉 | 高：多个版本留在不同的 run 里；合并两个大 run 时新旧两份同时占着磁盘 |
| 适合 | 读多、对空间敏感 | 写多、可以接受读慢一些 |

真实系统里两种都在用，常常混合：

| 系统 | 默认 | 另外可选 |
| --- | --- | --- |
| LevelDB | leveled | — |
| RocksDB | leveled | universal、FIFO（只按时间丢掉最旧的文件） |
| Pebble、TiKV（用 RocksDB） | leveled | — |
| Cassandra、ScyllaDB | size-tiered | leveled、按时间窗口的 TWCS；Cassandra 5.0 的 UCS 可以在两者之间连续调节 |
| GreptimeDB mito2 | TWCS（按时间窗口） | — |

RocksDB 的 leveled 本身就是混合的：它的 L0 文件按新旧排列、互相重叠，正是 tiered 的样子，L1 往下才是每层一个 sorted run。本项目的 L0 加 L1 与它同构。mito2 先按数据的时间戳把文件分进时间窗口，每个窗口里找出有几个 sorted run（`compaction/run.rs` 的 `find_sorted_runs`），太多时合并其中一部分（`reduce_runs`）：时序数据的写入几乎都落在最近的窗口，旧窗口写完就不再被重写。

## 本项目的做法

- **状态**：`LsmStorageState` 多了 `l1_sstables`，按 key 排好、互不重叠。
- **读**：`get` 把"在一个 SST 里找 key"（范围检查、问 filter、计数、定位）抽成 `get_from_sst`，L0 逐个调用，L1 用 `find_sst_index` 找到唯一的候选再调用。`scan` 只把与范围相交的 L1 文件交给串接游标，再用两路归并接在"memtable + L0"后面。
- **写 SST**：`write_ssts(游标, 目标大小, 是否丢掉墓碑)` 由 flush 与 compaction 共用：flush 传"不限大小、保留墓碑"，compaction 传"按目标大小切分、丢掉墓碑"。目标大小是选项 `target_sst_size`。`SsTableBuilder` 多了 `is_empty` 与 `estimated_size`。
- **compaction**：放在 `lsm_storage` 的子模块 `lsm_storage/compact.rs` 里。子模块能看到父模块的私有字段与方法，所以可以直接给 `LsmStorage` 加方法，而不必把 `state`、`write_ssts` 等改成公开的（见 Rust 笔记《模块、可见性与重导出》）。`compact` 只写新文件；`apply_compaction_result` 换状态；`force_full_compaction` 串起全部步骤，L0 为空时什么也不做。`compaction_lock` 让 compaction 排队，它与 `flush_lock` 是两把锁，flush 与 compaction 可以同时进行。
- **对外**：`Db::force_full_compaction`、`Db::l1_sstable_count`；命令行程序的 `compact` 命令，`status` 显示 L1 的文件数。
- **测试**：会走循环的测试放在带时限的线程里，循环里漏了让游标前进、换状态时拿着写锁再去读状态（死锁）都会在几秒内报"卡住"，而不是让整个测试进程停住。

## 效果

[2026-10-15 的基准](../bench/results/2026-10-15-full-compaction.md) 在同一次运行里对比合并前后（30 万条、33 个 L0 文件；100 万条覆盖写一遍、55 个 L0 文件）：

- **扫描快 7～19 倍**：从 33 或 55 路归并变成一个串接游标。
- **查不存在的 key 快 4～9 倍**：从检查每个文件变成只找一个候选。
- **查存在的 key 快 25%～80%**：Bloom filter 已经把进入的文件压到 1.1～1.2 个，必须读的那一个块省不掉。
- **写放大**：A 组每条数据写了两次，整次运行的写放大 1.95；C 组 1.46。
- **空间放大**：覆盖写之后从 1.95 降到 0.98，旧版本被清掉了。

## 工业对照

**mini-lsm**（[compact.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/compact.rs)）：`force_full_compaction` 与 `compact` 的分工、锁外合并锁内换状态、按编号拿掉 L0 的输入与本项目相同；串接游标 `SstConcatIterator` 的做法也相同。它在换状态的同时写 manifest，本项目到 L13 再加。

**RocksDB**：手动的全量合并是 `CompactRange`。状态是一串引用计数的 `Version`，compaction 的结果是一条 `VersionEdit`（加哪些文件、删哪些文件），应用到当前版本上得到新版本；没有任何 `Version` 再引用的文件才被删除。compaction 读输入时默认不放进块缓存（`fill_cache = false`），避免用即将作废的块挤掉有用的块；本项目的合并目前经过块缓存，见基准记录。

### 导读：mito2 怎样换状态、何时删文件

mito2 的 SST 存在对象存储上，没有"删了名字还能读"的语义，所以它显式地用引用计数决定何时删文件。先看 compaction 的结果怎样表示（`src/mito2/src/manifest/action.rs`）：

```rust
pub struct RegionEdit {
    pub files_to_add: Vec<FileMeta>,
    pub files_to_remove: Vec<FileMeta>,
    // ……（flush 用到的序号等字段，可以先跳过）
}
```

再看它怎样应用到当前版本（`src/mito2/src/region/version.rs` 的 `VersionBuilder::apply_edit`，删去了与 flush 有关的几行）：

```rust
if !edit.files_to_add.is_empty() || !edit.files_to_remove.is_empty() {
    let mut ssts = (*self.ssts).clone();                    // 复制当前版本的文件集合
    ssts.add_files(file_purger, edit.files_to_add.into_iter());
    ssts.remove_files(edit.files_to_remove.into_iter());     // 按文件编号拿掉
    self.ssts = Arc::new(ssts);
}
```

`remove_files` 并不删除文件，只做一个标记（`src/mito2/src/sst/version.rs`）：

```rust
for file in files_to_remove {
    let level = file.level;
    if let Some(handle) = self.levels[level as usize].files.remove(&file.file_id) {
        handle.mark_deleted();
    }
}
```

真正删除发生在最后一个引用放手时（`src/mito2/src/sst/file.rs`）：

```rust
impl Drop for FileHandleInner {
    fn drop(&mut self) {
        self.file_purger.remove_file(
            self.meta.clone(),
            self.deleted.load(Ordering::Acquire),   // 被标记过才真的删
            self.index_outdated.load(Ordering::Acquire),
        );
    }
}
```

与本项目逐项对上：

| mito2 | 本项目 |
| --- | --- |
| `RegionEdit` 的 `files_to_add`、`files_to_remove` | `apply_compaction_result` 的 `new_ssts` 与输入 `l0_inputs`、`l1_inputs` |
| 复制当前版本的文件集合，在副本上增删，再换成新的 `Arc` | 拿 `state_lock`，复制当前状态，在副本上改，换指针 |
| 按 `file_id` 拿掉 | 按 `sst.id()` 拿掉 L0 的输入 |
| `mark_deleted`，最后一个 `FileHandle` 释放时由 purger 删除文件 | 换状态后立刻 `remove_file`，靠 Unix 的语义让读者读完 |

可以先跳过的部分：`file_purger` 的具体实现（本地文件系统与对象存储两种，删除放到后台的调度器里做）、`index_outdated`（索引文件的更新，与数据文件无关）。

## 常见错误

- **只要最新版本是墓碑就丢掉，不管下面还有没有更旧的版本。** 被删除的值复活。
- **换状态时用开始时的快照覆盖当前状态。** 合并期间 flush 出的文件被抹掉，那段数据从读取中消失。
- **清空 L0 而不是按编号拿掉输入。** 同上。
- **从 L0 只拿新的文件往下合并、留下旧的。** 旧版本留在上层，盖住下层的新版本。
- **扫描时一个来源一个来源地读。** 输出不按 key 排序，还要把整个范围装进内存。
- **串接游标定位时打开了目标之前的文件，或者一开始就打开所有文件。** 结果仍然正确，只是白读了块；文件越多越慢。
- **串接游标的"下一个文件"下标先加一再用。** 跳过第一个文件，最后越界。
- **切分时在跳过墓碑之后也判断"写够了没有"。** 刚换上的空构建器被写成空文件，或者构建器断言"至少要有一条记录"而 panic。
- **跳过墓碑时用 `continue`，漏了让游标前进。** 死循环。
- **拿着状态的写锁再去读状态。** 读写锁不可重入，自己等自己，死锁。
- **换状态之前就删除旧文件。** 这段时间里新的读者取到的状态还指着已删的文件，用到时才打开文件的实现会读失败。
- **测试里手工写的 SST 编号与引擎分配的编号相同。** 块缓存按（编号，块的下标）存块，会把别的文件的块当成自己的。

## 资料

- mini-lsm 教程第二周第 1 章：[week2-01-compaction.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week2-01-compaction.md)；三种放大与两个极端见 [week2-overview.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week2-overview.md)。
- 已学内容中相关的篇：存储笔记[页回收与读者](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/07-page-reclamation-and-readers.md)、[B+Tree 与 LSM](https://github.com/luckysxx/go-btree-kv/blob/main/notes/06-storage-comparisons/03-btree-and-lsm.md)。
- 基准记录：[全量 compaction 的前后对比](../bench/results/2026-10-15-full-compaction.md)。

返回 [本组目录](README.md)。
