# tiered compaction

状态：已有正文 · 代码：[compaction/tiered.rs](../engine/src/compaction/tiered.rs) 的 `generate_task`、`pick_tiers`；[compaction.rs](../engine/src/compaction.rs) 的 `CompactionOptions`；[lsm_storage/compact.rs](../engine/src/lsm_storage/compact.rs) 的 `compact_once`、`run_tiered_task`、`compact_tiered_task`、`apply_tiered_result`；[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 flush（`force_flush_next_imm_memtable`）

## 位置

仍是[全景](README.md#全景)的第 ⑤ 步。[L09](09-compaction-execution.md) 写了合并的执行，[L10](10-simple-leveled-compaction.md) 写了第一种决策（简单分层）；这一篇换一种决策：tiered（RocksDB 叫 universal）。执行的骨架（在锁外合并、写新文件 → 同步目录 → 换状态 → 删旧文件）照用，变的是状态的形状与挑选的规则。下一篇 [L12](12-leveled-compaction.md) 是真正的 leveled：部分合并、按字节比较、基准层与 trivial move。

## 与 B+Tree 阶段的对应

| B+Tree | LSM（tiered） |
| --- | --- |
| 树只有一种形状，每个 key 只有一个位置 | 同一个 key 可以同时出现在好几个 tier 里，新旧靠 tier 的先后决定 |
| 修改原地进行（CoW 时复制一条路径），旧版本立刻成为垃圾 | 旧版本留在较旧的 tier 里，要等与最底 tier 合并才被丢掉：空间放大由策略决定 |
| 读一次最多走树高那么多页 | 读要看每个 tier（点查可用 Bloom filter 排除，扫描不能），tier 数就是读放大 |

## 问题：leveled 写得太多

leveled 的不变量是"每层只有一个 sorted run"。上层往下并时，下层必须整层（或与之重叠的部分）重写，才能保持一层一个有序序列。层间比例为 T 时，一条数据每下沉一层大约陪着重写 T/2～T 次：

- 写放大 ≈ 层数 × T/2
- 层数 ≈ log_T(总数据 ÷ 一次 flush 的大小)

调小 T（多分几层、每层小一点）并不能把写放大降下去：层数会跟着变多，乘积降不下去。总数据是 1024 次 flush 的量时粗算：

| T | leveled 层数 | leveled 写放大 ≈ 层数 × (T+1)/2 | tiered 写放大 ≈ 层数 | tiered 最多要看的 run ≈ 层数 × T |
| ---: | ---: | ---: | ---: | ---: |
| 2 | 10 | 15 | 10 | 20 |
| 4 | 5 | 12.5 | 5 | 20 |
| 10 | 3 | 16.5 | 3 | 30 |
| 32 | 2 | 33 | 2 | 64 |

leveled 怎么调都在 12 以上，最好的点在 T=3～4 附近。写放大来自"一层只许一个 run"这条规矩本身；要再降，只能放松它。

## tiered：只合并大小相近的 run

把规矩放松为：**同时可以有好几个 sorted run，只把大小相近的合在一起**。

```
最新 → [t9: 1 个文件] [t8: 1] [t5: 3] [t1: 20] ← 最旧
         ↑ flush 出的文件自己成为一个新的 tier，放在最前面
```

- 每个 tier 是一个 sorted run：内部按 key 排好、文件之间范围互不重叠。tier 之间可以重叠，新旧由位置决定。
- tier 的个数随时变化：flush 让它加一，合并让它减少。
- 合并像二进制计数器的进位：两个 1 合成 2，两个 2 合成 4……一条数据每升一个"大小档"才被重写一次，写放大约等于档数。没有"小的并进大的、大的陪着重写"。

文献里另一种说法是"每层最多放 T 个 run，放满了把这 T 个合成一个 run 送进下一层"，与"一列 tier、大小相近的合并"是同一件事，只是把大小档显式叫作层。

代价：

- **读放大**：点查与扫描要看每一个 tier。点查有 Bloom filter，多出来的 tier 大多在问过 filter 之后就被排除；扫描没有 filter，每个与范围相交的 tier 都要建游标、读块、参与归并。
- **空间放大**：同一个 key 的旧版本散在好几个 tier 里，墓碑也一样，只有与最底 tier 合并时才能丢。

一句话：leveled 用写换读和空间，tiered 反过来。

## 三种触发

这里用 mini-lsm 的简化版（RocksDB universal 的三条规则），tier 的大小用文件数。tier 数达到 `num_tiers` 才开始检查，按顺序看三种触发，第一个成立的给出任务：

| 顺序 | 触发 | 条件 | 合并哪些 | 能丢墓碑吗 |
| --- | --- | --- | --- | --- |
| 1 | 空间放大 | 除最底以外所有 tier 的大小之和 ÷ 最底 tier ≥ `max_size_amplification_percent`% | 全部 | 能 |
| 2 | 大小比例 | 从最新往旧累加，下一个 tier ÷ 前面之和 > (100 + `size_ratio`)%，且前面至少 `min_merge_width` 个 | 前面那几个，不含那个大的 | 不能 |
| 3 | 减少段数 | 前两种都不成立 | 最新的 `max_merge_width` 个（不设就全部） | 恰好取到全部时能 |

例子（`size_ratio` 1，`min_merge_width` 2，大小从新到旧）：

- `[1, 1, 1]`：上面 2 ÷ 最底 1 = 200%，阈值 200 时触发 1，全部合并。
- `[1, 1, 3]`：空间 2 ÷ 3 不够；比例 1 ÷ 1 不够，3 ÷ 2 = 150% 超过 101%，前缀有 2 个，合并前两个，得到 `[2, 3]`。
- `[1, 2, 4]`：2 ÷ 1 超过了，但前缀只有 1 个，不算；4 ÷ 3 超过，合并前两个。

### 为什么要三种

tiered 天生省写，要防的是另外两种放大。每种触发守一处，少一种就会在某种形状下失控：

- **触发 2 是日常的合并，负责形状。** 前面几个加起来比下一个小得多，就先把它们合成一个：合出来的仍比下一个小，形状保持由小到大，大的那个也不用陪着重写。大部分合并由它发起。
- **触发 1 防空间放大。** 触发 2 只合并上面的小 tier，永远碰不到最底，过期的版本与墓碑会越积越多。它把最底 tier 近似看作"整个数据集"，上面积的超过一定比例就全部合并一次。排在第一，因为磁盘满了是致命的，读慢一点不是。
- **触发 3 给读放大兜底。** 触发 2 有盲区：每个 tier 都正好等于前面之和（`[1, 1, 2, 4, 8]`，每一步正好 100%），它认为形状合格；最底又够大，触发 1 也不成立；tier 却已经太多。这时只剩触发 3 能动手。

注意一串一样大的 tier 并不落到触发 3：`[1, 1, 1, …]` 里最底只有 1，空间放大先超限，触发 1 先成立。触发 3 要求"最底够大"与"上面形状合格"同时成立，所以出场不多。

mini-lsm 的模拟器（flush 200 次，`num_tiers` 8）里，只实现触发 1 时 50 次 flush 后就有 27 个 tier；实现 1 和 2 时 38 个；三种都有时 7 个，写放大 3.7。

### 参数

| 参数 | 管什么 | RocksDB 默认 | 调大的效果 |
| --- | --- | --- | --- |
| `num_tiers` | tier 数到这么多才检查（RocksDB 里是 `level0_file_num_compaction_trigger`） | 4 | 合并来得晚：写更省，读更慢 |
| `max_size_amplification_percent` | 触发 1 的阈值 | 200 | 允许积更多过期数据 |
| `size_ratio` | 触发 2 判断"大得多"的宽容度 | 1 | 要差得更多才算大小不同，触发 2 变少 |
| `min_merge_width` | 触发 2 至少合并几个 | 2 | 每次合并更多个 run，次数更少，平时 run 更多 |
| `max_merge_width` | 一次最多合并几个 | 不限 | 单次任务更大、更久、临时占的空间更多 |

`num_tiers` 是门槛，不是上限：三种触发都不成立时 tier 数可以超过它。这几个参数都在同一条轴上调：合并来得勤，读与空间好、写放大高；来得懒，反过来。

### 一定会停下

每个任务至少把 2 个 tier 合成 1 个，tier 数严格减少，而不到 `num_tiers` 时不再给任务。这个论证要求"至少 2 个"：`num_tiers`、`min_merge_width`、`max_merge_width` 有一个是 1，就可能反复"合并"同一个 tier（原样重写它），后台线程永远停不下来。所以三者都要检查至少为 2。

## 正确性

- **只合并连续的一段**，结果放回原位，列表的顺序才始终等于数据的新旧（[L09](09-compaction-execution.md#另一种组织方式tiered)）。跳着合并（合并第 1 个与第 3 个、留下第 2 个），结果放在哪里都会颠倒新旧。
- **从最新一端开始取前缀**是 mini-lsm 的进一步简化：中间连续的一段也是正确的（RocksDB 允许）。取前缀时，合并结果一定接在"合并期间新 flush 的 tier"之后，换状态最简单。
- **只有包含最底 tier 时才能丢墓碑。** 前缀被 `max_merge_width` 截短、没有取到最底时，不算包含最底：更旧的 tier 里可能还有被墓碑盖住的值。
- **合并期间 flush 出的 tier 比合并结果新**，必须排在它前面。换状态时不能假定输入还在列表的最前面，要先找到第一个输入 tier 现在的位置。
- **合并结果可以是空的**：包含最底、而存活的全是墓碑时，一个文件都不写。这时那几个 tier 直接拿掉，不留下空的 tier。

## 按文件数模拟与真实引擎的差距

只看各 tier 的文件数，就能模拟策略：每次 flush 在最前面加一个大小为 1 的 tier，反复要任务、把选中的几个换成它们之和，直到没有任务。flush 200 次的结果：

| `num_tiers` | 触发 1 / 2 / 3 的次数 | 写放大 | 最后的形状 |
| ---: | --- | ---: | --- |
| 4 | 1 / 162 / 5 | 13.5 | `[4, 6, 190]` |
| 8 | 2 / 52 / 2 | 3.7 | `[1, 1, 4, 5, 21, 28, 140]` |
| 16 | 3 / 15 / 0 | 3.0 | 十个 1，然后 `15, 175` |

`num_tiers` 4 的写放大比 leveled 还高。过程是这样的：

```
[1, 1, 6, 10] → 合并前 2 个 → [2, 6, 10]
[1, 2, 6, 10] → 合并前 2 个 → [3, 6, 10]
[1, 3, 6, 10] → 合并前 2 个 → [4, 6, 10]
```

tier 太少时每来一个文件就满了，第二个 tier 每次收下 1 个文件、自己整个重写一遍，回到了"小的并进大的"。

但真实引擎里没有出现这个现象（见"效果"）。模拟假定每次 flush 之后合并立刻做完；实际写入比合并快，一次合并之间已经 flush 出好几个 tier，合并每次拿到的是一批。模拟适合检查规则与停机，不能直接预测真实引擎的写放大。

## 状态的表示：不用 L0

**tiered 下 `levels` 就是全部的 tier**，`levels[0]` 最新，L0 一直是空的；flush 出的文件作为一个只含 1 个文件的 tier 插到 `levels` 的最前面。

为什么不保留 L0、把每个 L0 文件当作一个单文件的 tier：设 L0 是 `[3, 2, 1]`（3 最新），任务只取最新的 2 个，输出是 7、8 两个文件组成的一个 sorted run，它比文件 1 新：

- **放进 `levels` 的最前面**：读路径先查完 L0 才查 `levels`，比 7、8 旧的文件 1 会先被查到，读到旧值。这是正确性错误。
- **放回 L0 的前面**（`[7, 8, 1]`）：读是对的（7、8 都比 1 新，彼此又不重叠），但 L0 记不住"7、8 是同一个 tier"。策略会把它们当成两个大小为 1 的 tier，三种触发都按错的大小判断；L0 的文件又要逐个检查，大 tier 放在 L0 里会让读变慢。

让 L0 记住分组，它就成了 `levels`。所以干脆不用 L0。

分层策略可以有 L0，是因为它每次都把 L0 **整个**并下去，留在 L0 的只可能是合并期间新 flush 的文件，本来就比合并结果新。

**读路径不用改**：点查从 `levels` 的前面往后查，每个 tier 内按 key 找唯一的候选文件；扫描每个 tier 一个串接游标，再归并，前面的胜出。"`levels` 从前往后越来越旧、每项是一个 sorted run"对两种策略都成立。

### tier 与 SST

tier 是逻辑上的分组，只记在状态里；SST 是磁盘上的文件，文件本身不知道自己属于哪个 tier。类型就是 `Vec<Vec<Arc<SsTable>>>`：外层是全部 tier，内层是一个 tier 的几个 SST。

| tier 怎么来的 | 含几个 SST |
| --- | --- |
| flush | 1 个：一张 memtable 一个文件 |
| 合并的输出 | 多个：按 `target_sst_size` 切开，按 key 排好、互不重叠 |

读放大由 tier 的**个数**决定，与每个 tier 里有几个 SST 无关：tier 内最多只看一个候选文件。策略里的"tier 大小"是文件数，文件大小相近时约等于字节数。tiered 总是整 tier 合并，切分对它不是必需的；保留切分是为了与分层共用格式和读路径、扫描时能按范围跳过文件、单个文件不至于无限变大。

## 两种策略在同一个引擎里

**一个引擎只能用一种**：两种策略对状态的理解不同（固定的层、数据往下沉 vs. 一列个数可变的 tier）。代码里两种都有，打开时用选项选定。以后有了 manifest，用另一种策略打开旧数据库，要么拒绝，要么先全量合并成一个 run（两种策略都认得的形状）再切换。

**参数只放在引擎里一份**：合并的入口不接受策略参数，而是读引擎自己的选项。策略决定状态的形状，如果入口能接受另一份参数，就可能对着 tier 列表执行"把 L1 并进 L2"，打破不变量。

**分支放在哪里**：两种策略的差别只有一两行的地方（打开时有没有空层、flush 把文件放在哪里），就地判断，不复制整段流程；复制的话，锁的顺序、断言、同步目录都有两份，以后改一处漏一处。整个流程都不同的地方（生成任务、合并、换状态），每种策略一套函数，入口只负责分派。读路径没有分支。

另一种做法是 trait（相当于 Go 的 interface）：每种策略实现"挑任务""做任务"等方法，引擎只存一个 `Box<dyn …>`。RocksDB 挑文件的部分是这样（`CompactionPicker` 的子类 `LevelCompactionPicker`、`UniversalCompactionPicker`、`FIFOCompactionPicker`），flush 与写版本的公共部分只有一份。两种策略的任务类型不同时，trait 要加关联类型或再包一层任务枚举；策略只有两三种时，枚举加 `match` 更直接（mini-lsm 也是枚举）。

## 本项目的做法

- **选项**：`LsmStorageOptions::compaction` 从 `Option<SimpleLeveledOptions>` 改成枚举 `CompactionOptions { NoCompaction, SimpleLeveled(..), Tiered(..) }`；`flush_to_l0()` 回答"flush 的文件放进 L0 还是成为新 tier"。`NoCompaction` 的形状与简单分层相同。
- **策略**：[compaction/tiered.rs](../engine/src/compaction/tiered.rs)。`TieredOptions` 五个参数；`pick_tiers` 只看各 tier 的文件数，返回触发的种类与合并个数，比较都写成整数乘法（`next × 100 > (100 + size_ratio) × 前面之和`），避免浮点数在阈值上的舍入；`generate_task` 抄出前几个 tier 的文件列表，`bottom_tier_included` 等于"取到了全部"。三个参数在 `check_options` 里断言至少为 2。
- **任务**：`TieredTask { tiers: Vec<Vec<Arc<SsTable>>>, bottom_tier_included }`。不给 tier 编号（mini-lsm 用第一个 SST 的编号）：`Arc` 的指针比较（`same_ssts`）就能认出一个 tier。
- **执行**：`compact_tiered_task` 每个 tier 一个串接游标，放进归并游标，前面的胜出；包含最底才丢墓碑。`apply_tiered_result` 先找到第一个输入 tier 现在的位置（前面是合并期间新 flush 的），断言后面紧跟着的正是任务里的那几个、断言"是否包含最底"与现在的状态一致，再用 `splice` 把它们换成一个新 tier（输出为空时只拿掉）。
- **入口**：`compact_once()` 不再接受参数，按引擎的选项分派到 `run_simple_leveled_task` 或 `run_tiered_task`；`has_pending_compaction` 移到引擎上，`Db` 不再另存一份参数。全量合并在 tiered 下就是"带上全部 tier 的任务"，不到 2 个 tier 时不做。
- **flush 与打开**：tiered 打开时没有空层；flush 换状态时 `levels.insert(0, vec![sst])`。
- **对外**：`uses_tiers()`；命令行程序第三个参数选 `simple` 或 `tiered`，`status` 在 tiered 下逐个列出 tier；基准驱动 `--compaction tiered` 与 `--num-tiers`、`--max-size-amp`、`--tier-size-ratio`、`--min-merge-width`、`--max-merge-width`（0 为不限）。
- **测试**：策略按例子测每种触发、阈值正好相等、参数断言，并与 mini-lsm 参考模拟器的结果逐项对照（`num_tiers` 8 时形状 `[1, 1, 4, 5, 21, 28, 140]`、写出 742 个文件）；执行与换状态测新版本胜出、墓碑的去留、合并期间 flush 的 tier 留在前面、空输出、断言的失败信息；60 轮随机写入与删除对照 `BTreeMap` 模型；`Db` 层测后台合并能停下、多线程写入与合并同时进行时数据不丢。模拟测试也设了任务数上限，策略写错时测试失败而不是卡住。

## 效果

[2026-10-18 的基准](../bench/results/2026-10-18-tiered-compaction.md)（与 L10 同一组负载，同一天重跑简单分层作对照）：

| | 整次运行的写放大 | 写完后等后台 | 停下时空间放大 | 扫描（万次/秒） |
| --- | ---: | ---: | ---: | ---: |
| 简单分层 | 5.36 | 1.06 s | 0.98 | 13.3 |
| tiered，`num_tiers` 4 | 2.55 | 0.35 s | 1.05～1.67 | 5.3～9.6 |
| tiered，`num_tiers` 8 | 2.42 | 0.44 s | 0.98～1.66 | 4.1～13.3 |
| tiered，`num_tiers` 16 | 1.99 | 0.21 s | 1.15～1.60 | 4.9～7.4 |

- **写放大约减半，合并跟得上写入**：L10 里"合并跟不上"的问题明显缓解。
- **读取决于停下时剩几个 tier**，每次运行都不一样：只剩一个 tier 时与简单分层完全相同；7 个 tier 时查不存在的 key 慢一半（每个 tier 问一次 filter），扫描慢到三分之一（没有 filter 可用）。点查存在的 key 每次仍只进入约 1 个 SST。
- **`num_tiers` 4 的写放大没有到模拟的 13.5**：写入快于合并，合并每次拿到一批 tier。

## 工业对照

**mini-lsm**（[tiered.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/compact/tiered.rs)）：规则相同。不同处：tier 是 `(tier 编号, SST 编号列表)`，编号取输出的第一个 SST；比较用浮点数；换状态时用一张"编号 → 文件列表"的表找到要拿掉的 tier，在遍历中遇到最后一个输入时插入输出；`max_merge_width` 只用于触发 3。

**RocksDB universal**：

- 保留 L0：每个 L0 文件算一个 sorted run，L1 往下每个非空的层算一个，层号越大越旧；合并结果放进某一层，用层号维持新旧顺序。
- 比较的是字节数；`size_ratio` 默认 1、`min_merge_width` 2、`max_merge_width` 不限、`max_size_amplification_percent` 200，sorted run 数到 `level0_file_num_compaction_trigger` 才开始检查。
- 挑选不限于前缀：从最新的 run 开始试，凑不够就换下一个 run 作为起点，正在合并的 run 跳过。
- 空间放大的代价：全量合并时输入与输出同时在盘上，最坏要留出与数据库一样大的空闲空间。

### 导读：mito2 的 TWCS 怎样找 sorted run

GreptimeDB 的 mito2 用 TWCS（time window compaction strategy）：先按数据的时间戳把文件分进时间窗口，每个窗口里再做一件和 tiered 很像的事——数一数有几个 sorted run，太多就合并一部分。与本项目不同的是，**它不在状态里记录 run，而是每次从文件的范围现场算出来**。

`src/mito2/src/compaction/run.rs` 的 `find_sorted_runs`（删去了剪枝用的下标列表，重叠检查改写成等价的一行）：

```rust
pub fn find_sorted_runs<T: Item>(items: &mut [T]) -> Vec<SortedRun<T>> {
    sort_ranged_items(items);                       // 按范围的起点排序
    let mut current_run = SortedRun::default();
    let mut runs = vec![];
    let mut selection = BitVec::repeat(false, items.len());
    while !selection.all() {                        // 还有文件没分进任何 run
        for (item, mut selected) in items.iter().zip(selection.iter_mut()) {
            if *selected { continue; }
            // 与当前 run 里的文件都不重叠，就放进当前 run
            if !current_run.items.iter().any(|run_item| run_item.overlap(item)) {
                selected.set(true);
                current_run.push_item(item.clone());
            }
        }
        runs.push(std::mem::take(&mut current_run)); // 扫完一遍得到一个 run
    }
    runs
}
```

每扫一遍，贪心地挑出一组互不重叠的文件作为一个 run，直到所有文件都有归属。

`src/mito2/src/compaction/twcs.rs` 的 `find_inputs` 决定合并什么：

```rust
let sorted_runs = find_sorted_runs(&mut files_to_merge);
let found_runs = sorted_runs.len();
// 合并后不再有重叠的文件，才去掉删除标记
let mut filter_deleted =
    found_runs <= 2 && !self.append_mode && !window_has_overlap(files, windows);
let mut inputs = if found_runs > 1 {
    reduce_runs(sorted_runs)                       // 有重叠：挑两个 run 里互相重叠的文件
} else {
    let run = sorted_runs.last().unwrap();
    if run.items().len() < self.trigger_file_num {
        return (vec![], filter_deleted);           // 只有一个 run、文件不多：不合并
    }
    merge_seq_files(run.items(), self.max_output_file_size) // 只把相邻的小文件并大
};
```

`reduce_runs` 按大小排序后，在最小的 100 个 run 里两两试，代价是"两个 run 之间互相重叠的文件的总大小"，取代价最小的那一对，只合并其中重叠的文件。

与本项目对上：

| mito2 | 本项目 |
| --- | --- |
| `find_sorted_runs`：每次从文件范围算出 run | 状态里的 `levels`：run 是记录下来的 |
| `found_runs`：run 的个数 | tier 的个数 |
| `reduce_runs`：挑重叠代价最小的两个 run，只合并重叠的文件 | 三种触发：按大小挑最新的几个 tier，整 tier 合并 |
| `filter_deleted`：合并后窗口里不再有重叠的文件 | `bottom_tier_included`：取到了最底的 tier |

差异来自两处：

- **数据模型**：时序数据的写入几乎都落在最近的时间窗口，旧窗口写完就不再变，所以按窗口分开合并，旧窗口的数据不会被反复重写。
- **谁新谁旧怎么判断**：mito2 的每一行都带着序号（`__sequence` 列），合并时同一个 key 按序号取最新的，不依赖文件的位置。所以它可以挑任意两个 run 里重叠的那部分文件来合并，不必像本项目那样只合并相邻的 tier。本项目的记录里没有序号，新旧只能由 tier 的先后决定，这正是"只能合并连续的一段"的来源。到 MVCC（L16 往后）给 key 加上时间戳之后，这个限制可以放宽。

可以先跳过的部分：`find_sorted_runs` 里 `active_run_item_indices` 的剪枝、`merge_seq_files` 的打分、`DEFAULT_MAX_INPUT_FILE_NUM` 对输入个数的限制、append 模式。

## 常见错误

- **跳着合并不相邻的 tier。** 结果放在哪里都会颠倒新旧，读到旧值。
- **前缀被 `max_merge_width` 截短时仍丢墓碑。** 没有取到最底，被删除的值从更旧的 tier 里复活。
- **换状态时假定输入还在最前面。** 合并期间 flush 出的 tier 被覆盖或排到了合并结果后面。
- **合并结果为空时留下一个空的 tier。** 之后的大小计算与"最底 tier"的判断都会出错（最底的大小为 0）。
- **归并时旧的 tier 排在前面。** 同一个 key 留下了旧版本。
- **`min_merge_width` 或 `max_merge_width` 允许为 1。** 反复原样重写同一个 tier，后台线程停不下来。
- **比例的不等式用了 `>=`。** 大小相等的 tier 也被当成"大得多"，合并比预期频繁。
- **保留 L0，把合并结果放进 `levels`。** L0 里剩下的旧文件比合并结果先被读到。
- **合并入口另外接受一份策略参数。** 参数与引擎打开时的形状不一致时，按错误的形状生成任务。
- **只看模拟就下结论。** 模拟假定合并立刻做完；写入快于合并时，真实的写放大与形状都不一样。

## 资料

- mini-lsm 教程第二周第 3 章：[week2-03-tiered.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week2-03-tiered.md)；三种放大见 [week2-overview.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week2-overview.md)。
- RocksDB Wiki：Universal Compaction。
- 上一篇：[简单分层 compaction](10-simple-leveled-compaction.md)；tiered 与 leveled 的最初对比：[compaction 的执行](09-compaction-execution.md#另一种组织方式tiered)。
- 基准记录：[tiered compaction：写放大减半，读取看停下时剩几个 tier](../bench/results/2026-10-18-tiered-compaction.md)。

返回 [本组目录](README.md)。
