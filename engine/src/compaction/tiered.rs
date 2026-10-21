//! tiered 策略（RocksDB 叫 universal）：状态是一列按新旧排列的 tier，每次合并从最新一端
//! 开始的连续几个 tier，结果作为一个 tier 放回原位。
//!
//! 这种策略不用 L0：flush 出的每个 SST 自己就是一个新的 tier，放在最前面。tier 放在状态的
//! `levels` 里，`levels[0]` 最新；每个 tier 是一个 sorted run（按 key 排好、互不重叠），
//! 个数随 flush 与合并变化，没有空的 tier。
//!
//! tier 数达到 `num_tiers` 才开始检查，按顺序看三种触发，第一个成立的给出任务：
//!
//! 1. 空间放大：除最底以外所有 tier 的大小之和 ÷ 最底 tier 的大小达到
//!    `max_size_amplification_percent`%，全部合并。
//! 2. 大小比例：从最新的往旧的累加，下一个 tier 比前面的总和大出 `size_ratio`% 以上，
//!    并且前面至少有 `min_merge_width` 个 tier，就合并前面这几个。
//! 3. 减少段数：前两种都不成立，合并最新的 `max_merge_width` 个（不设就全部）。
//!
//! 教程为了简单，tier 的大小用文件数；真实的引擎用字节数。

use crate::lsm_storage::LsmStorageState;
use crate::table::SsTable;
use std::sync::Arc;

/// tiered 策略的参数。默认值可以参照 RocksDB：200、1、2、不限。
#[derive(Debug, Clone)]
pub struct TieredOptions {
    /// tier 数达到这么多才检查三种触发，不到就不合并。它不是上限：三种触发都不成立时，
    /// tier 数可以超过它。至少为 2。
    pub num_tiers: usize,
    /// 第一种触发的阈值，百分数：除最底以外所有 tier 的大小之和达到最底 tier 的这么多，
    /// 就全部合并一次。例如 200 表示上面积的数据到了最底的 2 倍。
    pub max_size_amplification_percent: usize,
    /// 第二种触发判断"大得多"的宽容度，百分数：下一个 tier 比前面几个的总和大出这么多以上，
    /// 才算大得多。例如 1 表示大出 1% 以上。
    pub size_ratio: usize,
    /// 第二种触发至少合并几个 tier。至少为 2：只"合并"一个 tier 等于原样重写它。
    pub min_merge_width: usize,
    /// 第三种触发最多合并几个 tier；`None` 表示不限，全部合并。给出时至少为 2。
    pub max_merge_width: Option<usize>,
}

/// 是哪一种触发选中了这次合并。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TieredTrigger {
    /// 第一种：上面积的数据相对最底 tier 太多，全部合并。
    SpaceAmplification,
    /// 第二种：前面几个 tier 加起来比下一个小得多，合并它们。
    SizeRatio,
    /// 第三种：前两种都不成立，只为了减少 tier 的个数。
    TierCount,
}

/// 一次合并的任务：把最新的几个 tier 合并成一个，放回它们原来的位置。
///
/// tier 列表是生成任务那一刻的状态里的：执行时只读这些文件，换状态时只换掉这些 tier。
/// 这期间 flush 可能在最前面加了新的 tier，它们不在任务里，换状态之后仍然在合并结果之前。
pub struct TieredTask {
    /// 参加合并的 tier，从最新一端开始、连续的几个，顺序与状态里相同（新的在前）。
    pub tiers: Vec<Vec<Arc<SsTable>>>,
    /// 是否包含了最底（最旧）的 tier。包含时下面没有更旧的版本，合并时可以丢掉墓碑；
    /// 不包含时墓碑必须留下，更旧的 tier 里可能还有被它盖住的值。
    pub bottom_tier_included: bool,
}

