//! leveled 策略（RocksDB 的默认）：每层一个 sorted run，按字节给每层定目标大小，
//! 每次只把上层的一个文件并进下层。
//!
//! 与简单分层的三处不同：
//!
//! - **目标大小从最底层往上算**：最底层的目标取"实际大小"与 `base_level_size_bytes`
//!   中较大的；往上每层是下一层目标的 1/`level_size_multiplier`，直到下一层的目标不超过
//!   `base_level_size_bytes`，再往上的层目标都是 0。第一个目标大于 0 的层是**基准层**，
//!   L0 直接并进它；中间层随数据变大从下往上一层层启用，不会往空层原样复制。
//!   这就是 RocksDB 的 `level_compaction_dynamic_level_bytes`。
//! - **按字节比较**：L0 的文件数达到 `level0_file_num_compaction_trigger` 先做 L0；
//!   否则 L1 往下每层的分数是"实际大小 ÷ 目标大小"，大于 1 的层里取最大的往下并。
//! - **部分合并**：上层只挑一个文件，下层只拉进与它的 key 范围重叠的文件（L0 例外：
//!   拿全部 L0 文件）。
//!
//! 层号的写法与简单分层相同：0 是 L0，n 是 Ln（在 `state.levels[n - 1]`）。

use crate::compaction::{is_sorted_run, sorted_by_first_key};
use crate::lsm_storage::LsmStorageState;
use crate::table::SsTable;
use std::sync::Arc;

/// leveled 策略的参数。可以参照 RocksDB：触发值 4、比例 10、基准大小取
/// "memtable 上限 × 触发值"（RocksDB 的 256 MB 就是 64 MB × 4）。
#[derive(Debug, Clone)]
pub struct LeveledOptions {
    /// L0 的文件数达到这么多，就把 L0 全部并进基准层。至少为 1。
    pub level0_file_num_compaction_trigger: usize,
    /// 相邻两层的目标大小之比：上层的目标是下层的 1/这么多。至少为 2。
    pub level_size_multiplier: u64,
    /// 基准层大约多大，字节：下一层的目标超过它时，才给上面一层定一个大于 0 的目标。
    /// 所以基准层的目标不超过它（只有一层时例外，见 `target_sizes`）。至少为 1。
    pub base_level_size_bytes: u64,
}

/// 检查参数，不合法时 panic。
fn check_options(options: &LeveledOptions) {
    assert!(
        options.level0_file_num_compaction_trigger >= 1,
        "L0 的触发值至少为 1，否则 L0 是空的也会不停地生成任务"
    );
    assert!(
        options.level_size_multiplier >= 2,
        "level_size_multiplier 至少为 2，否则上层的目标不比下层小"
    );
    assert!(
        options.base_level_size_bytes >= 1,
        "base_level_size_bytes 至少为 1，否则空的树没有一层的目标大于 0，找不到基准层"
    );
}

/// 一次合并的任务：上层挑出的文件与下层和它们重叠的文件合并，结果放进下层；
/// 或者（`is_trivial_move`）把上层的文件原样挪进下层。
///
/// 文件列表是生成任务那一刻的状态里的：执行时只读这些文件，换状态时只拿掉这些文件。
/// 这期间 flush 可能往 L0 加了新文件，它们不在任务里。
pub struct LeveledTask {
    /// 上层的层号：0 是 L0，n 是 Ln。
    pub upper_level: usize,
    /// 上层参加合并的文件。L0 是当时的全部 L0 文件（新的在前，顺序与状态里相同）；
    /// 其他层是挑出的一个文件。
    pub upper_ssts: Vec<Arc<SsTable>>,
    /// 下层的层号：L0 的任务是基准层，其他是 `upper_level + 1`。
    pub lower_level: usize,
    /// 下层与 `upper_ssts` 的整体 key 范围重叠的文件，按 key 排好（顺序与状态里相同）。
    /// 可以是空的。
    pub lower_ssts: Vec<Arc<SsTable>>,
    /// 下层是不是最底层。是的话下面没有更旧的版本，合并时可以丢掉墓碑。
    pub is_lower_level_bottom: bool,
    /// 只挪动、不重写：上层的文件按首 key 排好之后彼此不重叠（只有一个时自然成立），
    /// 下层没有与它们重叠的文件。执行时不读写文件，只把这些文件从上层拿掉、按 key 排好
    /// 放进下层的正确位置，也不删除它们。顺序写入时 L0 常积着好几个这样的文件。
    /// 挪进最底层时其中的墓碑会留下（不影响读的结果，只是暂时多占空间）。
    pub is_trivial_move: bool,
}

