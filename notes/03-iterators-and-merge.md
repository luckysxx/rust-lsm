# 迭代器接口与多路归并

状态：部分已整理 · 代码：[iterators.rs](../engine/src/iterators.rs) 的 `StorageIterator`；[iterators/vec_iterator.rs](../engine/src/iterators/vec_iterator.rs)；[mem_table.rs](../engine/src/mem_table.rs) 的 `MemTableIterator`、`first_in_range`；[iterators/merge_iterator.rs](../engine/src/iterators/merge_iterator.rs) 的 `MergeIterator`；[lsm_iterator.rs](../engine/src/lsm_iterator.rs) 的 `LsmIterator`；[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `scan`

## 位置

[全景](README.md#全景)里，这一篇做的是在一摞 memtable 上的范围扫描。上一篇是 [memtable 与跳表](02-memtable-and-skiplist.md)，下一篇是 [数据块格式](04-block-format.md)。这里做出的归并工具之后还用于把 memtable 写成 SST、把几个 SST 合并成新的 SST。

## 问题

点查的规则是"从新到旧找，碰到就停"。范围扫描停不下来：

- 范围里的每个 key 都要找出来，它们散落在各个来源里，任何一个来源都可能有别处没有的 key。所以每个来源都要看。
- 同一个 key 可能在几个来源里各有一份，要的是最新的那份。
- 被删除的 key 不能出现在结果里，而它的墓碑和旧值可能在不同的来源里。

把所有来源里落在范围内的记录倒进一张大的有序表再整个返回，结果是对的，但要把结果全部放进内存，调用方只要前几条也得先整理完全部，flush 与 compaction 这种处理整张表、整批文件的操作也没法用。需要的是"要一条、给一条"。

## 游标

把一个有序的来源看成一张按 key 排好序的清单，游标是按在清单某一行上的一根手指，只做两件事：告诉你它现在指着哪一行；往下挪一行。归并排序里合并两个有序数组时的下标 `i`、`j` 就是两个游标。

| 操作 | 作用 |
| --- | --- |
| 当前的 key、当前的 value | 看手指指着的那一行 |
| 是否有效 | 是否已经走到头 |
| 前进 | 往下挪一行 |

"看当前这一行"与"前进"是分开的两个操作。归并时要反复比较几个游标各自指着的 key，只看不走；把两者合成一步的接口（取出下一条并前进）不合用。

游标不是来源自带的：每来一次扫描，临时在每个来源上各放一个，扫完就丢。

memtable 之所以能提供这样的游标，是因为跳表每次插入都把节点放在正确的位置，最底层始终是一条有序链表；它除了按 key 查找，还能从某个 key 起按顺序往后走。

## 多路归并

k 个来源各有一个游标，各自有序，要合成一个有序的游标：

```
反复做：
    在 k 个游标当前指着的 key 里找出最小的
    输出它
    让那个游标前进
```

LSM 的归并比教科书上的多两条规则。

**key 相同时取最新来源的，其余的也要前进。** 平时比的是 key 的大小，只有 key 相同时才看新旧。输掉的游标必须跟着前进，否则旧版本下一轮还会冒出来。

**墓碑先参与比较，胜出之后才被隐藏。** 如果各个来源的游标先自行滤掉墓碑，较新来源里的墓碑消失了，较旧来源里的值就会在归并时被输出，被删除的数据复活。这与点查中"读到墓碑就停、不能当作没看见"是同一条道理。

例：T0 最新，T2 最旧。

```
T0:  b → 删除   c → 4
T1:  a → 1      c → 3
T2:  b → 2      d → 5

归并的结果：  a → 1   b → 删除   c → 4   d → 5
用户看到的：  a → 1   c → 4   d → 5
```

## 为什么用堆

每一轮都把 k 个游标看一遍，产出一条记录要比较 k 次。但相邻两轮之间只有一个游标前进了，其余的原地没动，上一轮得到的"谁比谁小"在这一轮大部分仍然成立，重新比一遍是浪费。

堆把一部分比较结果记在结构里。它是一棵二叉树，只守一条规矩：每个父亲不大于它的两个孩子（兄弟之间不管）。于是最小的一定在树顶。

```
        1                      6                      2
      /   \                  /   \                  /   \
     4     2      →         4     2      →         4     6
    / \   / \              / \   / \              / \   / \
   9   5 7   8            9   5 7   8            9   5 7   8

  树顶的 1 被输出，       6 与较小的孩子 2 比，    6 与较小的孩子 7 比，
  那个游标前进到 6        更大，交换               更小，停下
```

树顶变了以后，只需要让新的值沿一条路径往下沉，每层比较两次，层数是 log k。另一半子树完全没被碰到，它内部的大小关系直接沿用。k 为 64 时每轮最多约 12 次比较，逐个看要 63 次。

代价没有消失，而是挪到了维护上：开始时要建堆；每次变动都要调整；要多存一棵树，代码更复杂；k 只有两三个时，逐个看反而更快。归并的每一轮恰好一次变动、一次查询，这笔交换是划算的。用维护结构的代价换查询的代价，与 B+Tree、索引是同一种交换。

归并用堆的方式：堆里放游标，按 **（当前的 key，来源的新旧序号）** 比较，序号小的更新。每一轮取出堆顶输出；把堆顶上 key 相同的其余游标逐个取出、前进、放回（走到头的不放回）；被输出的那个游标也前进后放回。key 相同的游标在堆里一定接连出来，清理旧版本因此很顺手。

只有两个来源时直接比较即可，不需要堆，见 [读路径](06-read-path.md)。

## 分层

```
对外的游标          去掉墓碑；到范围的上界就结束
      ↑
归并游标            k 个来源合成一个有序的；同一个 key 只留最新的，墓碑照留
      ↑
单个来源的游标 × k  各自按顺序给出范围内的记录，墓碑原样给出
```

三层是同一套接口，可以一层套一层。以后加上 SST，只是最下面多几种游标。

## 扫描要看多少个来源

全部。扫描开始时的状态快照里有几张表，就放几个游标。memtable 通常只有个位数；放一个游标的代价与一次点查相当；范围内没有数据的来源，游标一放上去就无效，不再参与比较。

数据落盘以后来源会多起来，那时靠两件事减少：每个 SST 记着自己的首末 key，与扫描范围不相交的直接跳过；compaction 把文件整理成层内不重叠，一个范围只会碰到少数几个文件。一次读取要看多少个来源就是读放大。

## memtable 的游标

### 自引用的问题

数组上的游标很简单：数据是它自己的，位置是一个下标。memtable 的游标两样都不同：数据在表里，被多方共用并且还在被写入；跳表里的位置是"指着某个节点"，即一个指向表内部的引用。

扫描函数创建好游标后把它交给调用方慢慢使用，这期间表不能被释放，所以游标要拿着一个 `Arc` 让表活着。于是游标里既要有"表的所有权"，又要有"指向表里面的引用"：

```rust
struct MemTableIterator {
    table: Arc<MemTable>,          // 让表活着
    position: /* 指向 table 内部某个节点的引用 */,
}
```

第二个字段借用的是第一个字段里的东西，这叫自引用结构体。Rust 检查借用的办法是比较借用者与被借用者各自的存活范围，而这里两者在同一个结构体里一起生灭、一起被移动，语言没有办法表达"这个字段借自隔壁那个字段"，所以不允许直接这样写。有垃圾回收的语言没有这个问题。

### 四种做法

| | 做法 | 每步的代价 | 其他代价 |
| --- | --- | --- | --- |
| 一 | 不存位置，只记住当前记录；前进时在表里重新查找"比当前 key 大的第一条" | 一次查找，O(log n) | 无 |
| 二 | 用专门的库（`ouroboros`）生成安全的自引用结构体 | O(1) | 多一个依赖；字段要通过库生成的特殊写法访问 |
| 三 | 游标不拥有表，由调用方持有快照，游标只借用 | O(1) | 每一层游标的类型都要带上借用的标记，对外接口变成"先取快照、再建游标"两步 |
| 四 | 创建游标时把范围内的记录全部拷进一个数组 | 创建时 O(范围大小) | 只要前几条也得拷完；多占一份内存 |

mini-lsm 用第二种（[mem_table.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/mem_table.rs) 的 `MemTableIterator`）。本项目用第一种：游标里只有自己拥有的东西，不引入新的依赖。

实测（100 万条记录的跳表从头扫到尾，release 编译，三轮）：顺着跳表走每步约 70 纳秒，每步重新定位约 140 到 175 纳秒，慢 2 到 2.5 倍。前者没有计入第二种做法每步还要做的克隆，实际差距略小。

这个倍数是与"顺着跳表走"相比的。放进基准里与平衡树相比，差距大得多：范围扫描每条记录在 `BTreeMap` 上约 14 纳秒，在单张 memtable 上约 200 纳秒。跳表逐节点追指针本身就比一个节点里放很多 key 的平衡树慢，重新定位只是其中的两到三倍（见 [结果记录](../bench/results/2026-10-10-engine-and-scan.md)）。

### 导读：mito2 时序 memtable 的迭代器

mito2 的时序 memtable 用的是第一种做法（[memtable/time_series.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/memtable/time_series.rs)）：

```rust
struct Iter {
    series: Arc<RwLock<SeriesMap>>,
    last_key: Option<Vec<u8>>,
    // 其余字段是过滤条件、指标统计等
}

fn next(&mut self) -> Option<Self::Item> {
    let map = self.series.read().unwrap();
    let range = match &self.last_key {
        None => map.0.range::<Vec<u8>, _>(..),
        Some(last_key) => map
            .0
            .range::<Vec<u8>, _>((Bound::Excluded(last_key), Bound::Unbounded)),
    };

    // TODO(hl): maybe yield more than one time series to amortize range overhead.
    for (primary_key, series) in range {
```

| mito2 | 含义 |
| --- | --- |
| `series: Arc<…>` | 用 `Arc` 让数据活着 |
| `last_key: Option<Vec<u8>>` | 只记住上一次给出的 key，不保存位置 |
| `None => map.0.range(..)` | 第一次调用：从头开始 |
| `Some(last_key) => …range((Bound::Excluded(last_key), Bound::Unbounded))` | 之后每次：从比上次的 key 大的地方重新定位 |

它这样做的条件与本项目不同，同一个做法在两边的理由并不一样：

- 它的数据在一把读写锁里，位置依附于读锁的守卫。要把位置留到下一次调用，就得整个扫描期间一直持有读锁：这会挡住需要写锁的写入；标准库的守卫也不能跨线程传递，而它的迭代器类型要求可以。所以每次调用重新拿锁、定位、取完就放。无锁跳表没有这个约束。
- 它每调用一次给出一整条时间线的全部数据点，一次定位的代价摊在很多数据上。代码里的 TODO 考虑的优化方向也是一次多给几条时间线。本项目每步给出一条记录，定位的代价没有东西可摊。

### 本项目的做法

[mem_table.rs](../engine/src/mem_table.rs)：

- `MemTable::first_in_range(lower, upper)`：范围内 key 最小的那条记录，没有则为 `None`。
- `MemTableIterator` 的字段：`table: Arc<MemTable>`、`upper: Bound<Bytes>`、`current: Option<(Bytes, Bytes)>`。下界只在创建时用一次，不保存。
- 创建：`current = first_in_range(lower, upper)`。
- 前进：`current = first_in_range(Excluded(当前的 key), upper)`。
- 有效：`current` 是 `Some`。

范围的边界用标准库的 `Bound`：`Included(k)` 包含 k，`Excluded(k)` 不包含 k，`Unbounded` 没有边界。

表在扫描期间仍可能被写入：排在游标之后的新 key 会被扫到，排在之前的不会。扫描看到的不是某一时刻的快照，这要等到 [快照读](18-snapshot-read.md) 才解决。

## 归并的实现

[iterators/merge_iterator.rs](../engine/src/iterators/merge_iterator.rs)。

堆里放的不是数据，而是游标：每个还没走到头的来源占一项，一项由来源的序号与它的游标组成。归并三张各一百万条记录的表，堆里最多三项；归并占用的内存只与来源的个数有关。

```rust
struct HeapItem<I: StorageIterator> {
    index: usize,   // 来源的序号，越小越新
    iter: I,        // 这个来源的游标
}

pub struct MergeIterator<I: StorageIterator> {
    current: Option<HeapItem<I>>,    // 当前胜出的来源，单独拿在堆外
    heap: BinaryHeap<HeapItem<I>>,   // 其余还没走到头的来源
}
```

每个来源任何时刻只在一个地方：`current`（最多一个）、堆里，或者已经走到头被丢掉。所以堆里的项数是"还有效的来源数减一"。

不变量：

1. `current` 为 `Some` 时里面的游标一定有效，归并游标指着的就是它的当前记录；为 `None` 就是全部走到头。于是 `is_valid` 只需看 `current` 是不是 `Some`。
2. 堆里只放有效的游标，而且每一项都排在 `current` 之后：key 更大，或者 key 相同但更旧。

**比较规则**写在 `HeapItem` 的 `cmp` 里：先比游标当前的 key，相同再比序号，最后整体取反。取反是因为标准库的 `BinaryHeap` 是大顶堆；取反之后 key 小的在顶上，key 相同时序号小的在顶上，"大"就是"应该排在前面"。顺序不能颠倒：序号只是 key 相同时的决胜局，先比序号会让 key 较大的新来源排到 key 较小的旧来源前面，输出不再有序。

**创建**：有效的来源全部放进堆，再把堆顶取出来作为 `current`。

**前进**，四步：

```
1. 堆顶的 key 等于 current 的 key：它是旧版本，让它前进一步；走到头就从堆里拿掉。
   重复，直到堆顶的 key 不同或者堆空了
2. current 前进一步
3. current 走到头：从堆里取出最该排前面的一个作为新的 current（堆空就得到 None），返回
4. 和堆顶比较：堆顶应该排在前面（key 更小，或者 key 相同但更新），就把两者交换
```

- 第 1 步必须在第 2 步之前：清理旧版本要拿 current **还没变的** key 去比；current 还在原处，它的 key 可以直接用，不必复制。
- 旧版本的来源只是前进，不拿走：它后面还有别的 key。只有走到头的才拿掉。
- 第 1 步里看堆顶、让它前进用的是 `BinaryHeap::peek_mut`：借出堆顶而不取出，改动之后放手时堆自动把它挪到该在的位置。要拿走借着的这一个用 `PeekMut::pop(top)`；第 3 步手里没有借着任何东西，直接 `heap.pop()`。
- 第 3 步整个替换 `current`，不和堆顶交换：交换会把一个无效的游标放进堆里，违反不变量 2。
- 第 4 步要比完整的顺序，不能只比 key：current 前进到 b，而一个更新的来源也停在 b 上，只比 key 会认为不必交换，给出旧版本。交换用 `std::mem::swap`，两个位置的内容对调，堆里的项数不变。
- "堆空了"只说明没有别的来源，不说明 current 走到头了；把两者当成一回事，只剩一个来源时后面的数据就丢了。
- 前进出错时，把出错的来源从堆里拿掉再返回错误；归并游标此后不能再使用。

**早先的写法与它的代价。** 最初的实现没有 `current`，堆顶就是当前记录。前进时"记下当前 key → 取出堆顶、前进、放回 → 堆顶的 key 还相同就继续"。取出之后原来那条记录的游标已经前进，所以开头必须把当前 key 复制进一个新分配的 `Vec`，每前进一步都有一次分配与一次出堆入堆。只有一个来源时这些都是白做的。读路径推广到多层（[L10](10-simple-leveled-compaction.md)）之后，扫描在各层的串接游标外面套了这一层归并；全部数据在一层时，每前进一步多出约 30 纳秒，扫描 100 条从 6.9 微秒涨到 10.1 微秒，吞吐降了约三成（[基准记录](../bench/results/2026-10-17-simple-leveled-compaction.md)）。改成把当前来源拿在堆外之后，前进不再分配内存，单个来源时堆是空的，第 1、4 步各看一眼空堆就过去了，扫描回到每秒约 12 万次。另一个更小的改法是在原写法前面加一条捷径：堆里只剩一个来源时原地前进；它只照顾单个来源，多个来源时仍然每步分配。

单个来源里同一个 key 至多一条（memtable 的写入是覆盖），重复的 key 只出现在不同来源之间，所以新旧可以只看来源的序号，清理旧版本的循环也至多转来源个数那么多圈。

### 导读：mito2 的比较规则

mito2 归并时堆里比较的写法（[read/flat_merge.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/read/flat_merge.rs)）与上面是同一个形状：

```rust
impl Ord for RowCursor {
    /// Compares by primary key, time index, sequence desc.
    fn cmp(&self, other: &RowCursor) -> Ordering {
        self.first_primary_key()
            .cmp(other.first_primary_key())
            .then_with(|| self.first_timestamp().cmp(&other.first_timestamp()))
            .then_with(|| other.first_sequence().cmp(&self.first_sequence()))
    }
}

impl<T> Ord for GenericNode<T> {
    fn cmp(&self, other: &GenericNode<T>) -> Ordering {
        // The std binary heap is a max heap, but we want the nodes are ordered in
        // ascend order, so we compare the nodes in reverse order.
        other.cursor.cmp(&self.cursor)
    }
}
```

| mito2 | 本项目 | 说明 |
| --- | --- | --- |
| `first_primary_key().cmp(…)` | `iter.key().cmp(…)` | 先比 key |
| `.then_with(\|\| …timestamp…)` | — | key 相同再比时间戳；时序数据的一行由主键加时间戳确定 |
| `.then_with(\|\| other.first_sequence().cmp(&self.first_sequence()))` | `.then(self.index.cmp(&other.index))` | 再相同比新旧。它比的是每行自带的序号，而且把 `other` 与 `self` 对调了位置，效果是序号大（更新）的排在前面 |
| `other.cursor.cmp(&self.cursor)` | `.reverse()` | 大顶堆要反过来。它用对调两边实现，注释里写的理由与这里相同 |

`then_with` 与 `then` 的区别：前者收一个闭包，只有前面比出相等时才去执行后面的比较；后者不管用不用得上都先算好。后面的比较代价不小时用前者。

## 对外的游标与引擎的扫描

[lsm_iterator.rs](../engine/src/lsm_iterator.rs) 的 `LsmIterator<I>` 包在任意一个游标外面，只做一件事：跳过墓碑。它的不变量是"只要有效，指着的就不是墓碑"，所以有两个时刻要跳：刚创建时，和每次前进之后。跳的时候先确认里面的游标有效，再看它的 value 是否为空。

[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `scan` 把三层接起来：

```
取状态快照
按从新到旧，给每张表建一个游标：先是可写的表，再是冻结列表里的各张表
交给归并
外面包上去墓碑的一层
```

返回的类型是 `LsmIterator<MergeIterator<MemTableIterator>>`，从里往外读正好是三层。每一层都实现同一个接口，调用最外层的方法时逐层转交给里面，与 Go 里 `bufio.NewReader(gzip.NewReader(file))` 的套法相同。对外看它什么都能做，实际上每层只做自己的一件事。

来源的顺序是这里唯一容易错的地方：归并以排在前面的来源为新。

**去墓碑独立成一层的理由**：不是所有用到归并的地方都要去掉墓碑。把几个 SST 合并成新的 SST 时需要归并后的结果，但墓碑必须保留，因为更下层的文件里可能还有这个 key 的旧值。那时只用归并，外面不套这一层。

**扫描看到的是什么**：游标建立在调用那一刻的状态快照上，之后的冻结不影响它，冻结之后写进新表的 key 它看不到。但快照里那张可写的表在被冻结之前仍在接收写入，排在游标之后的新 key 会被扫到。所以它是结构的快照，不是数据的快照。

## 工业对照

**LevelDB、RocksDB、Pebble** 的迭代器都是这种游标式的接口（定位、前进、是否有效、当前的 key 与 value），归并迭代器内部用堆。

**mini-lsm**：接口是 [iterators.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/iterators.rs) 的 `StorageIterator`，四个方法 `key`、`value`、`is_valid`、`next`，`next` 返回 `Result`，因为以后前进一步可能要读磁盘。归并在 [merge_iterator.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/iterators/merge_iterator.rs)：

- 堆里的元素是（来源序号，游标），比较规则是先 key、后序号，再整体取反，因为 Rust 标准库的 `BinaryHeap` 是大顶堆。
- 当前最小的那个游标单独拿在堆外，其余的在堆里。前进时先把堆顶上 key 与当前相同的游标逐个前进，再让当前的游标前进，然后与堆顶比较，必要时交换。本项目现在用的就是这种结构。一处不同：mini-lsm 的当前游标走到头、堆也空了时，仍把那个无效的游标留在 `current` 里，`is_valid` 要去问里面的游标；本项目在走到头时整个换掉 `current`（堆空则为 `None`），`is_valid` 只看是不是 `Some`。另外 mini-lsm 的堆元素里放的是 `Box<I>`，交换时只搬一个指针；本项目直接放游标，交换时搬整个游标结构体，游标只有几十到一百多字节，差别很小。
- 去掉墓碑与检查范围上界在更外面一层（[lsm_iterator.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/lsm_iterator.rs) 的 `LsmIterator`）；再外面的 `FusedIterator` 保证游标无效或出过错之后再调用是安全的。

**GreptimeDB mito2**（[read/flat_merge.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/read/flat_merge.rs)、[read/flat_dedup.rs](https://github.com/GreptimeTeam/greptimedb/blob/v1.2.1/src/mito2/src/read/flat_dedup.rs)）同样用堆做归并，差别来自它是列式的时序引擎：

| | 本项目与 mini-lsm | mito2 |
| --- | --- | --- |
| 流动的单位 | 一条记录 | 一批行（Arrow 的 `RecordBatch`，按列存放） |
| 比较什么 | key，相同时比来源的序号 | 主键、时间戳，相同时比每行自带的序号，序号大的在前 |
| 新旧由什么决定 | 来源的位置：靠前的来源更新 | 每一行写入时分配的序号 |
| 去重在哪 | 归并层里顺手完成 | 归并只负责排序，重复的行相邻输出；后面单独一层按策略去重 |
| 删除 | 最外层去掉空 value | 每行带操作类型，去重之后过滤，由一个开关控制 |
| 额外的优化 | 无 | 两个堆：`hot` 放当前这批数据与归并窗口重叠的来源，`cold` 放不重叠的；`hot` 里只剩一个来源时，整批直接输出，不逐行比较 |

"新旧由每条记录自带的序号决定"也是 LevelDB 一类引擎的做法。靠来源的位置判断，前提是每个来源里一个 key 至多一份；等 key 带上时间戳、同一个来源里可以有同一个 key 的多个版本时，这里也要改成比较记录自带的版本号，见 [带时间戳的 key](16-timestamped-keys.md)。

## 常见错误

- **各来源先滤掉墓碑再归并。** 被删除的值复活。
- **key 相同时只前进胜出的游标。** 旧版本在下一轮被当作新的一条输出。
- **比较时只看 key。** key 相同时谁先出来不确定，可能输出旧值。
- **前进时把当前的 key 也包含在新范围里。** 重新定位得到的还是当前这条，游标原地不动，扫描永不结束。
- **前进时忘了带上界。** 游标越过范围的上界一直走到表尾。
- **扫描时把来源按从旧到新交给归并。** 旧值盖住新值，已删除的 key 重新出现。
- **跳墓碑时先读 value、后判断是否有效。** 走到头之后去读一个无效游标的 value。
- **只在前进之后跳墓碑，创建时不跳。** 第一条记录恰好是墓碑时，它被当作正常记录给了出去。
- **把走到头的游标留在堆里。** 之后比较时会去读一个无效游标的 key。
- **把"堆空了"当成"归并结束了"。** 堆空只说明没有别的来源，当前来源可能还有记录；只剩一个来源时后面的数据全丢。
- **当前来源走到头时与堆顶交换，而不是整个换掉。** 无效的游标进了堆。
- **当前来源前进之后与堆顶比较时只比 key。** 更新的来源停在同一个 key 上时，给出的是旧版本。
- **清理旧版本时把前进过的来源无条件拿掉。** 那个来源后面的记录全丢了；只有走到头的才拿掉。

## 前进可能失败

内存里的游标前进一步不会出错。SST 上的游标走到块尾时要从文件里读下一个块，读文件可能失败，所以接口里的"前进"返回一个结果：成功，或者一个错误。包在外面的游标（归并、去墓碑）把里面那层的错误原样交给调用方；创建时就要前进的游标（去墓碑的那一层要先跳过开头的墓碑），创建本身也可能失败。

约定：前进返回错误之后，游标的状态不确定，调用方不能再使用它。以归并为例，出错的那个来源已经从堆里取出、没有放回，剩下的堆不再代表正确的结果。

归并的"前进"里另有一条用断言守住的前提：每个来源自己是严格有序的，前进之后 key 一定变大。清理旧版本的循环靠"堆顶的 key 是否还等于刚才那个 key"来结束，如果某个来源前进之后 key 没有变，这个循环永远不会停。调试编译下在放回堆之前检查这一点，把卡死变成一句报错。

## 资料

- mini-lsm 教程第一周第 2 章：[week1-02-merge-iterator.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week1-02-merge-iterator.md)。
- 已学内容中相关的篇：存储笔记《扫描与叶子链接》。

返回 [本组目录](README.md)。