/// 看一份状态，给出下一个要做的任务；没有要合并的就返回 `None`。
///
/// 与简单分层一样，一次只给一个任务，做完、换进状态之后应当再调用一次。反复调用总会停下：
/// 每个任务至少把 2 个 tier 合成 1 个，tier 数严格减少，而不到 `num_tiers` 时不再给任务。
pub fn generate_task(options: &TieredOptions, state: &LsmStorageState) -> Option<TieredTask> {
    assert!(
        state.l0_sstables.is_empty(),
        "tiered 策略不用 L0，flush 出的文件应当成为新的 tier"
    );

    // 各 tier 的大小（文件数），新的在前。
    let mut tier_sizes = Vec::new();
    for tier in &state.levels {
        tier_sizes.push(tier.len());
    }

    let (_trigger, count) = pick_tiers(options, &tier_sizes)?;
    Some(TieredTask {
        tiers: state.levels[..count].to_vec(),
        bottom_tier_included: count == state.levels.len(),
    })
}

/// 只看各 tier 的大小，决定合并最新的几个 tier，返回是哪种触发与合并的个数；
/// 不需要合并时返回 `None`。
///
/// `tier_sizes` 新的在前，最后一项是最底的 tier。返回的个数至少是 2。
fn pick_tiers(options: &TieredOptions, tier_sizes: &[usize]) -> Option<(TieredTrigger, usize)> {
    check_options(options);
    let tier_count = tier_sizes.len();
    if tier_count < options.num_tiers {
        return None;
    }

    // 第一种：空间放大。上面之和 ÷ 最底 ≥ 阈值 ÷ 100，两边同乘 100 × 最底换成整数比较。
    let bottom_size = tier_sizes[tier_count - 1];
    let upper_size: usize = tier_sizes[..tier_count - 1].iter().sum();
    if upper_size * 100 >= options.max_size_amplification_percent * bottom_size {
        return Some((TieredTrigger::SpaceAmplification, tier_count));
    }

    // 第二种：大小比例。`newer_size` 是前 `width` 个 tier 的大小之和，拿下一个 tier 与它比。
    // 下一个 ÷ 前面之和 > (100 + size_ratio) ÷ 100，同样两边同乘换成整数比较。
    let mut newer_size = 0;
    for width in 1..tier_count {
        newer_size += tier_sizes[width - 1];
        let next_size = tier_sizes[width];
        let next_is_much_larger = next_size * 100 > (100 + options.size_ratio) * newer_size;
        if next_is_much_larger && width >= options.min_merge_width {
            return Some((TieredTrigger::SizeRatio, width));
        }
    }

    // 第三种：减少段数。
    let max_width = options.max_merge_width.unwrap_or(usize::MAX);
    Some((TieredTrigger::TierCount, tier_count.min(max_width)))
}