/// L1 往下各层的目标大小，字节。
///
/// `level_sizes` 是 L1 往下各层的实际大小（第 0 项是 L1，最后一项是最底层），
/// 返回的目标与它一一对应、长度相同。至少要有一层。
///
/// 最底层的目标 = max(最底层的实际大小, `base_level_size_bytes`)；从倒数第二层往上，
/// 下一层的目标超过 `base_level_size_bytes` 时，这一层的目标 = 下一层的目标 ÷
/// `level_size_multiplier`（整数除法），否则是 0。一旦出现 0，再往上都是 0。
/// 目标只取决于最底层的实际大小，上面各层有多少数据不影响目标。
///
/// 例如基准 16、比例 10、4 层：最底层是 8 时目标是 `[0, 0, 0, 16]`，
/// 116 时是 `[0, 0, 11, 116]`，1600 时是 `[0, 16, 160, 1600]`。
fn target_sizes(options: &LeveledOptions, level_sizes: &[u64]) -> Vec<u64> {
    assert!(!level_sizes.is_empty(), "L0 之下至少要有 1 层");
    let level_count = level_sizes.len();
    let bottom = level_count - 1;
    let mut targets: Vec<u64> = vec![0; level_count];
    let bottom_size = level_sizes[bottom];
    let bottom_target = bottom_size.max(options.base_level_size_bytes);
    targets[bottom] = bottom_target;
    for i in (0..bottom).rev() {
        if targets[i + 1] <= options.base_level_size_bytes {
            break;
        }
        targets[i] = targets[i + 1] / options.level_size_multiplier;
    }
    targets
}

/// 基准层的层号（从 1 开始）：第一个目标大于 0 的层。L0 往下并时并进这一层。
///
/// `targets` 是 `target_sizes` 的结果，第 0 项是 L1。最底层的目标至少是
/// `base_level_size_bytes`（大于 0），所以一定找得到。
fn base_level(targets: &[u64]) -> usize {
    let index = targets
        .iter()
        .position(|&t| t > 0)
        .expect("最底层的目标至少是 base_level_size_bytes，一定大于 0");
    index + 1
}

/// 只看 L0 的文件数与各层的字节数，决定把哪一层并进哪一层，返回 `(上层, 下层)` 的层号；
/// 不需要合并时返回 `None`。
///
/// `level_sizes` 与 `target_sizes` 的参数相同：L1 往下各层的实际大小，最后一项是最底层。
///
/// 1. L0 的文件数达到触发值：返回 `(0, n)`，n 是基准层与第一个有数据的层中更靠上的那个。
///    基准层上面有数据时（数据变少、基准层下移之后留下的，或从简单分层换过来的）
///    不能跳过它：L0 的新值落到它下面，点查会先查到它里面的旧值。
/// 2. 否则看 L1 到倒数第二层（最底层没有下一层，不参加），分数 = 实际 ÷ 目标，
///    只有大于 1 的层可以选；取分数最大的那层 n，返回 `(n, n + 1)`。分数相同时取上面的层。
///    目标是 0 却有数据的层（数据变少、基准层下移之后留下的）分数算作无穷大；
///    目标是 0、也没有数据的层不选。
/// 3. 都不满足：`None`。
fn pick_levels(
    options: &LeveledOptions,
    l0_file_count: usize,
    level_sizes: &[u64],
) -> Option<(usize, usize)> {
    check_options(options);
    let targets = target_sizes(options, level_sizes);
    let base_level = base_level(&targets);
    if l0_file_count >= options.level0_file_num_compaction_trigger {
        // 第一个有数据的层在 `level_sizes` 里的下标，第 0 项是 L1。
        let first_non_empty_index = level_sizes.iter().position(|&size| size > 0);
        if let Some(index) = first_non_empty_index {
            return Some((0, base_level.min(index + 1)));
        }
        return Some((0, base_level));
    }
    let level_count = level_sizes.len();
    let bottom = level_count - 1;
    // 目前分数最大的层：(分数, 层号)。只有分数大于 1 的层才放进来，分数还是 0 表示没有。
    let mut best_score = (0.0, 0);
    for i in 0..bottom {
        if targets[i] == 0 {
            if level_sizes[i] > 0 {
                return Some((i + 1, i + 2));
            }
            continue;
        }
        let score = level_sizes[i] as f64 / targets[i] as f64;
        if score > 1.0 && score > best_score.0 {
            best_score = (score, i + 1);
        }
    }
    if best_score.0 > 1.0 {
        return Some((best_score.1, best_score.1 + 1));
    }
    None
}

