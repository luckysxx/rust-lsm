# 简单分层 compaction

状态：已有正文 · 代码：[compaction/simple_leveled.rs](../engine/src/compaction/simple_leveled.rs) 的 `generate_task`、`pick_upper_level`；[lsm_storage/compact.rs](../engine/src/lsm_storage/compact.rs) 的 `compact_once`、`compact_task`、`apply_compaction_result`；[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `levels`、`get`、`scan`；[db.rs](../engine/src/db.rs) 的 `compaction_loop`、`stop_thread`

## 位置

[全景](README.md#全景)的第 ⑤ 步开始自动运转。[L09](09-compaction-execution.md) 做了合并的**执行**：给定一组文件，合并、换状态、删旧文件，但只能手动触发。这一篇补上**决策**：引擎看当前状态，自己决定要不要合并、合并哪两层，后台线程反复去做；状态也从 L0 加 L1 推广成 L0 加固定数目的几层。下一篇 [L11](11-tiered-compaction.md) 换一种决策（tiered），[L12](12-leveled-compaction.md) 是真正的 leveled（只合并一部分文件、按字节比较、基准层），执行部分三篇共用。

## 与 B+Tree 阶段的对应

| B+Tree | LSM |
| --- | --- |
| 整理（页的分裂与合并）在写入路径上同步完成，写入要等它 | 整理交给后台线程，写入不等；整理跟不上时要靠写入停顿反过来限制写入 |
| 结构只有一种形状，没有"什么时候整理"的问题 | 什么时候合并、合并哪些文件是一种可以替换的策略，决定三种放大怎样取舍 |
| 一个写者加多个读者，后台没有线程 | 写入、flush 线程、compaction 线程同时改动状态，只在换状态的一瞬间排队 |

## 问题：手动合并不够

- **一直不合并**：L0 文件越积越多，读越来越慢（[L09 的基准](../bench/results/2026-10-15-full-compaction.md)：55 个 L0 文件时扫描每秒 0.72 万次，合并成一层后 13.8 万次）。
- **每攒几个文件就全量合并**：每次都要重写整个数据库，代价与总数据量成正比，与这次新写入多少无关。数据库 1 GB、memtable 4 MB、每攒 4 个 L0 文件（16 MB）全量合并一次：一共合并 64 次，第 k 次重写约 k × 16 MB，合计约 32 GB，写放大约 32；数据涨到 10 GB 时约 320，随数据量线性增长。

要自动，而且每次只碰一小部分数据。

## 分层：小层吸收新数据，大层少被重写

把数据按新旧分成几层，每层是上一层的若干倍（这个倍数叫 fanout 或 size ratio）：

```
L1   16 MB      新数据只和它合并：小，合并便宜，可以经常做
L2   160 MB     L1 满了才整体并下来，次数少得多
L3   1.6 GB     更少
```

- **写**：频繁的合并只碰小层；大层也要重写，但次数少，成本摊到了很多数据上。一条数据从上往下沉，每过一层大约被重写 fanout 次，层数是总量的对数，所以写放大约为 **fanout × 层数**，数据涨 10 倍只多一层。这与归并排序是同一个道理：只有一层时，每来一批新数据就要把整个有序数组重抄一遍（O(n²)）；分层之后每个元素只被抄 log n 次。
- **读**：L1 往下每层是一个 sorted run，每层最多看一个文件，读放大约为"L0 的文件数 + 层数"。
- **空间**：最底层装着大部分数据，而且已经去重，空间放大约 1 + 1/fanout。
- **墓碑**只在合并进最底层时才能丢：合并 L1 与 L2 时，L3 里可能还有这个 key 的旧值，要靠墓碑盖住（[L09](09-compaction-execution.md#墓碑什么时候可以丢)）。
- **上层的作用是缓冲**：即使上下两层没有共同的 key，什么也去不掉，也要往下合并，为的是让上层保持小，下一次 L0 并进来时仍然便宜。这种情况下合并的代价全部是写放大，没有空间上的回报，是 leveled 最吃亏的负载。

几种做法的数量级（1 GB 数据、memtable 4 MB 的粗算）：

| 做法 | 写放大 | 点查最多看几个文件 | 空间放大 |
| --- | --- | --- | --- |
| 不合并 | 1 | 全部 L0（约 256 个） | 覆盖写几轮就是几倍 |
| 每次全量合并 | 约 32，随数据量线性增长 | L0 几个 + 1 | 约 1 |
| leveled（fanout 10，3 层） | 约 10～30，对数增长 | L0 几个 + 层数 | 约 1.1 |
| tiered（[L11](11-tiered-compaction.md)） | 约等于层数 | 每层可能有好几个 run，最多 | 最高可到 2 |

leveled 用写换读和空间，tiered 反过来。

## 简单分层策略

教程里最朴素的 leveled，三处简化：一次合并**两侧整层**（上层的全部文件并进下层的全部文件）；用**文件数**代替字节数；层数固定。

参数：

- `level0_file_num_compaction_trigger`：L0 的文件数达到它，就把 L0 与 L1 合并。
- `size_ratio_percent`：相邻两层，下层的文件数不到上层的这个百分比，就把上层整层并进下层。
- 层数：本项目不放在策略参数里，直接取状态里的层数（选项 `max_levels`），只有一个来源。

决定合并哪一对：

1. L0 达到触发值，先做 L0。L0 的文件范围互相重叠，读的时候要逐个看，又是写入停顿的触发条件，是最直接拖慢前台的一层。
2. 否则从 L1 开始从上往下，找第一对"下层 ÷ 上层 < 比例"的相邻层。
3. 上层是空的不选：没有东西可以往下并，选了就会一直生成空任务。最底层没有下一层，不选。恰好等于比例算够了。

比例可以写成整数比较 `下层 × 100 < 上层 × 比例`：上层为 0 时右边是 0，不等式自然不成立。

例子，比例 200%：L1 有 2 个文件、L2 有 3 个、L3 有 8 个。L2/L1 = 150%，不够，合并 L1 与 L2；L3/L2 = 266%，够了。

**一定会停下**：每个任务都把至少一个文件往下移一层，最底层的文件不再移动，所以反复生成任务总会到"没有任务"。

**固定顺序不一定最省。** 比例 200%，各层 `[L0 0, L1 2, L2 2, L3 2]`：从上往下先做 L1→L2（写 4 个文件）再做 L2→L3（写 6 个），共 10 个；从下往上先做 L2→L3（写 4 个）再做 L1→L2（写 2 个，L2 已空），共 6 个，停下的形状也不同。长期哪种更省要模拟或实测。真实的引擎不用固定顺序，而是给每层算一个分数（离目标有多远），先做分数最高的，见工业对照。

## 策略与执行分开

**控制器只读状态、只给出任务**，不写文件、不拿锁、不改状态。执行在 `lsm_storage` 里。好处是策略可以不碰磁盘地测试，换一种策略也不必改执行。

任务记五样东西：上层是哪一层、上层参加合并的文件、下层是哪一层、下层参加合并的文件、下层是不是最底层。记具体的文件而不是只记"合并 L1 与 L2"，因为三处要用：

- **执行**：只读这些文件。执行到一半再去看当前状态，可能已经多了新文件。
- **换状态**：合并期间 flush 会往 L0 加文件；只知道"合并了 L0"就只能清空 L0，新文件跟着丢了（[L09 的丢失更新](09-compaction-execution.md#执行的步骤与并发)）。
- **删除**：删的正是这些文件。

**每次只给出一个任务**，即使好几对层都不满足比例。做完第一个以后状态变了，其余几个的文件列表和比例都已经过期，只能扔掉重算；而整层合并时相邻的两个任务必然共用一层，也不可能同时做。所以策略只给最该做的那一个，调用方做完再问，直到"没有任务"。即使允许并行合并，RocksDB 也是每次挑一个任务、把挑中的文件标记为正在合并、再挑下一个，而不是一次给出一批。

决策还可以再拆一层：一个只看各层文件数的纯函数决定"合并哪一层"，另一个函数把那两层的文件列表抄进任务。前者用几个数字就能测，也能做模拟：每次 flush 让 L0 加一个文件，反复要任务、把上层的文件数加到下层，看形状怎样演变、会不会停不下来。模拟的开头是这样的（触发值 2、比例 200%、3 层）：

```
[L0 2, L1 0, L2 0, L3 0]
  → [0, 2, 0, 0]   L0 并进 L1：真合并，L0 的文件互相重叠
  → [0, 0, 2, 0]   L1 并进空的 L2：原样复制
  → [0, 0, 0, 2]   L2 并进空的 L3：原样复制
```

## 多层的状态

**L0 单独放，L1 往下放进一个数组**（`levels[0]` 是 L1）。另一种做法是全放进一个数组，L0 是第 0 项（RocksDB）。两种做法都省不掉对 L0 的特殊处理，因为 L0 与其他层守的是两套不变量：

| | L0 | L1 往下 |
| --- | --- | --- |
| 排列 | 按新旧，新的在前 | 按 key |
| 文件之间 | 范围可以重叠 | 范围互不重叠 |
| 点查 | 每个文件都要看 | 每层只看一个候选 |
| 扫描 | 每个文件一个游标，再归并 | 一层一个串接游标 |
| 合并期间 | 会被 flush 加入新文件 | 只由 compaction 改动 |

区别在出错的后果：L0 单独放的代价是层号与下标差一（Ln 在 `levels[n - 1]`），写错时通常越界 panic 或测试立刻失败；放在一个数组里时，遍历各层容易忘记 L0 特殊，比如对 L0 用"按 key 找候选文件"，结果**静默**地读错数据。存储引擎宁可要前一种。

空的层也占一项，`levels[i]` 永远是 L(i+1)。

**读路径**：

- 点查查完 L0 之后**从上往下**逐层查，每层用 `find_sst_index` 找到唯一的候选文件再查；上层的数据更新，查到就停。
- 扫描每层建一个串接游标（只放与范围相交的文件），各层再用归并游标合起来，上层排在前面。层与层之间可能有同一个 key，必须归并，不能串接。

## 执行一个任务

- **上层是 L0**：L0 的文件范围重叠，用归并游标；**上层是 Ln**：与下层一样用串接游标。上层排在两路归并的前一个位置，同一个 key 上层胜出。两种上层游标的类型不同，拼出的两路归并也是不同的类型；Rust 的泛型在编译时为每种具体类型各生成一份代码，所以分两支各写一次文件（写文件的函数是泛型的，两种都接受）。也可以装进 `Box<dyn …>` 统一成一个类型，只有两支时不值得。
- **墓碑**：下层是最底层才丢，否则照写。
- **换状态**：上层是 L0 时按编号拿掉输入，合并期间新 flush 出的留下；上层是 Ln 时断言它还是任务里记下的文件，然后清空；下层断言还是任务里记下的文件，然后整层换成新文件；其他层不动。

断言成立的前提是**同一时刻只有一个合并**：L1 往下的层只由 compaction 改动。这个前提哪天被打破（比如允许并行合并），断言会立刻报出来，而不是悄悄覆盖别人的改动。断言的失败信息要带上层号，出问题时一眼看出是哪一层。

**手动的全量合并放进最底层**，而不是 L1。单论这一次，放进 L1 也对：各层都参加了合并，L1 下面已经没有更旧的数据。但之后的自动合并会看到"L2 是空的、比例为 0"，立刻把整个 L1 并进 L2，再并进 L3，整个数据库白白多写两遍。RocksDB 手动发起的全量合并（`CompactRange`）也放进最底层。

### 并行合并要多管什么

本项目只有一个 compaction 线程。允许几个合并同时进行时：

- **用处**：紧急的 L0→L1 不必排在一个要跑几分钟的 L3→L4 后面；部分合并（L12）时，同一层里范围不相交的几段可以各自合并；单线程的合并吞吐有上限（本机约每秒 270～400 MB），而用户能持续写入的速度约等于合并吞吐除以写放大。
- **约束**：一个文件同时只能被一个合并使用（RocksDB 标记 `being_compacted`）；两个合并写进同一层的输出范围不能重叠；L0 的文件范围宽，几乎和整个 L1 冲突，同一时刻基本只能有一个 L0→L1，RocksDB 用 subcompaction 把这一个任务按 key 范围切成几段并行。换状态时每一层都要按编号增删，不能整层替换。
- 慢的部分（读输入、归并、写文件、同步）本来就不持有任何锁；排队的只是最后换状态的一瞬间。所以并行的障碍不是锁，而是上面这些冲突检查。

## 后台线程

**单独一个线程**，不让 flush 线程顺带做：一次合并可能很慢（L09 的全量合并 0.42 秒），flush 线程忙着合并时，以每秒 360 MB 写入，这段时间会攒下约 38 张 4 MB 的冻结 memtable 在内存里排队。RocksDB 更进一步，flush 与 compaction 分两个线程池，flush 优先。

循环：

```
外层：等信号，最多等 interval
        Wake 或超时 → 往下做
        Stop 或发送端没了 → 结束
      内层：反复 compact_once
        做了一个 → 看一眼有没有 Stop，有就结束；没有就接着做
        没有任务 → 回到外层
        出错 → 打印，回到外层，下一轮重试（线程不结束）
```

**两种等待不能弄反**：外层没事可做，要**等**（`recv_timeout`，相当于 Go 的 `select` 加 `time.After`）；内层两个任务之间只是**看一眼**（`try_recv`，相当于带 `default` 的 `select`）。弄反的后果：空闲时线程不睡，空转占满一个核；一串任务做到一半停下来等。

- **做完一个立刻再问**：一次合并常常让下一对层也需要合并，不必等到下一轮。
- **任务之间看 Stop**：一串任务可能要做很久，关闭时等手上这一个做完就够了。
- **关闭的顺序**：先停 compaction，再停 flush，再冻结并写出内存里的数据。最后这次 flush 产生的 L0 文件不必再为它们开始合并。

**flush 与 compaction 的三处牵连**：

1. 换状态要排队（`state_lock`），只占一瞬间。
2. 争抢磁盘与 CPU：合并写得猛时 flush 变慢。RocksDB 用限速器（rate limiter）限制合并的写入速度。
3. 反压：合并跟不上 flush，L0 越积越多，读越来越慢，只能让写入慢下来，即写入停顿（write stall）。RocksDB 默认 L0 到 20 个文件开始减速、36 个停止写入。本项目还没有。

## 浪费：穿过空层的复制

一个下层是空的时候，上层并进去不去掉任何东西，只是原样重写。简单分层的比例检查对空的下层总是成立（0%），所以新数据要一层层"复制"下去才沉到最底层（见上面模拟的开头）。两种省法：

- **trivial move**：上层的文件与下层**没有任何文件**范围重叠（下层为空时必然如此）时，不重写，只在元数据里改它的层号。顺序写入时这种情况很常见。
- **从下往上建层**：数据库还小时 L0 直接合并进最底层，中间的层先空着；最底层装不下了，才启用上面一层作为新的"基准层"（base level），L0 改为往它合并。RocksDB 的 `level_compaction_dynamic_level_bytes` 就是这样，较新的版本默认开启。这样中间层出现时下面已经有了数据，层间合并从一开始就是真的合并。

本项目在 L10 不做，留到 L12，好让基准先量出这部分浪费有多大。

## 本项目的做法

- **状态**：`LsmStorageState` 的 `l1_sstables` 换成 `levels: Vec<Vec<Arc<SsTable>>>`；选项多了 `max_levels`（至少 1）与 `compaction: Option<SimpleLeveledOptions>`（`None` 表示不自动合并）。`LsmStorageState::level_mut(n)` 取 Ln 的可变引用，把 `- 1` 收在一处。
- **策略**：crate 顶层的 [compaction/](../engine/src/compaction.rs) 模块，每种策略一个子模块，只依赖公开的 `LsmStorageState`；执行留在 `lsm_storage` 的子模块里，因为它要用 `LsmStorage` 的私有字段。依赖只有一个方向：执行依赖策略给出的任务类型。层号用 `usize`，0 是 L0。任务里放 `Arc<SsTable>`（状态里直接放 `Arc`，没有"编号 → 文件"的映射）。策略用自由函数，调用处写成 `simple_leveled::generate_task(&options, &state)`。
- **执行**：`compact_task` 写新文件，`apply_compaction_result` 换状态，`compact_once` 串起"拿 `compaction_lock` → 生成任务 → 写文件、同步目录 → 换状态 → 删旧文件"，返回这次有没有做。任务必须在拿到锁之后生成，才能看到上一次合并换完之后的状态。全量合并改名为 `compact_full`、`apply_full_compaction_result`，结果放进最底层。
- **线程**：`Db` 有两个后台线程，共用信号 `Signal`（`Wake`、`Stop`）与停线程的函数 `stop_thread`；compaction 线程每 50 毫秒醒来一次。`Db::has_pending_compaction` 返回当前形状还有没有待做的任务，基准用它等后台做完。
- **对外**：`level_sstable_counts` 代替 `l1_sstable_count`；命令行程序默认开启自动合并（触发值 2、比例 200%、4 层），`status` 逐层列出；基准驱动新增 `--max-levels`、`--compaction`、`--l0-trigger`、`--size-ratio`，每段负载之后连同后台合并、旧文件的删除一起等完。
- **测试**：策略用文件数直接测，另有 100 次 flush 的模拟；执行与换状态各有测试，其中三个检查断言的失败信息；一个综合测试做 30 轮随机写入与删除，每轮合并到停下，再与 `BTreeMap` 模型比对、核对目录里的文件、确认最底层没有墓碑；线程的测试覆盖"一次叫醒做完全部任务""任务之间看到 Stop 就停""失败后重试"。

## 效果

[2026-10-17 的基准](../bench/results/2026-10-17-simple-leveled-compaction.md)（100 万条，fillrandom 再 overwrite，memtable 4 MB，后台合并做完之后再读）：

- **读回到"全量合并一次"的水平**：与从不合并（55 个 L0 文件）相比，查存在的 key 快 1.7 倍、查不存在的 key 快 8.9 倍、扫描快 13 倍；空间放大从 1.95 自动降到 0.98。
- **代价是写放大**：整次运行约 5.5（不合并时 0.97，L09 只在最后合并一次时 1.46）。粗算，若按同样的频率做全量合并，写放大约 10 且随数据量增长。
- **约 2 是穿过空层的复制**：L0 之下只设 2 层时写放大约 3.9，停下的形状与 4 层完全相同。比例 200% 与 400% 没有区别。
- **合并跟不上写入**：写入每秒约 320 MB，合并每秒约写出 400 MB，而写放大 5 需要 1.6 GB。写入期间 L0 一直积压，合并每次拿到一大批文件，一路并到最底层；停下时全部数据都在最底层，不是稳定写入下的形状。测到的写放大因此偏低，触发值 4 的三次运行分成两种结果。写入期间的读没有测。
- **扫描比 L09 慢了约三成**：读路径推广到多层时，扫描在各层外面多套了一层归并游标，而原来的归并游标每前进一步都复制一次 key、出堆再入堆。改成把当前来源拿在堆外之后（见 [L03](03-iterators-and-merge.md#归并的实现)），全部数据在一层时扫描从每秒 9.3 万次回到 12.1 万次。

## 工业对照

**mini-lsm**（[simple_leveled.rs](../../references/mini-lsm/mini-lsm/src/compact/simple_leveled.rs)、[compact.rs](../../references/mini-lsm/mini-lsm/src/compact.rs)）：规则相同。不同处：上层用 `Option<usize>` 表示（`None` 是 L0）；任务里记文件编号，状态里有一张"编号 → 文件"的表；层数是策略参数；后台每 50 毫秒只做一个任务；L0 的任务把"下层是最底层"固定为 false（只有一层时会多保留墓碑）。

**RocksDB**：

- 给每层算分数：L0 是文件数 ÷ 触发值（默认触发值 4），L1 往下是实际字节数 ÷ 这一层的目标字节数；分数大于等于 1 的层需要合并，先做分数最高的。另有优先项：墓碑特别多的文件、太久没合并的文件（TTL、periodic compaction）。
- 部分合并：每次只挑上层的一个文件与下层与它重叠的文件；不重叠时 trivial move；`level_compaction_dynamic_level_bytes` 从最底层往上建层。
- 后台任务数由 `max_background_jobs` 控制（默认 2），flush 与 compaction 分两个线程池；挑任务时标记 `being_compacted`、检查输出范围与正在进行的合并是否重叠；`max_subcompactions` 把一个大任务按范围切开并行。
- 写入停顿：L0 到 20 个文件减速、36 个停止。

### 导读：mito2 的 picker 与"每个 region 一次一个合并"

mito2 的策略与执行也是分开的。策略是一个 trait（`src/mito2/src/compaction/picker.rs`）：

```rust
pub trait Picker: Debug + Send + Sync + 'static {
    /// Picks input SST files for compaction.
    async fn pick(&self, compaction_region: &CompactionRegion) -> Result<Option<PickerOutput>>;
}
```

`pick` 看一个 region 当前的文件，返回 `None`（没事可做）或者一组要做的合并。每个合并是一个 `CompactionOutput`（`src/mito2/src/compaction.rs`）：

```rust
pub struct CompactionOutput {
    /// Compaction output file level.
    pub output_level: Level,
    /// Compaction input files.
    pub inputs: Vec<FileHandle>,
    /// Whether to remove deletion markers.
    pub filter_deleted: bool,
    // ……（按时间窗口合并时的输出范围，可以先跳过）
}
```

时序策略 TWCS 决定 `filter_deleted` 的那几行（`src/mito2/src/compaction/twcs.rs` 的 `find_inputs`）：

```rust
// We only remove deletion markers if we found less than 2 runs and not in append mode.
// because after compaction there will be no overlapping files.
let mut filter_deleted =
    found_runs <= 2 && !self.append_mode && !window_has_overlap(files, windows);
// ……
if filter_deleted && overlaps_files_left_behind(&inputs, &files_to_merge) {
    filter_deleted = false;
}
```

调度器保证同一个 region 同一时刻只有一个合并（`src/mito2/src/compaction/scheduler.rs` 的 `schedule_compaction`，删去了与 DDL 有关的分支）：

```rust
if let Some(status) = self.region_status.get_mut(&region_id) {
    // 这个 region 正在合并
    if requires_pending_compaction_slot(&compact_options, time_range) {
        status.set_pending_request(PendingCompaction { /* …… */ });  // 手动的：排在这次之后
    } else {
        status.merge_regular_trigger(waiter);                        // 自动的：并进这一次
    }
    return Ok(false);
}
// 没有正在进行的合并：登记，再开始挑文件
let mut status = CompactionStatus::new(region_id, version_control.clone(), access_layer.clone());
// ……
self.region_status.insert(region_id, status);
self.dispatch_compaction_planning(plan_id, request, compact_options, time_range);
```

与本项目逐项对上：

| mito2 | 本项目 |
| --- | --- |
| `Picker::pick` 返回 `Option<PickerOutput>` | `generate_task` 返回 `Option<SimpleLeveledTask>` |
| `CompactionOutput` 的 `inputs`（`FileHandle`，引用计数的文件） | 任务里的 `upper_ssts`、`lower_ssts`（`Arc<SsTable>`） |
| `output_level` | `lower_level` |
| `filter_deleted`：合并之后这个时间窗口里不再有重叠的文件、也没有被留下的重叠文件，才去掉删除标记 | `is_lower_level_bottom`：下面没有更旧的层，才丢墓碑 |
| `region_status` 里有这个 region 就不再开新的合并，自动触发并进当前这次 | `compaction_lock` 加单个线程：同一时刻只有一个合并 |

差异来自数据模型：mito2 按数据的时间戳把文件分进时间窗口，每个窗口单独找 sorted run 并合并，所以 `pick` 一次可能给出几个窗口各自的合并；"能不能去掉删除标记"看的是同一时间窗口里还有没有重叠的文件，而不是"是不是最底层"。它的 `output_level` 总是 L1（mito2 只有两层）。一个节点上有成百上千个 region，并行来自很多 region 同时合并，每个 region 内部保持串行。

可以先跳过的部分：`PickerOutput` 的 `expired_ssts`（按 TTL 直接删掉过期的文件，不用合并）、`max_parallelism`、调度器里与 DDL 有关的等待与取消、`pending_request` 的具体排队方式。

## 常见错误

- **比例的两边写反**（上层 ÷ 下层）。会选错层；测试用"恰好等于比例""空的下层""空的上层"几种形状就能区分。
- **上层是空的也选中。** 一直生成空任务，停不下来。
- **循环包含了最底层。** 访问不存在的下一层，越界。
- **一次返回好几个任务，依次执行。** 第一个做完后其余的文件列表已经过期，换状态时断言失败或者漏掉新文件。
- **不在最底层也丢墓碑。** 被删除的值从更下面的层复活。
- **全量合并的结果放进 L1。** 随后的自动合并把整个数据库一层层原样复制下去。
- **换状态时清空 L0，或者用开始时的快照覆盖当前状态。** 合并期间 flush 出的文件丢失。
- **上层是 L0 时也去算 `levels[upper_level - 1]`。** 0 减 1，panic。
- **合并时上层与下层的先后放反。** 同一个 key 留下了旧版本。
- **断言的失败信息不带层号，或者在它前面又加了一条比较长度的断言。** 出问题时看不出哪一层，或者先触发的是那条没有信息的断言。
- **后台循环里两种等待弄反。** 空闲时空转，或者一串任务做到一半停下。
- **合并出错就让线程结束。** 之后再也不合并，L0 无限增长。
- **做完一个任务就回去等下一轮。** 一串级联的任务要等好几轮才做完。

## 资料

- mini-lsm 教程第二周第 2 章：[week2-02-simple.md](../../references/mini-lsm/mini-lsm-book/src/week2-02-simple.md)；三种放大见 [week2-overview.md](../../references/mini-lsm/mini-lsm-book/src/week2-overview.md)。
- 上一篇：[compaction 的执行](09-compaction-execution.md)；归并游标的改动：[迭代器接口与多路归并](03-iterators-and-merge.md#归并的实现)。
- 基准记录：[简单分层 compaction：自动合并下的三种放大](../bench/results/2026-10-17-simple-leveled-compaction.md)。
- Rust：[[rust-learning/notes/13-engineering/02-模块、可见性与重导出|模块、可见性与重导出]]。

返回 [本组目录](README.md)。