/// 检查参数：每个任务至少合并 2 个 tier，反复要任务才会停下。
fn check_options(options: &TieredOptions) {
    assert!(
        options.num_tiers >= 2,
        "num_tiers 至少为 2，否则只剩一个 tier 也会不停地合并它自己"
    );
    assert!(
        options.min_merge_width >= 2,
        "min_merge_width 至少为 2，否则会不停地原样重写同一个 tier"
    );
    if let Some(max_width) = options.max_merge_width {
        assert!(
            max_width >= 2,
            "max_merge_width 至少为 2，否则会不停地原样重写同一个 tier"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_table::MemTable;
    use crate::table::SsTableBuilder;

    /// mini-lsm 模拟器的参数：8 个 tier 开始检查，空间放大 200%，比例 1%，至少合并 2 个，
    /// 最多不限。
    fn options() -> TieredOptions {
        TieredOptions {
            num_tiers: 8,
            max_size_amplification_percent: 200,
            size_ratio: 1,
            min_merge_width: 2,
            max_merge_width: None,
        }
    }

    /// 同样的参数，只是 tier 数到 `num_tiers` 就检查。
    fn options_with_num_tiers(num_tiers: usize) -> TieredOptions {
        TieredOptions {
            num_tiers,
            ..options()
        }
    }

    // ---------- 只看大小：pick_tiers ----------

    // tier 数不到 num_tiers：形状再差也不合并。
    #[test]
    fn nothing_to_do_below_num_tiers() {
        let options = options_with_num_tiers(4);
        assert_eq!(pick_tiers(&options, &[]), None);
        assert_eq!(pick_tiers(&options, &[1, 1, 1]), None);
        assert_eq!(pick_tiers(&options, &[100, 1, 1]), None);
    }

    // 教程的例子：上面 1 + 1 = 2，最底 1，正好 200%：全部合并。
    #[test]
    fn space_amplification_merges_everything() {
        let options = options_with_num_tiers(3);
        assert_eq!(
            pick_tiers(&options, &[1, 1, 1]),
            Some((TieredTrigger::SpaceAmplification, 3))
        );
    }

    // 上面之和不到最底的 200%：空间放大不成立。
    #[test]
    fn space_amplification_below_the_threshold() {
        let options = options_with_num_tiers(3);
        // 2 ÷ 3 ≈ 67%；比例那一步 1 ÷ 1 不够、3 ÷ 2 够了，由第二种合并前 2 个。
        assert_eq!(
            pick_tiers(&options, &[1, 1, 3]),
            Some((TieredTrigger::SizeRatio, 2))
        );
    }

    // 空间放大与大小比例都成立时，空间放大优先。
    #[test]
    fn space_amplification_comes_before_size_ratio() {
        let options = options_with_num_tiers(3);
        // 比例：1 ÷ 1 不够、5 ÷ 2 够了；空间：(1 + 1 + 5) ÷ 1 远超 200%。
        assert_eq!(
            pick_tiers(&options, &[1, 1, 5, 1]),
            Some((TieredTrigger::SpaceAmplification, 4))
        );
    }

    // 教程的例 3：2 ÷ 1 超过了，但前缀只有 1 个，不算；4 ÷ 3 超过，合并前 2 个。
    #[test]
    fn size_ratio_needs_min_merge_width() {
        let options = options_with_num_tiers(3);
        assert_eq!(
            pick_tiers(&options, &[1, 2, 4]),
            Some((TieredTrigger::SizeRatio, 2))
        );
    }

    // 下一个正好是前面之和的 (100 + size_ratio)%：不算大得多。
    #[test]
    fn size_ratio_exactly_at_the_threshold_does_not_trigger() {
        let options = TieredOptions {
            num_tiers: 3,
            size_ratio: 100,
            ..options()
        };
        // 1 ÷ 1 = 100%、4 ÷ 2 = 200%，都不超过 200%；空间 6 ÷ 10 不够。只剩第三种。
        assert_eq!(
            pick_tiers(&options, &[1, 1, 4, 10]),
            Some((TieredTrigger::TierCount, 4))
        );
        // 5 ÷ 2 = 250%，超过了。
        assert_eq!(
            pick_tiers(&options, &[1, 1, 5, 10]),
            Some((TieredTrigger::SizeRatio, 2))
        );
    }

    // 完美的倍增形状：每个 tier 都正好等于前面之和，比例不成立；最底够大，空间也不成立。
    // tier 太多，由第三种全部合并。
    #[test]
    fn tier_count_merges_a_well_shaped_but_long_list() {
        let options = options_with_num_tiers(4);
        assert_eq!(
            pick_tiers(&options, &[1, 1, 2, 4, 8]),
            Some((TieredTrigger::TierCount, 5))
        );
    }

    // 前缀只有 1 个时比例不算，后面又都不够：落到第三种。
    #[test]
    fn tier_count_after_min_merge_width_blocks_the_ratio() {
        let options = options_with_num_tiers(3);
        assert_eq!(
            pick_tiers(&options, &[1, 2, 3]),
            Some((TieredTrigger::TierCount, 3))
        );
    }

    // 第三种最多合并 max_merge_width 个。
    #[test]
    fn tier_count_respects_max_merge_width() {
        let options = TieredOptions {
            num_tiers: 4,
            max_merge_width: Some(2),
            ..options()
        };
        assert_eq!(
            pick_tiers(&options, &[1, 1, 2, 4, 8]),
            Some((TieredTrigger::TierCount, 2))
        );
    }

    // 参数不合法时直接报错，而不是让后台线程停不下来。
    #[test]
    #[should_panic(expected = "num_tiers 至少为 2")]
    fn num_tiers_below_two_is_rejected() {
        pick_tiers(&options_with_num_tiers(1), &[1]);
    }

    #[test]
    #[should_panic(expected = "min_merge_width 至少为 2")]
    fn min_merge_width_below_two_is_rejected() {
        let options = TieredOptions {
            min_merge_width: 1,
            ..options()
        };
        pick_tiers(&options, &[1, 5]);
    }

    #[test]
    #[should_panic(expected = "max_merge_width 至少为 2")]
    fn max_merge_width_below_two_is_rejected() {
        let options = TieredOptions {
            max_merge_width: Some(1),
            ..options()
        };
        pick_tiers(&options, &[1, 5]);
    }

    // ---------- 模拟 ----------

    /// 按文件数模拟的结果。
    struct Simulation {
        /// 最后的形状：各 tier 的文件数，新的在前。
        tier_sizes: Vec<usize>,
        /// flush 与合并一共写出的文件数。
        files_written: usize,
        /// 三种触发各发生了几次：空间放大、大小比例、减少段数。
        trigger_counts: [usize; 3],
    }

    /// 按文件数模拟：每次 flush 在最前面加一个只有 1 个文件的 tier，然后反复要任务、"执行"，
    /// 直到没有任务。"执行"就是把选中的几个 tier 换成一个，文件数是它们之和
    /// （与 mini-lsm 的模拟器一样，不考虑合并时去掉重复的 key）。
    ///
    /// 每次 flush 之后要的任务超过 100 个就判定为停不下来。
    fn simulate(options: &TieredOptions, flushes: usize) -> Simulation {
        let mut tier_sizes: Vec<usize> = Vec::new();
        let mut files_written = 0;
        let mut trigger_counts = [0; 3];
        for flush in 1..=flushes {
            tier_sizes.insert(0, 1);
            files_written += 1;
            let mut tasks = 0;
            while let Some((trigger, count)) = pick_tiers(options, &tier_sizes) {
                tasks += 1;
                assert!(
                    tasks <= 100,
                    "第 {flush} 次 flush 之后停不下来：{tier_sizes:?}"
                );
                println!("  {trigger:?} 合并前 {count} 个：{tier_sizes:?}");
                let trigger_index = match trigger {
                    TieredTrigger::SpaceAmplification => 0,
                    TieredTrigger::SizeRatio => 1,
                    TieredTrigger::TierCount => 2,
                };
                trigger_counts[trigger_index] += 1;
                let merged_size: usize = tier_sizes[..count].iter().sum();
                files_written += merged_size;
                tier_sizes.splice(..count, [merged_size]);
            }
            println!("第 {flush} 次 flush 之后：{tier_sizes:?}");
        }
        Simulation {
            tier_sizes,
            files_written,
            trigger_counts,
        }
    }

    // 与 mini-lsm 的参考模拟器对照（num_tiers 为 8 与 16，flush 200 次）：最后的形状和写出的
    // 文件数与教程里给出的一样。三种触发里绝大多数是大小比例。
    // 想看每一步怎样合并：cargo test -p lsm tiered::tests::simulation -- --nocapture
    #[test]
    fn simulation_matches_the_reference_simulator() {
        let eight = simulate(&options_with_num_tiers(8), 200);
        assert_eq!(eight.tier_sizes, vec![1, 1, 4, 5, 21, 28, 140]);
        assert_eq!(eight.files_written, 742);
        assert_eq!(eight.trigger_counts, [2, 52, 2]);

        let sixteen = simulate(&options_with_num_tiers(16), 200);
        let mut expected_sizes = vec![1; 10];
        expected_sizes.extend([15, 175]);
        assert_eq!(sixteen.tier_sizes, expected_sizes);
        assert_eq!(sixteen.files_written, 607);
    }

    // 每次 flush 之后都能停下，停下时 tier 数不到 num_tiers；文件一个不少。
    #[test]
    fn simulation_always_settles_below_num_tiers() {
        for num_tiers in [2, 3, 4, 8] {
            let options = options_with_num_tiers(num_tiers);
            let mut tier_sizes: Vec<usize> = Vec::new();
            for flush in 1..=200 {
                tier_sizes.insert(0, 1);
                let mut tasks = 0;
                while let Some((_trigger, count)) = pick_tiers(&options, &tier_sizes) {
                    tasks += 1;
                    assert!(
                        tasks <= 100,
                        "num_tiers {num_tiers}，第 {flush} 次 flush 之后停不下来：{tier_sizes:?}"
                    );
                    let merged_size: usize = tier_sizes[..count].iter().sum();
                    tier_sizes.splice(..count, [merged_size]);
                }
                assert!(tier_sizes.len() < num_tiers, "{tier_sizes:?}");
                assert_eq!(tier_sizes.iter().sum::<usize>(), flush);
            }
        }
    }

    // ---------- 从状态生成任务：generate_task ----------

    /// 在 `dir` 里写一个只有一条记录的 SST，编号为 `id`。
    fn tiny_sst(dir: &tempfile::TempDir, id: usize) -> Arc<SsTable> {
        let path = dir.path().join(format!("{id:05}.sst"));
        let mut builder = SsTableBuilder::new(64);
        builder.add(format!("key-{id}").as_bytes(), b"value");
        Arc::new(builder.build(id, None, &path).unwrap())
    }

    /// 一份只有 tier 的状态：`tiers[i]` 是第 i 个 tier 各文件的编号，新的 tier 在前。
    fn state_with(dir: &tempfile::TempDir, tiers: &[&[usize]]) -> LsmStorageState {
        let to_ssts = |ids: &[usize]| -> Vec<Arc<SsTable>> {
            ids.iter().map(|id| tiny_sst(dir, *id)).collect()
        };
        LsmStorageState {
            memtable: Arc::new(MemTable::new(0)),
            imm_memtables: Vec::new(),
            l0_sstables: Vec::new(),
            levels: tiers.iter().map(|ids| to_ssts(ids)).collect(),
        }
    }

    /// 每个 tier 各文件的编号。
    fn ids(tiers: &[Vec<Arc<SsTable>>]) -> Vec<Vec<usize>> {
        tiers
            .iter()
            .map(|tier| tier.iter().map(|sst| sst.id()).collect())
            .collect()
    }

    // 大小比例的任务：带上最新的几个 tier（顺序不变），没有带上最底，墓碑要留下。
    #[test]
    fn size_ratio_task_takes_the_newest_tiers_without_the_bottom() {
        let dir = tempfile::tempdir().unwrap();
        // 大小 [1, 1, 3]：合并前 2 个。
        let state = state_with(&dir, &[&[9], &[8], &[5, 6, 7]]);

        let task = generate_task(&options_with_num_tiers(3), &state).unwrap();

        assert_eq!(ids(&task.tiers), vec![vec![9], vec![8]]);
        assert!(!task.bottom_tier_included);
    }

    // 空间放大的任务：带上全部 tier，包含最底。
    #[test]
    fn space_amplification_task_includes_the_bottom() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[&[9], &[8], &[7]]);

        let task = generate_task(&options_with_num_tiers(3), &state).unwrap();

        assert_eq!(ids(&task.tiers), vec![vec![9], vec![8], vec![7]]);
        assert!(task.bottom_tier_included);
    }

    // 减少段数被 max_merge_width 截短时，没有取到最底，不算包含最底。
    #[test]
    fn capped_tier_count_task_does_not_include_the_bottom() {
        let dir = tempfile::tempdir().unwrap();
        // 大小 [1, 1, 2, 4, 8]：只有第三种成立。
        let state = state_with(
            &dir,
            &[
                &[20],
                &[19],
                &[17, 18],
                &[13, 14, 15, 16],
                &[1, 2, 3, 4, 5, 6, 7, 8],
            ],
        );
        let options = TieredOptions {
            num_tiers: 4,
            max_merge_width: Some(2),
            ..options()
        };

        let task = generate_task(&options, &state).unwrap();

        assert_eq!(ids(&task.tiers), vec![vec![20], vec![19]]);
        assert!(!task.bottom_tier_included);
    }

    // tier 数不到 num_tiers：没有任务。
    #[test]
    fn no_task_below_num_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[&[9], &[8]]);

        assert!(generate_task(&options_with_num_tiers(3), &state).is_none());
    }
}