/// 看一份状态，给出下一个要做的任务；没有要合并的就返回 `None`。
///
/// 先按各层的字节数（SST 文件长度之和）由 `pick_levels` 决定哪层并进哪层，
/// 再由 `build_task` 挑出具体的文件。与另外两种策略一样，一次只给一个任务，
/// 做完、换进状态之后应当再调用一次。
pub fn generate_task(options: &LeveledOptions, state: &LsmStorageState) -> Option<LeveledTask> {
    let l0_file_count = state.l0_sstables.len();
    let level_sizes = state
        .levels
        .iter()
        .map(|level| level.iter().map(|sst| sst.file_size()).sum())
        .collect::<Vec<u64>>();
    pick_levels(options, l0_file_count, &level_sizes)
        .map(|(upper_level, lower_level)| build_task(state, upper_level, lower_level))
}

/// 按 `pick_levels` 的决定，从状态里挑出具体的文件，组成任务。
///
/// - 上层是 L0：拿全部 L0 文件（只拿一部分时，留下的较旧文件会挡住推下去的较新数据）。
/// - 上层是 Ln：由 `pick_upper_sst` 挑一个文件。
/// - 下层：与上层这些文件的**整体**范围（最小的首 key 到最大的末 key，见 `key_span`）
///   重叠的文件。按整体范围而不是逐个文件比，因为输出按大小切分，一个输出文件可能
///   横跨两个 L0 文件之间的空隙，盖住落在空隙里的下层文件。
/// - 上层的文件按首 key 排好之后彼此不重叠（`sorted_by_first_key` 之后 `is_sorted_run`）、
///   下层没有重叠的文件时，标为 trivial move。L0 的文件彼此不重叠，说明没有一个 key
///   同时在两个文件里，新旧无关紧要，它们本身就能拼成一个 sorted run。
fn build_task(state: &LsmStorageState, upper_level: usize, lower_level: usize) -> LeveledTask {
    let upper_ssts = if upper_level == 0 {
        state.l0_sstables.clone()
    } else {
        vec![pick_upper_sst(state.level_at(upper_level))]
    };
    let (first_key, last_key) = key_span(&upper_ssts);
    let lower_ssts = find_overlapping(state.level_at(lower_level), first_key, last_key);
    let is_trivial_move = is_sorted_run(&sorted_by_first_key(&upper_ssts)) && lower_ssts.is_empty();
    let is_lower_level_bottom = lower_level == state.levels.len();
    LeveledTask {
        upper_level,
        upper_ssts,
        lower_level,
        lower_ssts,
        is_lower_level_bottom,
        is_trivial_move,
    }
}

/// 从 L1 往下某一层里挑一个文件往下并：编号最小的，也就是在这一层待得最久的
/// （合并的输出会拿到新编号）。mini-lsm 的做法；换别的挑法只改这里。
/// `level` 不能是空的。
fn pick_upper_sst(level: &[Arc<SsTable>]) -> Arc<SsTable> {
    let oldest_sst = level
        .iter()
        .min_by_key(|sst| sst.id())
        .expect("一定存在至少一个文件");
    Arc::clone(oldest_sst)
}

/// 一组文件合起来的 key 范围：(最小的首 key, 最大的末 key)，两端都包含。
/// `ssts` 不能是空的。L0 的文件彼此可以重叠、也不按 key 排，所以要逐个比较。
fn key_span(ssts: &[Arc<SsTable>]) -> (&[u8], &[u8]) {
    let first_key_min = ssts
        .iter()
        .map(|sst| sst.first_key())
        .min()
        .expect("ssts 不能为空");
    let last_key_max = ssts
        .iter()
        .map(|sst| sst.last_key())
        .max()
        .expect("ssts 不能为空");
    (first_key_min, last_key_max)
}

