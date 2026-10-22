# 数据块格式

状态：部分已整理（解码时对坏数据的检查未展开）· 代码：[block.rs](../engine/src/block.rs) 的 `Block`；[block/builder.rs](../engine/src/block/builder.rs) 的 `BlockBuilder`；[block/iterator.rs](../engine/src/block/iterator.rs) 的 `BlockIterator`

## 位置

[全景](README.md#全景)里，这一篇属于第 ④ 步：冻结的 memtable 写成一个 SST 文件。块是 SST 里最小的单位。上一篇是 [迭代器接口与多路归并](03-iterators-and-merge.md)，下一篇 [SST 格式](05-sst-format.md) 把很多块加一份索引拼成文件，之后 [读路径](06-read-path.md) 与 [写路径](07-write-path.md) 把它接进引擎。

## 问题：为什么把文件切成块

一张冻结的 memtable（几十 MB，有序）变成文件之后，要能在不把整个文件读进内存的前提下查一个 key，或者从某个 key 起往后扫。

**按顺序一条接一条写下去。** 记录是变长的，不能按下标定位，找一个 key 只能从头解析，最坏要读完整个文件。

**给整个文件加一张偏移表，每条记录一项。** 能二分了，但 100 万条记录的表有几 MB，要么常驻内存，要么每次查都先读它；二分的每一步又都落在文件里一个不相干的位置，约 20 步就是 20 次磁盘读。

**把记录切成一段一段，每段几 KB，只为每一段记一个 key。** 这份稀疏索引很小，可以常驻内存。查找分两步：先在内存里的索引上二分找到段，再用一次磁盘读把这一段读进来，在段内查找。

这一段就是数据块。它与 B+Tree 是同一个两层结构：索引相当于内部节点，块相当于叶子页。块因此成为几件事的共同单位：一次磁盘读读一块，缓存按块缓存，校验和与压缩也按块做。

## 块、SST 与 memtable

```
一张冻结的 memtable（比如 64 MB）
        │ flush
        ▼
一个 SST 文件（几十 MB）
        │ 里面切成
        ▼
约 1.6 万个块（每个约 4 KB）＋ 文件尾部的索引
```

一张 memtable 对应一个 SST 文件。块只是 SST 内部的切分单位，与 memtable 没有直接的对应关系。

触发写盘的是 memtable 写满，不是块写满。块满只是 flush 过程中的一个内部动作：

```
flush 一张冻结的 memtable：
    拿它的游标，从头到尾按顺序走
    每条记录交给当前的块
    块满了 → 封口，接到 SST 的数据后面，在索引里记一项，换一个新的空块
    游标走到头 → 封最后一个块，写索引和文件尾，同步
    整个文件写完并同步之后，才登记进 MANIFEST
```

封好的块是立刻写进文件，还是先攒在内存里最后一次写出，是实现上的选择。mini-lsm 把所有块攒在一个 `Vec<u8>` 里，最后一次写出整个文件（[table/builder.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/table/builder.rs) 的 `build`）；LevelDB、RocksDB 封一个块就往文件里追加一个，文件很大时不必在内存里多留一份。两种做法对外没有区别，因为写到一半的 SST 不算数：只有整个文件写完、同步、登记之后它才存在，中途崩溃留下的半个文件在重启时当作残留删掉。这与 B+Tree 的存储笔记[提交顺序与同步](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/04-commit-order-and-sync.md)是同一个道理：数据页写到一半时，meta 还指着旧版本。

flush 时没有"SST 写满"：memtable 里有多少数据，写出的 SST 就多大。SST 的大小上限出现在 compaction 中，见 [compaction 的执行](09-compaction-execution.md)。

一条刚写入的数据可能要等几十 MB 之后才进 SST，这期间保住它的是 WAL，不是块：

| 时机 | 发生什么 | 笔记 |
| --- | --- | --- |
| 每次写入 | 追加一条 WAL 记录，插入 memtable | [WAL](14-wal.md) |
| memtable 写满 | 冻结，后台 flush 成一个 SST | [写路径](07-write-path.md) |
| flush 过程中每攒够约 4 KB | 封一个块 | 本篇 |

## 两层索引，两个尾巴

"偏移数组"在 SST 里有两层，不要混在一起：

```
一个 SST 文件
┌────────┬────────┬─────┬────────┬───────────────────┬──────────────┐
│  块 0  │  块 1   │  …  │  块 N  │ 块的索引（每块一项）  │  索引从哪开始 │
└────────┴────────┴─────┴────────┴───────────────────┴──────────────┘
     │                             └── 文件的尾巴，打开文件时读一次
     ▼
一个块（约 4 KB）
┌────────────────────────────┬────────────────────────────┬──────┐
│ 记录 0 │ 记录 1 │ … │ 记录 n │ 偏移 0 │ 偏移 1 │ … │ 偏移 n │ 条数  │
└────────────────────────────┴────────────────────────────┴──────┘
                             └── 块的尾巴，每条记录一项
```

| | 文件的尾巴 | 块的尾巴 |
| --- | --- | --- |
| 每项对应 | 一个块 | 块里的一条记录 |
| 内容 | 块的位置、块的首 key | 记录在块内的起点 |
| 大小 | 64 MB 的文件约 1.6 万项 | 4 KB 的块几十项 |
| 何时读 | 打开文件时读一次，常驻内存 | 跟着块一起读进来 |
| 用途 | 找到 key 在哪个块 | 在块内二分 |

两层都放在尾部，理由相同：内容只追加，封口之前不知道一共有多少项，所以把索引写在数据后面，最后再记下项数或起点，读的人从尾巴倒着找回来。文件那一层见 [SST 格式](05-sst-format.md)，本篇只管块里的。

memtable 不需要偏移数组：跳表靠节点之间的指针定位。偏移数组是给"一段连续字节里的变长记录"用的，数据变成字节之后才出现。一次点查在两类来源上走的是两套办法：

```
memtable：跳表，顺着指针找
SST：     内存里的索引二分找到块 → 读进这一块 → 块内用偏移数组二分
```

## 与 B+Tree 页的对照

块内部的问题与存储笔记[节点格式与变长记录寻址](https://github.com/luckysxx/go-btree-kv/blob/main/notes/03-btree-pages/03-node-format-and-record-addressing.md)相同：几 KB 里放有序的变长记录，要能二分。答案的主体也相同：变长记录配一个定长的偏移数组。不同来自四处条件：

| | B+Tree 的页 | LSM 的数据块 |
| --- | --- | --- |
| 修改 | 往中间插入、删除、分裂 | 只在构建时从头到尾追加，封口后不再修改 |
| 输入顺序 | 任意 | 已经有序（来自 memtable 的游标或归并） |
| 大小 | 固定，按页号乘页大小定位 | 只有目标大小，实际长短不一，靠索引里记的位置定位 |
| 条数 | 重建整页时事先知道 | 一条一条加进来，封口前不知道最后有几条 |

于是页里的类型、兄弟页号这些字段都不需要；没有空闲空间的管理；条数与偏移数组从页头挪到了块尾。

## 为什么块的大小可以不固定

页的大小固定，常被归因于磁盘的扇区。扇区决定的其实是另一件事。

**"固定"来自原地修改与重用。**

- 页会被释放再重用。空出来的位置要能放下将来任何一张页，所以所有页一样大。大小不一的话，文件内部就需要一个处理碎片的分配器，与定长分配和 `malloc` 的差别相同。
- 页靠页号定位，位置是"页号 × 页大小"，不需要查表。大小不一就得另存一张"页号 → 位置与长度"的表，这张表本身还要随每次修改更新并持久化。

**"是扇区或系统页的整数倍"才与硬件有关。** 它关系到三件事：一次写入跨越几个扇区（撕裂写，见存储笔记[文件格式与元数据校验](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/03-file-format-and-meta-validation.md)）；mmap 以系统页为单位映射；绕过页缓存的直接 I/O 要求对齐。普通的文件读写没有对齐的要求，文件系统允许读写任意的字节范围，由内核去读覆盖它的那些扇区。

块不满足"固定"的两个前提：

- 块写出后不修改，也不单独释放，整个文件一起删除。没有重用，也就不需要统一大小。
- 位置由文件尾部的索引给出。这张表只在文件封口时写一次，之后不变，维护它没有代价。

崩溃时写了一半的块也不成问题：整个文件尚未登记，一起作废。另一个让块长短不一的原因是压缩：同样 4 KB 的内容，压缩后长度各不相同。

大小并非没有限制：

- 长度与偏移字段的宽度决定了单条记录与单个块的上限。
- 块是整个读进内存的。块越大，查一个 key 要读进来的无关数据越多；块越小，索引越大。目标大小取几 KB，与系统页、SSD 页的大小相近，一次读取大致是一个 I/O 单位。硬件在这里影响的是目标值的选取，不是对齐。

不对齐的代价是一个块可能跨两个系统页，读它要碰两页而不是一页。

## 块满与过大的记录

块的构建器只认识自己这一个块：

```
add(key, value)
    放得下 → 追加，返回"已加入"
    放不下 → 什么都不做，返回"没有加入"
```

开下一个块不是它的事。块封口时要在 SST 的索引里记一项（位置、首 key），这是文件一级才知道的信息，所以由 SST 的构建器拿到"没有加入"后封口、记索引、新建一个空的构建器，再把那条记录重新加进去。这与 B+Tree 的分层相同：页一级的函数只管一页，放不下就把结果交回去，由树一级更新父节点。

单条记录比目标大小还大时：**块是空的就无条件收下**，这条记录独占一个块，这个块比目标大小大。目标大小只是决定何时封口的阈值，所以不需要 B+Tree 那样拒绝过大的记录或使用溢出页。

把大 value 另存一处、原处只留指针的做法在 LSM 中也有（WiscKey、RocksDB 的 BlobDB），动机是 compaction 反复重写大 value 带来的写放大，与块的大小无关。

## 块内定位

块上的游标只提供一种按 key 的定位：**停在第一条 key ≥ 给定 key 的记录上**；没有这样的记录时游标无效。给定 key 是调用方这一次要找的 key：点查时是被查的 key，范围扫描时是范围的下界。它不是存在文件里的东西，不要与索引里记的"块的首 key"混淆，后者用在上一层，用来选块。

一次点查 `get("e")` 走过两层：

```
索引（每块的首 key）：   块0: b     块1: k     块2: t
    找最后一个首 key ≤ e 的块 → 块 0

块 0 的内容：  b  d  f  h
    定位到第一条 key ≥ e 的记录 → f
    f ≠ e → 这个 SST 里没有 e
```

块里是 `b d f h` 时：

| 给定 key | 游标停在 | 点查的结论 | 扫描的结论 |
| --- | --- | --- | --- |
| `d` | `d` | 相等，找到 | 从 `d` 开始往后走 |
| `e` | `f` | 不相等，这里没有 | 从 `f` 开始往后走 |
| `a` | `b` | 不相等，这里没有 | 从 `b` 开始往后走 |
| `z` | 无效 | 这里没有 | 这个块里没有范围内的数据，换下一个块 |

只提供"≥"而不另设"恰好等于"：点查要的相等可以在定位之后多比较一次得到；扫描的下界不一定是存在的 key，用"恰好等于"得不到起点。前者能推出后者，反过来不行，一个定位操作就同时服务点查与扫描。Go 的 `sort.Search`、B+Tree 游标的定位、[memtable 游标](03-iterators-and-merge.md#本项目的做法) 的 `first_in_range` 都是这个语义。

"游标无效"在实际中会出现：给定 key 落在一个块的末 key 与下一个块的首 key 之间时，索引选中前一个块，块内定位走到了头，由外层换到下一个块的第一条。

## 读回内存后的形态

从磁盘读回的块不还原成跳表或逐条的 `(key, value)`，而是保留原始字节，另外只把尾部的偏移数组解出来；查找时直接在这段字节上二分，用到哪条解析哪条。逐条解析要为每条记录分配内存，而一次查找只碰其中几条。

块的解码只需要一段字节，这段字节是 `pread` 读来的还是 mmap 映射来的都一样；读块的方式在 [SST 格式](05-sst-format.md) 中决定。

## 格式

```
┌──────────────────────────────┬─────────────────────────────┬──────┐
│ 记录 0 │ 记录 1 │ … │ 记录 n-1 │ 偏移 0 │ 偏移 1 │ … │ 偏移 n-1 │ 条数 │
└──────────────────────────────┴─────────────────────────────┴──────┘
  数据区                          偏移数组                       尾

一条记录：  共有前缀的长度 │ 剩余部分的长度 │ key 的剩余部分 │ value 长度 │ value
```

所有整数（三个长度、偏移、条数）都是 2 字节，大端。

**key 做了前缀压缩。** 每个 key 只存与块里**第一个 key** 不同的那部分：完整的 key 是"第一个 key 的前（共有前缀的长度）个字节"接上"剩余部分"。第一条记录的共有前缀为 0，key 完整存放。每个 key 只依赖第一个 key，任何一条记录都能单独还原，所以偏移数组仍然每条一项、二分直达。原理与取舍见 [Bloom filter 与 key 前缀压缩](08-bloom-filter-and-prefix-compression.md#key-前缀压缩)。

**偏移存的是起点。** 偏移 i 是记录 i 在数据区里的起始位置，第一项恒为 0。B+Tree 的页存的是终点，因为那里要靠偏移数组算出记录的长度与页的已用空间；这里记录的长度由它自己的三个长度字段给出，数据区的终点就是偏移数组的起点，存起点可以直接用。

**从尾部找回各部分：**

```
条数          = 最后 2 字节
偏移数组的起点 = 总长度 − 2 − 条数 × 2
数据区        = 偏移数组起点之前的全部字节
```

**大小。** 编码后的长度是 `数据区 + 条数 × 2 + 2`。每条记录的固定开销是 8 字节：三个长度字段加偏移数组里的一项；每个块的固定开销是 2 字节的条数。key 16 字节、value 100 字节、没有共有前缀时一条记录占 124 字节；基准里的 key 与块的第一个 key 平均共有约 12 字节，一条记录约 112 字节，4096 字节的块放约 36 条。

加入一条记录之前的判断：`现在的大小 + 记录的大小 + 2 ≤ 目标大小`。那个 2 是偏移数组里新增的一项，容易漏掉。块是空的时候不做这个判断。

**字段宽度带来的上限。** 单个 key、单个 value 最大 65535 字节；块的目标大小也不超过 65535，否则靠后的记录的起点记不进 2 字节的偏移。过大的记录独占一个块时它的偏移是 0，不受影响。这两个上限要在引擎接受写入的入口处检查，不能等到 flush 时才发现。

**字节序。** 多字节的整数先写高位字节还是低位字节，由格式规定，不能由机器决定，否则换一种 CPU 就读不出来。大端（高位在前）与小端都可以，读写两边一致即可。本项目的 B+Tree 页用小端，这里随 mini-lsm 用大端。

**例子**：`cat → meow`、`cow → moo`，共 30 字节。

```
       00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F
0x00 │ 00 00 00 03 63 61 74 00 04 6D 65 6F 77 00 01 00 │ ....cat..meow...
0x10 │ 02 6F 77 00 03 6D 6F 6F 00 00 00 0D 00 02       │ .ow..moo......
```

- `00 00`、`00 03`、`63 61 74`：共有前缀 0，剩余部分长度 3，`cat`。
- `00 04`、`6D 65 6F 77`：value 长度 4，`meow`。记录 0 占 `[0, 13)`。
- `00 01`、`00 02`、`6F 77`：与第一个 key 共有 1 字节（`c`），只存 `ow`。
- `00 03`、`6D 6F 6F`：value `moo`。记录 1 占 `[13, 24)`。
- `00 00`、`00 0D`：两条记录的起点 0 和 13。
- `00 02`：条数。

墓碑的 value 长度为 0，后面没有 value 的字节；记录照常占一项偏移。

## 与 LevelDB 的 restart point 对照

LevelDB 的块也是"记录在前、索引在尾、最后是项数"，但索引不是每条记录一项。

它的一条记录是 `共享长度 │ 非共享长度 │ value 长度 │ key 的后缀 │ value`：key 只存与**前一条** key 不同的后缀，三个长度用变长整数。有序的 key 相邻两条往往前缀相同，这样省下很多字节。代价是一条记录的 key 要靠前一条才能还原，无法从任意一条开始解析。

所以每隔若干条（默认 16）存一条完整的 key，这个位置叫 restart point；块尾的数组只记 restart point 的偏移（每项 4 字节），最后是它们的个数。定位时先在 restart point 上二分，再从选中的那一个开始顺序解析，最多十几条。

| | 本项目与 mini-lsm | LevelDB |
| --- | --- | --- |
| 尾部索引的密度 | 每条记录一项 | 每 16 条一项 |
| 摊到每条记录的索引开销 | 2 字节 | 0.25 字节 |
| 长度字段 | 定长 2 字节 | 变长整数，小的值 1 字节 |
| key 的压缩 | 相对块的首 key，见 [Bloom filter 与 key 前缀压缩](08-bloom-filter-and-prefix-compression.md) | 相对前一条 key |
| 定位 | 二分直达 | 二分到一段，再顺序解析 |

这是同一种交换出现的第三层：文件里每个块记一个 key，块里每 16 条记一个完整的 key，都是用稀疏的索引加一小段顺序查找，换索引的体积。相对块的首 key 压缩是折中：每条记录都只依赖首 key，仍然可以每条一项、二分直达，但压缩的效果不如相对前一条。

## 本项目的做法

三个角色各占一个文件，目录的组织与 `iterators.rs` 加 `iterators/` 相同。

**构建器**（[block/builder.rs](../engine/src/block/builder.rs) 的 `BlockBuilder`）：字段是数据区 `Vec<u8>`、偏移数组 `Vec<u16>`、块的第一个 key 和目标大小。`add` 先算出与第一个 key 的共有前缀，按压缩之后的大小判断放不放得下，再记下起点（追加之前数据区的长度）、按格式追加五段；块里的第一条加入时记下它的 key。`build` 把两个数组原样交给 `Block`，不拷贝。`add` 标了 `#[must_use]`：不看返回值就可能丢掉一条没加进去的记录。它不检查 key 的顺序，由调用方保证。

**块**（[block.rs](../engine/src/block.rs) 的 `Block`）：字段就是数据区与偏移数组。`encode` 在数据区后面接上偏移数组与条数；`decode` 先检查结构（见 [SST 格式](05-sst-format.md#不信任文件的内容)），再从尾部倒着找回两部分，数据区拷贝一份。`entry(index)` 按偏移找到记录，读出共有前缀的长度、剩余部分与 value，后两者借自数据区；`key_at(index, buf)` 把完整的 key 拼进调用方给的缓冲区，`value_at(index)` 只取 value。记录的格式只有这三个文件知道，块之外只通过构建器的 `add`、块的编解码和游标访问记录，改格式时改动不出这个模块。

**块上的游标**（[block/iterator.rs](../engine/src/block/iterator.rs) 的 `BlockIterator`）：字段是 `Arc<Block>`、一个下标和一个 `Vec<u8>`。块是一段不再变化的字节，位置用"第几条"表示就够了，没有 [memtable 游标](03-iterators-and-merge.md#自引用的问题) 那样的自引用问题。压缩之后完整的 key 在块里不是连续的一段，`key()` 要返回 `&[u8]` 就只能借游标自己的缓冲区：每次移动位置之后把当前的 key 拼进去，缓冲区反复使用，容量够时不再分配内存。二分时也用它临时拼中间那条的 key 再比较。`value` 仍然直接借自块。它实现的是统一的游标接口，可以直接交给归并。

格式里的整数是 `u16`，内存里做下标运算用 `usize`：写出时由 `usize` 转 `u16`（之前的断言保证放得下），读入时立刻转成 `usize`，此后不再转换。见 Rust 笔记《数值转换与溢出处理》。

### 二分的写法

"第一条 key ≥ 给定 key"的二分，把下标分成三段来想，边界就不用记：

```
下标：   0 … low-1   │   low … high-1   │   high … n-1
         已知：小        还不知道            已知：不小
        (key < target)                    (key ≥ target)
```

每一轮从中间那段取一个 `mid` 看一眼，把它归到左边或右边；中间那段空了就结束，答案是右边那段的第一个。

| 问题 | 答案 | 理由 |
| --- | --- | --- |
| 初始值 | `low = 0`，`high = n` | 一开始什么都不知道 |
| 循环条件 | `low < high` | 中间那段还有东西 |
| `mid` 小 | `low = mid + 1` | `low` 是左边那段之后的第一个 |
| `mid` 不小 | `high = mid` | `high` 是右边那段的第一个本身 |
| 结束时的答案 | `low` | 中间空了，`low` 与 `high` 相等 |
| 全都小 | 答案是 `n` | 右边那段是空的，游标无效，不需要特殊处理 |

这种写法里没有 `<=`，没有 `high - 1`，不需要在循环里判断相等：相等属于"不小"，照常归到右边。Go 的 `sort.Search` 内部是同一个模板。

## 工业对照

**mini-lsm**（[block.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/block.rs)、[block/builder.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/block/builder.rs)、[block/iterator.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/block/iterator.rs)）：块尾的布局、字段宽度与字节序和本项目相同。不同之处：

- 记录格式与本项目相同，也是相对块的首 key 做前缀压缩。
- 它的游标同样把拼好的 key 存在自己里面，value 只记起止位置；它另外缓存了块的首 key，本项目每次从第一条记录里取。
- 解析记录时不算下标，而是拿着一段"还没读的字节"，读走多少就把它缩短多少（`bytes` 库的 `Buf`）。
- 解码时逐项检查偏移递增、长度不越界，坏数据返回错误。

**GreptimeDB mito2** 没有自己的数据块格式。它的 SST 是 Parquet 文件（[sst/parquet/writer.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/sst/parquet/writer.rs)），数据按列存放，文件里的单位是 row group 和其中每一列的 page，由 Parquet 的库负责编码。两边可以对上的是：

| | 本项目 | mito2 |
| --- | --- | --- |
| 文件里的单位 | 块，约 4 KB，装着若干行的 key 与 value | row group，默认 102400 行（[sst/parquet.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/sst/parquet.rs) 的 `DEFAULT_ROW_GROUP_SIZE`），内部按列分成 page |
| 何时封口 | 加入之后会超过目标字节数 | 行数达到上限 |
| 块内定位 | 偏移数组上二分 | 用每个 row group、每个 page 的最小最大值统计跳过不相关的部分，再按列解码 |
| 压缩 | 目前没有 | 每个 page 用 ZSTD 压缩 |

差异来自数据模型：时序查询通常是"某段时间内、某几列"的大范围扫描加聚合，按列存放并成批解码合算；按 key 逐条点查不是它的主要负载。

### 为什么 mito2 不自己定数据块的格式

LSM 的结构（memtable、flush、compaction、manifest、WAL）与文件内部的格式是两件事。mito2 自己写了前者，后者选了现成的 Parquet。理由有三层：

- **负载适合按列存放。** 一行是"若干标签列、一个时间戳、若干数值列"，查询多是对几列做大范围的扫描与聚合。按行存放的块（key、value 首尾相接）读一列也要把整行读进来；按列存放只读用到的列，同一列的值类型相同、彼此相近，压缩率高得多，解出来就是可以成批计算的数组。
- **不必重做。** Parquet 是成熟的开放格式，Rust 里有现成的读写库，统计信息（每个 row group、每个 page 的最小最大值）与按条件跳过都已具备；mito2 的查询引擎 DataFusion 本来就以 Arrow 的列式批次为处理单位，读出来不需要转换。mito2 的依赖里直接有 `parquet` 与 `datafusion`（[Cargo.toml](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/Cargo.toml)）。
- **文件能被别的系统读。** 它的数据放在对象存储上，Parquet 文件可以被其他分析工具直接读取。源码里专门为列写入了与 Iceberg 兼容的字段编号（[sst.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/sst.rs) 的 `PARQUET_FIELD_ID_KEY`）。

代价是按 key 找一行不方便：Parquet 里没有"每行一项"的偏移，最细只能靠统计信息定位到 page，再把这一页解码。mito2 为此另建索引（倒排索引、Bloom filter 等），存在与 SST 配套的 puffin 文件里。

Parquet 文件内部仍然是同一套思路，只是多了"列"这一维：

```
本项目：  文件 → 块 → 记录                          尾部：每个块的位置与首 key
Parquet： 文件 → row group → 列 → page → 值         尾部：每个 row group、每列的位置与统计信息
```

元信息同样写在文件末尾，最后是它的长度与魔数，读的时候从尾巴倒着找。

### 导读：puffin 文件的尾部

GreptimeDB 里手写的、与"从尾部倒着找"对应的代码在索引文件的格式里。mito2 的索引（倒排索引、Bloom filter 等）存在 puffin 格式的文件中，读文件尾的代码在 [puffin/src/file_format/reader/footer.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/puffin/src/file_format/reader/footer.rs)：

```rust
/// Footer layout: HeadMagic Payload PayloadSize Flags FootMagic
///                [4]       [?]     [4]         [4]   [4]

// check the magic
let magic = Self::read_tailing_four_bytes(&suffix)?;
ensure!(magic == MAGIC, MagicNotMatchedSnafu);

let flags = self.decode_flags(&suffix[..suffix_len - MAGIC_SIZE as usize])?;
let length = self.decode_payload_size(
    &suffix[..suffix_len - MAGIC_SIZE as usize - FLAGS_SIZE as usize],
)?;

fn read_tailing_four_bytes(suffix: &[u8]) -> Result<[u8; 4]> {
    let suffix_len = suffix.len();
    ensure!(suffix_len >= 4, InvalidPuffinFooterSnafu);
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&suffix[suffix_len - 4..suffix_len]);

    Ok(bytes)
}

fn decode_payload_size(&self, suffix: &[u8]) -> Result<u64> {
    let payload_size = i32::from_le_bytes(Self::read_tailing_four_bytes(suffix)?);

    ensure!(payload_size >= 0, /* … */);
    let payload_size = payload_size as u64;
```

| puffin | 本项目的 `decode` | 说明 |
| --- | --- | --- |
| 注释里的布局：变长的内容在前，它的长度在后 | 偏移数组在前，条数在后 | 同一个理由：写的时候最后才知道长度 |
| `read_tailing_four_bytes`：`suffix[suffix_len - 4..suffix_len]` | `read_u16(encoded, encoded.len() - 2)` | 取最后几个字节 |
| `&suffix[..suffix_len - MAGIC_SIZE as usize]` 之后再取"最后 4 字节" | 先算出 `count_at`、`offsets_start` 两个位置 | 它每读一段就把切片的尾巴切掉一段，再读新的尾巴；本项目算下标 |
| `i32::from_le_bytes` | `u16::from_be_bytes` | 它是 4 字节、小端 |
| `payload_size as u64` | `as usize` | 读进来立刻转成做运算用的宽度 |
| `ensure!(…)` | 目前没有 | 不信任输入：条件不成立就返回一个错误，而不是 panic |

可以先跳过的：`async`、`.await` 与预取（它读的是远端的文件，一次多读一些尾部以省掉一次请求）；`.context(ReadSnafu)?` 是它的错误处理写法；`Flags` 与 LZ4 是尾部内容的压缩。

## 常见错误

- **记录里四段的顺序与格式不符。** 例如写成两个长度在前、key 与 value 在后。那也是一种可行的格式，但读写两边必须是同一种；编码的测试逐字节比较就是为了钉住这一点。
- **判断放不放得下时漏掉偏移数组里新增的一项。** 块会比目标大小多出 2 字节。
- **把条数也算成每条记录的开销。** 条数每个块只有一份。
- **空块也做大小判断。** 过大的记录永远加不进去，调用方换多少个新块都一样。
- **放不下时已经改动了构建器。** 例如先记了偏移再判断。返回"没有加入"时必须保持原样。
- **"最后 2 个字节"的起点写成 `len - 1 - 2`。** `len - 1` 是最后一个字节的下标，最后 n 个字节从 `len - n` 开始。读错的条数随后会让无符号数相减下溢。
- **value 的长度从记录的固定位置读。** 它紧跟在 key 后面，位置取决于 key 的长度，只能先读 key 的长度再算。
- **二分找成了"第一条大于"。** 给定的 key 存在时会跳过它，点查就找不到已有的 key。
- **定位从游标当前的位置往后找。** 往回定位时得到错误的结果；定位与之前的位置无关。
- **在只读的方法里克隆 `Arc` 再从副本借出引用。** 借出的字节要从游标自己拿着的那个 `Arc` 出发，见 Rust 笔记《返回引用与生命周期标注》。

## 大纲

- 解码时对坏数据的检查（未展开）。

## 资料

- mini-lsm 教程第一周第 3 章：[week1-03-block.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week1-03-block.md)。
- 已学内容中相关的篇：存储笔记[节点格式与变长记录寻址](https://github.com/luckysxx/go-btree-kv/blob/main/notes/03-btree-pages/03-node-format-and-record-addressing.md)、[页内布局对照](https://github.com/luckysxx/go-btree-kv/blob/main/notes/06-storage-comparisons/01-page-layout-alternatives.md)。

返回 [本组目录](README.md)。