/// `ssts`（一层的文件，按 key 排好）中与 `[first, last]`（两端都包含）重叠的文件，
/// 顺序不变。首末 key 相等也算重叠：两个文件都有这个 key，必须一起合并。
fn find_overlapping(ssts: &[Arc<SsTable>], first: &[u8], last: &[u8]) -> Vec<Arc<SsTable>> {
    ssts.iter()
        .filter(|sst| !(sst.last_key() < first || sst.first_key() > last))
        .map(Arc::clone)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_table::MemTable;
    use crate::table::SsTableBuilder;

    /// 触发值 2、比例 10、基准 16。数字都很小，把单位当成 MB 读，与讲解里的表对得上。
    fn options() -> LeveledOptions {
        LeveledOptions {
            level0_file_num_compaction_trigger: 2,
            level_size_multiplier: 10,
            base_level_size_bytes: 16,
        }
    }

    // ---------- 目标大小：target_sizes ----------

    // 空的树：只有最底层有目标，就是基准大小。
    #[test]
    fn empty_tree_targets_only_the_bottom() {
        assert_eq!(target_sizes(&options(), &[0, 0, 0, 0]), vec![0, 0, 0, 16]);
    }

    // 最底层不超过基准大小（包括正好等于）：上面还不需要缓冲层。
    #[test]
    fn small_bottom_keeps_the_upper_levels_off() {
        assert_eq!(target_sizes(&options(), &[0, 0, 0, 8]), vec![0, 0, 0, 16]);
        assert_eq!(target_sizes(&options(), &[0, 0, 0, 16]), vec![0, 0, 0, 16]);
    }

    // 最底层超过基准大小：上面一层启用，目标是最底层的 1/10（整数除法）。
    #[test]
    fn bottom_over_the_base_size_turns_on_one_level() {
        assert_eq!(target_sizes(&options(), &[0, 0, 0, 24]), vec![0, 0, 2, 24]);
        assert_eq!(
            target_sizes(&options(), &[0, 0, 0, 116]),
            vec![0, 0, 11, 116]
        );
    }

    // 数据继续变大：中间层从下往上一层层启用。
    // 1600 时 L2 的目标正好是 16，不超过基准，L1 还不启用；1700 时 L2 是 17，L1 启用。
    #[test]
    fn levels_turn_on_from_the_bottom_up() {
        assert_eq!(
            target_sizes(&options(), &[0, 0, 0, 1600]),
            vec![0, 16, 160, 1600]
        );
        assert_eq!(
            target_sizes(&options(), &[0, 0, 0, 1700]),
            vec![1, 17, 170, 1700]
        );
        assert_eq!(
            target_sizes(&options(), &[0, 0, 0, 2000]),
            vec![2, 20, 200, 2000]
        );
    }

    // 目标只看最底层：上面各层有多少数据都不改变目标。
    #[test]
    fn targets_ignore_the_upper_levels() {
        assert_eq!(
            target_sizes(&options(), &[50, 0, 300, 116]),
            vec![0, 0, 11, 116]
        );
    }

    // 只有一层：它既是最底层也是基准层，目标是实际大小与基准大小中较大的。
    #[test]
    fn a_single_level_is_its_own_target() {
        assert_eq!(target_sizes(&options(), &[5]), vec![16]);
        assert_eq!(target_sizes(&options(), &[500]), vec![500]);
    }

    // 对很多种最底层的大小检查两条性质：大于 0 的目标是连续的一段、一直到最底层，
    // 并且其中最多只有一层（基准层）的目标不超过基准大小。
    #[test]
    fn at_most_one_positive_target_is_not_above_the_base_size() {
        let options = options();
        for bottom in (0..100_000).step_by(7) {
            let targets = target_sizes(&options, &[0, 0, 0, 0, 0, bottom]);
            let first_positive = targets.iter().position(|&t| t > 0).unwrap();
            assert!(
                targets[first_positive..].iter().all(|&t| t > 0),
                "最底层 {bottom}：{targets:?}"
            );
            let small_count = targets
                .iter()
                .filter(|&&t| t > 0 && t <= options.base_level_size_bytes)
                .count();
            assert!(small_count <= 1, "最底层 {bottom}：{targets:?}");
        }
    }

    // ---------- 基准层：base_level ----------

    // 第一个目标大于 0 的层，层号从 1 开始。
    #[test]
    fn base_level_is_the_first_positive_target() {
        assert_eq!(base_level(&[0, 0, 0, 16]), 4);
        assert_eq!(base_level(&[0, 0, 11, 116]), 3);
        assert_eq!(base_level(&[0, 16, 160, 1600]), 2);
        assert_eq!(base_level(&[2, 20, 200, 2000]), 1);
        assert_eq!(base_level(&[16]), 1);
    }

    // ---------- 选层：pick_levels ----------

    // L0 不到触发值，各层都没超过目标：没事可做。
    #[test]
    fn nothing_to_do() {
        assert_eq!(pick_levels(&options(), 0, &[0, 0, 0, 0]), None);
        assert_eq!(pick_levels(&options(), 1, &[0, 0, 0, 8]), None);
        assert_eq!(pick_levels(&options(), 1, &[0, 0, 11, 116]), None);
    }

    // L0 达到触发值：并进基准层，而不是 L1。
    #[test]
    fn l0_goes_to_the_base_level() {
        assert_eq!(pick_levels(&options(), 2, &[0, 0, 0, 8]), Some((0, 4)));
        assert_eq!(pick_levels(&options(), 2, &[0, 0, 11, 116]), Some((0, 3)));
        assert_eq!(
            pick_levels(&options(), 3, &[0, 16, 160, 1600]),
            Some((0, 2))
        );
    }

    // L0 达到触发值，基准层上面却有数据的层（数据变少、基准层下移后留下的，或从简单分层
    // 换过来的）：并进第一个有数据的层，不能跳过它。跳过的话，L0 的新值落在它下面，
    // 点查先查到它里面的旧值。
    #[test]
    fn l0_does_not_skip_a_level_with_data() {
        // 基准层是 L3，L1 有数据：并进 L1。
        assert_eq!(pick_levels(&options(), 2, &[5, 0, 11, 116]), Some((0, 1)));
        // 基准层是 L3，L2 有数据：并进 L2。
        assert_eq!(pick_levels(&options(), 2, &[0, 30, 100, 116]), Some((0, 2)));
        // 第一个有数据的层在基准层下面（L3 空着、L4 有数据）：照样并进基准层 L3。
        assert_eq!(pick_levels(&options(), 2, &[0, 0, 0, 116]), Some((0, 3)));
    }

    // L0 达到触发值，同时 L3 远超目标：先做 L0。
    #[test]
    fn l0_comes_before_the_scores() {
        assert_eq!(pick_levels(&options(), 2, &[0, 0, 100, 116]), Some((0, 3)));
    }

    // 超过目标的层并进下一层；正好等于目标（分数 1）不算超过。
    #[test]
    fn a_level_over_its_target_goes_down() {
        // L3 的目标是 11。
        assert_eq!(pick_levels(&options(), 0, &[0, 0, 11, 116]), None);
        assert_eq!(pick_levels(&options(), 0, &[0, 0, 12, 116]), Some((3, 4)));
        assert_eq!(pick_levels(&options(), 0, &[0, 0, 20, 116]), Some((3, 4)));
    }

    // 好几层都超过目标：取分数最大的，不是最上面的。
    #[test]
    fn the_highest_score_wins() {
        // 目标 [0, 16, 160, 1600]。L2 40 ÷ 16 = 2.5，L3 160 ÷ 160 = 1：选 L2。
        assert_eq!(
            pick_levels(&options(), 0, &[0, 40, 160, 1600]),
            Some((2, 3))
        );
        // L2 20 ÷ 16 = 1.25，L3 480 ÷ 160 = 3：选 L3。
        assert_eq!(
            pick_levels(&options(), 0, &[0, 20, 480, 1600]),
            Some((3, 4))
        );
    }

    // 分数相同：取上面的层。
    #[test]
    fn a_tie_goes_to_the_upper_level() {
        // L2 32 ÷ 16 = 2，L3 320 ÷ 160 = 2。
        assert_eq!(
            pick_levels(&options(), 0, &[0, 32, 320, 1600]),
            Some((2, 3))
        );
    }

    // 基准层上面的层目标是 0，却有数据（数据变少、基准层下移后留下的）：最先往下并。
    #[test]
    fn data_above_the_base_level_goes_down_first() {
        // 基准层是 L3；L1 有 5，目标 0。
        assert_eq!(pick_levels(&options(), 0, &[5, 0, 11, 116]), Some((1, 2)));
        // L2 有 30、目标 0，比远超目标的 L3（100 ÷ 11）还优先。
        assert_eq!(pick_levels(&options(), 0, &[0, 30, 100, 116]), Some((2, 3)));
    }

    // 最底层没有下一层：再大也不选。
    #[test]
    fn the_bottom_level_is_never_picked() {
        assert_eq!(pick_levels(&options(), 0, &[0, 0, 0, 100_000]), None);
        assert_eq!(pick_levels(&options(), 0, &[500]), None);
    }

    // 只有一层：只可能是 L0 并进 L1。
    #[test]
    fn a_single_level_only_takes_l0() {
        assert_eq!(pick_levels(&options(), 2, &[500]), Some((0, 1)));
    }

    // 比例为 1 的参数不合法：上层的目标会和下层一样大。
    #[test]
    #[should_panic(expected = "level_size_multiplier 至少为 2")]
    fn multiplier_below_two_panics() {
        let options = LeveledOptions {
            level_size_multiplier: 1,
            ..options()
        };
        pick_levels(&options, 0, &[0, 0, 0, 0]);
    }

    // ---------- 挑文件：find_overlapping、key_span、pick_upper_sst、build_task ----------

    /// key 的写法：`key-0100`。位数固定，字节序与数值大小一致。
    fn key(n: u32) -> Vec<u8> {
        format!("key-{n:04}").into_bytes()
    }

    /// 在 `dir` 里写一个编号为 `id`、key 范围是 `[first, last]` 的 SST：只放首末两条记录
    /// （`first == last` 时一条）。
    fn sst(dir: &tempfile::TempDir, id: usize, first: u32, last: u32) -> Arc<SsTable> {
        let path = dir.path().join(format!("{id:05}.sst"));
        let mut builder = SsTableBuilder::new(4096);
        builder.add(&key(first), b"value");
        if last != first {
            builder.add(&key(last), b"value");
        }
        Arc::new(builder.build(id, None, &path).unwrap())
    }

    /// 一个文件的描述：(编号, 首 key, 末 key)。
    type Spec = (usize, u32, u32);

    /// 一组文件。
    fn ssts(dir: &tempfile::TempDir, specs: &[Spec]) -> Vec<Arc<SsTable>> {
        specs
            .iter()
            .map(|&(id, first, last)| sst(dir, id, first, last))
            .collect()
    }

    /// 一份只有 SST 的状态：`l0` 是 L0 的文件（新的在前），`levels[i]` 是 L(i+1) 的。
    fn state_with(dir: &tempfile::TempDir, l0: &[Spec], levels: &[&[Spec]]) -> LsmStorageState {
        LsmStorageState {
            memtable: Arc::new(MemTable::new()),
            imm_memtables: Vec::new(),
            l0_sstables: ssts(dir, l0),
            levels: levels.iter().map(|specs| ssts(dir, specs)).collect(),
        }
    }

    /// 一组 SST 的编号。
    fn ids(ssts: &[Arc<SsTable>]) -> Vec<usize> {
        ssts.iter().map(|sst| sst.id()).collect()
    }

    // 教程的例子：上层 [100, 200]，下层四个文件里中间两个与它重叠。
    #[test]
    fn overlapping_files_in_the_middle() {
        let dir = tempfile::tempdir().unwrap();
        let lower = ssts(
            &dir,
            &[(1, 50, 99), (2, 100, 150), (3, 151, 250), (4, 251, 300)],
        );
        let found = find_overlapping(&lower, &key(100), &key(200));
        assert_eq!(ids(&found), vec![2, 3]);
    }

    // 只在端点上相等也算重叠；差一个就不算。
    #[test]
    fn touching_endpoints_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let lower = ssts(&dir, &[(1, 50, 100), (2, 200, 300)]);
        assert_eq!(
            ids(&find_overlapping(&lower, &key(100), &key(200))),
            vec![1, 2]
        );

        let lower = ssts(&dir, &[(3, 50, 99), (4, 201, 300)]);
        assert!(find_overlapping(&lower, &key(100), &key(200)).is_empty());
    }

    // 范围落在下层一个文件的内部、或者盖住下层全部文件；下层是空的。
    #[test]
    fn overlap_inside_one_file_or_covering_all() {
        let dir = tempfile::tempdir().unwrap();
        let lower = ssts(&dir, &[(1, 100, 200), (2, 300, 400)]);
        assert_eq!(
            ids(&find_overlapping(&lower, &key(120), &key(130))),
            vec![1]
        );
        assert_eq!(
            ids(&find_overlapping(&lower, &key(0), &key(999))),
            vec![1, 2]
        );
        assert!(find_overlapping(&[], &key(0), &key(999)).is_empty());
    }

    // 整体范围：最小的首 key 与最大的末 key，不管文件的先后。
    #[test]
    fn key_span_covers_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = ssts(&dir, &[(12, 50, 60), (11, 10, 20), (10, 15, 55)]);
        assert_eq!(key_span(&files), (key(10).as_slice(), key(60).as_slice()));

        let one = ssts(&dir, &[(9, 30, 40)]);
        assert_eq!(key_span(&one), (key(30).as_slice(), key(40).as_slice()));
    }

    // 挑编号最小的文件，不管它在层里的位置。
    #[test]
    fn the_oldest_file_is_picked() {
        let dir = tempfile::tempdir().unwrap();
        let level = ssts(&dir, &[(9, 1, 10), (3, 20, 30), (7, 40, 50)]);
        assert_eq!(pick_upper_sst(&level).id(), 3);
    }

    // L0 的任务：拿全部 L0（顺序不变），下层按整体范围 [10, 60] 找重叠。
    // L0 两个文件之间的空隙里有下层的 7 号 [35, 45]：它也要一起合并。
    #[test]
    fn l0_task_uses_the_whole_span() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            &dir,
            &[(12, 10, 20), (11, 50, 60)],
            &[
                &[],
                &[(5, 1, 5), (6, 15, 30), (7, 35, 45), (8, 70, 80)],
                &[(1, 0, 999)],
            ],
        );

        let task = build_task(&state, 0, 2);

        assert_eq!(task.upper_level, 0);
        assert_eq!(ids(&task.upper_ssts), vec![12, 11]);
        assert_eq!(task.lower_level, 2);
        assert_eq!(ids(&task.lower_ssts), vec![6, 7]);
        assert!(!task.is_lower_level_bottom);
        assert!(!task.is_trivial_move);
    }

    // 层与层之间的任务：上层只挑最旧的一个，下层只拉进与它重叠的；下层是最后一层时标为最底层。
    #[test]
    fn level_task_takes_one_file_and_its_overlaps() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            &dir,
            &[],
            &[
                &[],
                &[(20, 0, 99), (14, 100, 200), (21, 201, 300)],
                &[
                    (1, 0, 49),
                    (2, 50, 120),
                    (3, 121, 180),
                    (4, 181, 260),
                    (5, 261, 400),
                ],
            ],
        );

        let task = build_task(&state, 2, 3);

        assert_eq!(task.upper_level, 2);
        assert_eq!(ids(&task.upper_ssts), vec![14]);
        assert_eq!(task.lower_level, 3);
        assert_eq!(ids(&task.lower_ssts), vec![2, 3, 4]);
        assert!(task.is_lower_level_bottom);
        assert!(!task.is_trivial_move);
    }

    // 下层没有与它重叠的文件（包括下层是空的）：trivial move。
    #[test]
    fn no_overlap_is_a_trivial_move() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            &dir,
            &[],
            &[&[(14, 100, 200)], &[(1, 0, 50), (2, 300, 400)], &[]],
        );
        let task = build_task(&state, 1, 2);
        assert_eq!(ids(&task.upper_ssts), vec![14]);
        assert!(task.lower_ssts.is_empty());
        assert!(task.is_trivial_move);
        assert!(!task.is_lower_level_bottom);

        let state = state_with(&dir, &[], &[&[(15, 100, 200)], &[]]);
        let task = build_task(&state, 1, 2);
        assert!(task.is_trivial_move);
        assert!(task.is_lower_level_bottom);
    }

    // 只有一个 L0 文件、基准层没有重叠：trivial move。
    #[test]
    fn a_single_l0_file_can_move() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[(12, 10, 20)], &[&[(1, 50, 60)]]);
        let task = build_task(&state, 0, 1);
        assert!(task.is_trivial_move);
        assert!(task.lower_ssts.is_empty());
    }

    // 几个 L0 文件彼此不重叠、基准层也没有重叠：一起挪；任务里的顺序仍与状态里相同
    // （新的在前），按 key 排好是执行的事。
    #[test]
    fn l0_files_that_do_not_overlap_move_together() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            &dir,
            &[(14, 30, 40), (13, 10, 20), (12, 50, 60)],
            &[&[(1, 70, 80)]],
        );
        let task = build_task(&state, 0, 1);
        assert!(task.is_trivial_move);
        assert_eq!(ids(&task.upper_ssts), vec![14, 13, 12]);
        assert!(task.lower_ssts.is_empty());
    }

    // L0 的文件彼此重叠（包括只在端点上相等：那个 key 两个文件里都有）：要合并，不能挪。
    #[test]
    fn overlapping_l0_files_are_merged() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[(14, 15, 35), (13, 10, 20)], &[&[(1, 70, 80)]]);
        assert!(!build_task(&state, 0, 1).is_trivial_move);

        let state = state_with(&dir, &[(16, 20, 30), (15, 10, 20)], &[&[(2, 70, 80)]]);
        assert!(!build_task(&state, 0, 1).is_trivial_move);
    }

    // L0 的文件彼此不重叠，但基准层有文件落在它们的整体范围里：要合并。
    #[test]
    fn l0_files_around_a_lower_file_are_merged() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[(14, 50, 60), (13, 10, 20)], &[&[(1, 30, 40)]]);
        let task = build_task(&state, 0, 1);
        assert!(!task.is_trivial_move);
        assert_eq!(ids(&task.lower_ssts), vec![1]);
    }

    // 按首 key 排好的副本；传进来的列表顺序不变。
    #[test]
    fn sorted_by_first_key_makes_a_sorted_copy() {
        let dir = tempfile::tempdir().unwrap();
        let files = ssts(&dir, &[(14, 30, 40), (13, 10, 20), (12, 50, 60)]);
        assert_eq!(ids(&sorted_by_first_key(&files)), vec![13, 14, 12]);
        assert_eq!(ids(&files), vec![14, 13, 12]);
        assert!(sorted_by_first_key(&[]).is_empty());
    }

    // sorted run：相邻两个之间要有空隙，端点相等不算；顺序乱了也不算。
    #[test]
    fn is_sorted_run_needs_order_and_gaps() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_sorted_run(&ssts(&dir, &[(1, 10, 20), (2, 21, 30)])));
        assert!(!is_sorted_run(&ssts(&dir, &[(3, 10, 20), (4, 20, 30)])));
        assert!(!is_sorted_run(&ssts(&dir, &[(5, 21, 30), (6, 10, 20)])));
        assert!(is_sorted_run(&[]));
    }

    // ---------- 从状态生成任务：generate_task ----------

    // 空的状态：没事可做。
    #[test]
    fn no_task_for_an_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[], &[&[], &[], &[], &[]]);
        assert!(generate_task(&options(), &state).is_none());
    }

    // 空的树攒够 L0：直接并进最底层（基准层），不经过上面的空层。
    #[test]
    fn l0_of_an_empty_tree_goes_to_the_bottom() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[(12, 10, 20), (11, 30, 40)], &[&[], &[], &[], &[]]);

        let task = generate_task(&options(), &state).unwrap();

        assert_eq!(task.upper_level, 0);
        assert_eq!(ids(&task.upper_ssts), vec![12, 11]);
        assert_eq!(task.lower_level, 4);
        assert!(task.is_lower_level_bottom);
    }

    // 按字节比较：L3 有 3 个文件、最底层 1 个，基准大小 1 字节、比例 10 时，
    // L3 的目标约是一个文件的 1/10，远超目标，挑 L3 最旧的文件并进 L4。
    #[test]
    fn an_overfull_level_goes_down_by_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(
            &dir,
            &[],
            &[
                &[],
                &[],
                &[(9, 0, 99), (7, 100, 199), (8, 200, 299)],
                &[(1, 0, 299)],
            ],
        );
        let options = LeveledOptions {
            base_level_size_bytes: 1,
            ..options()
        };

        let task = generate_task(&options, &state).unwrap();

        assert_eq!(task.upper_level, 3);
        assert_eq!(ids(&task.upper_ssts), vec![7]);
        assert_eq!(task.lower_level, 4);
        assert_eq!(ids(&task.lower_ssts), vec![1]);
    }

    // 比的是字节，不是文件数：L3 两个只有两条记录的小文件，最底层一个 2000 条记录的大文件。
    // 按字节 L3 远没到目标（大文件的 1/10），不合并；按文件数会以为 L3 比最底层还多。
    #[test]
    fn small_files_do_not_count_as_a_full_level() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state_with(&dir, &[], &[&[], &[], &[(7, 0, 99), (8, 100, 199)], &[]]);
        let path = dir.path().join("00001.sst");
        let mut builder = SsTableBuilder::new(4096);
        for n in 0..2000 {
            builder.add(&key(n), b"value");
        }
        state.levels[3].push(Arc::new(builder.build(1, None, &path).unwrap()));
        let options = LeveledOptions {
            base_level_size_bytes: 1,
            ..options()
        };

        assert!(generate_task(&options, &state).is_none());
    }
}
