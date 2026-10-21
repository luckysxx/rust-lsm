//! 简单分层（simple leveled）策略：每次挑一对相邻的层，把上层整层并进下层。
//!
//! 两种情况会触发：
//!
//! - L0 的文件数达到 `level0_file_num_compaction_trigger`：合并 L0 与 L1。
//! - L1 往下某一对相邻的层，下层的文件数不到上层的 `size_ratio_percent`%：
//!   下层相对上层"不够大"，把上层整层并进下层。
//!
//! 两种都满足时先做 L0；层与层之间从上往下找第一对。教程为了简单，比较的是文件数；
//! 真实的引擎比较字节数。层数不在这里设：它就是状态里 `levels` 的长度。

use crate::lsm_storage::LsmStorageState;
use crate::table::SsTable;
use std::sync::Arc;

/// 简单分层策略的参数。
#[derive(Debug, Clone)]
pub struct SimpleLeveledOptions {
    /// L0 的文件数达到这么多，就把 L0 与 L1 合并。至少为 1。
    pub level0_file_num_compaction_trigger: usize,
    /// 相邻两层的目标比例，百分数：下层的文件数应当至少是上层的这么多倍。
    /// 例如 200 表示下层至少是上层的 2 倍；不到就把上层并进下层。
    pub size_ratio_percent: usize,
}

/// 一次合并的任务：把上层的全部文件与下层的全部文件合并，结果放进下层，上层清空。
///
/// 文件列表是生成任务那一刻的状态里的：执行时只读这些文件，换状态时只拿掉这些文件。
/// 这期间 flush 可能往 L0 加了新文件，它们不在任务里。
pub struct SimpleLeveledTask {
    /// 上层的层号：0 是 L0，n 是 Ln。
    pub upper_level: usize,
    /// 上层参加合并的文件，就是上层当时的全部文件，顺序与状态里相同
    /// （L0 新的在前；其他层按 key 排好）。
    pub upper_ssts: Vec<Arc<SsTable>>,
    /// 下层的层号，总是 `upper_level + 1`。
    pub lower_level: usize,
    /// 下层参加合并的文件，就是下层当时的全部文件，按 key 排好。
    pub lower_ssts: Vec<Arc<SsTable>>,
    /// 下层是不是最底层。是的话下面没有更旧的版本，合并时可以丢掉墓碑。
    pub is_lower_level_bottom: bool,
}

/// 看一份状态，给出下一个要做的任务；没有要合并的就返回 `None`。
///
/// 任务做完、结果换进状态之后，应当再调用一次：一次合并常常让下面一对层也需要合并。
/// 反复调用总会停下：每个任务都把至少一个文件往下移一层，而最底层的文件不会再动。
pub fn generate_task(
    options: &SimpleLeveledOptions,
    state: &LsmStorageState,
) -> Option<SimpleLeveledTask> {
    assert!(
        options.level0_file_num_compaction_trigger >= 1,
        "L0 的触发值至少为 1，否则 L0 是空的也会不停地生成任务"
    );

    // 各层的文件数，下标就是层号：第 0 项是 L0，第 n 项是 Ln。
    let mut level_counts = vec![state.l0_sstables.len()];
    for level in &state.levels {
        level_counts.push(level.len());
    }

    let upper_level = pick_upper_level(options, &level_counts)?;
    let lower_level = upper_level + 1;
    Some(SimpleLeveledTask {
        upper_level,
        upper_ssts: ssts_of_level(state, upper_level).to_vec(),
        lower_level,
        lower_ssts: ssts_of_level(state, lower_level).to_vec(),
        is_lower_level_bottom: lower_level == state.levels.len(),
    })
}

/// 只看各层的文件数，决定把哪一层并进它的下一层，返回上层的层号；不需要合并时返回 `None`。
///
/// `level_counts` 的下标就是层号：第 0 项是 L0 的文件数，第 n 项是 Ln 的，
/// 最后一项是最底层。最底层没有下一层，不会被选中。
fn pick_upper_level(options: &SimpleLeveledOptions, level_counts: &[usize]) -> Option<usize> {
    // L0 优先：它的文件读的时候要逐个看。
    if level_counts[0] >= options.level0_file_num_compaction_trigger {
        return Some(0);
    }
    // 从上往下找第一对比例不够的相邻层。循环只到最底层的上一层：最底层没有下一层。
    // 下层 ÷ 上层 < 比例 ÷ 100，两边同乘 100 × 上层换成整数比较；
    // 上层是空的时候右边是 0，不会成立，空的层不会被选中。
    (1..level_counts.len() - 1).find(|&upper| {
        level_counts[upper + 1] * 100 < options.size_ratio_percent * level_counts[upper]
    })
}

/// 第 `level` 层的文件：0 是 L0，n 是 Ln（在 `state.levels[n - 1]`）。
fn ssts_of_level(state: &LsmStorageState, level: usize) -> &[Arc<SsTable>] {
    if level == 0 {
        &state.l0_sstables
    } else {
        &state.levels[level - 1]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_table::MemTable;
    use crate::table::SsTableBuilder;

    /// 触发值 2、比例 200%：L0 攒到 2 个文件就并进 L1；下层至少要是上层的 2 倍。
    fn options() -> SimpleLeveledOptions {
        SimpleLeveledOptions {
            level0_file_num_compaction_trigger: 2,
            size_ratio_percent: 200,
        }
    }

    // ---------- 只看文件数：pick_upper_level ----------

    // L0 不到触发值、各层都是空的：没事可做。
    #[test]
    fn nothing_to_do_below_the_l0_trigger() {
        assert_eq!(pick_upper_level(&options(), &[0, 0, 0, 0]), None);
        assert_eq!(pick_upper_level(&options(), &[1, 0, 0, 0]), None);
    }

    // L0 的文件数达到或超过触发值：合并 L0 与 L1。
    #[test]
    fn l0_reaching_the_trigger_is_compacted() {
        assert_eq!(pick_upper_level(&options(), &[2, 0, 0, 0]), Some(0));
        assert_eq!(pick_upper_level(&options(), &[3, 4, 8, 16]), Some(0));
    }

    // L0 达到触发值，同时 L1 与 L2 的比例也不够：先做 L0。
    #[test]
    fn l0_comes_before_the_ratio() {
        assert_eq!(pick_upper_level(&options(), &[2, 2, 0, 0]), Some(0));
    }

    // 教程的例子：L2 是 L1 的 150%，不到 200%，合并 L1 与 L2；L3 是 L2 的 266%，够了。
    #[test]
    fn lower_level_too_small_is_compacted() {
        assert_eq!(pick_upper_level(&options(), &[0, 2, 3, 8]), Some(1));
        // 下层是空的：0%，一定不够。
        assert_eq!(pick_upper_level(&options(), &[0, 2, 0, 0]), Some(1));
    }

    // 下层恰好是上层的 200%：已经达到目标，不合并。
    #[test]
    fn exactly_the_target_ratio_is_enough() {
        assert_eq!(pick_upper_level(&options(), &[0, 2, 4, 8]), None);
    }

    // 好几对层的比例都不够：从上往下，先做最上面的一对。
    #[test]
    fn the_topmost_pair_comes_first() {
        assert_eq!(pick_upper_level(&options(), &[0, 2, 2, 2]), Some(1));
    }

    // 上面的层是空的，比例不够的是下面一对：跳过空的层，选中 L2。
    #[test]
    fn a_deeper_pair_is_found() {
        assert_eq!(pick_upper_level(&options(), &[0, 0, 2, 2]), Some(2));
        assert_eq!(pick_upper_level(&options(), &[0, 0, 1, 0]), Some(2));
    }

    // 上层是空的：没有东西可以往下并，不管下层有多少都不合并。
    // 最底层没有下一层，文件再多也不会被选中。
    #[test]
    fn empty_upper_level_and_the_bottom_level_are_never_picked() {
        assert_eq!(pick_upper_level(&options(), &[0, 0, 0, 5]), None);
        assert_eq!(pick_upper_level(&options(), &[0, 0, 0, 1000]), None);
    }

    // 只有一层（L1 就是最底层）：只可能合并 L0 与 L1。
    #[test]
    fn a_single_level_only_takes_l0() {
        assert_eq!(pick_upper_level(&options(), &[2, 0]), Some(0));
        assert_eq!(pick_upper_level(&options(), &[1, 5]), None);
    }

    /// 按文件数模拟：每次 flush 往 L0 加一个文件，然后反复要任务、"执行"，直到没有任务。
    /// "执行"一个任务就是把上层的文件数加到下层、上层清零（教程的模拟器也这样算，
    /// 不考虑合并时去掉重复的 key）。返回每次 flush 之后各层的文件数。
    ///
    /// 每次 flush 之后要的任务超过 100 个就判定为停不下来。
    fn simulate(
        options: &SimpleLeveledOptions,
        max_levels: usize,
        flushes: usize,
    ) -> Vec<Vec<usize>> {
        let mut level_counts = vec![0; max_levels + 1];
        let mut history = Vec::new();
        for flush in 1..=flushes {
            level_counts[0] += 1;
            let mut tasks = 0;
            while let Some(upper) = pick_upper_level(options, &level_counts) {
                tasks += 1;
                assert!(
                    tasks <= 100,
                    "第 {flush} 次 flush 之后停不下来：{level_counts:?}"
                );
                println!("  合并 L{upper} 与 L{}：{level_counts:?}", upper + 1);
                level_counts[upper + 1] += level_counts[upper];
                level_counts[upper] = 0;
            }
            println!("第 {flush} 次 flush 之后：{level_counts:?}");
            history.push(level_counts.clone());
        }
        history
    }

    // 模拟 100 次 flush：每次都能停下；停下时 L0 不到触发值，每对相邻的层
    // 要么上层是空的，要么下层够大；文件一个不少。
    // 想看每一步怎样合并：cargo test -p lsm simulation -- --nocapture
    #[test]
    fn simulation_always_settles() {
        let options = options();
        let history = simulate(&options, 3, 100);

        for (index, level_counts) in history.iter().enumerate() {
            let flushes = index + 1;
            assert!(level_counts[0] < options.level0_file_num_compaction_trigger);
            for upper in 1..level_counts.len() - 1 {
                let upper_count = level_counts[upper];
                let lower_count = level_counts[upper + 1];
                assert!(
                    upper_count == 0
                        || lower_count * 100 >= upper_count * options.size_ratio_percent,
                    "第 {flushes} 次 flush 之后 L{upper} 与 L{} 不满足比例：{level_counts:?}",
                    upper + 1
                );
            }
            assert_eq!(level_counts.iter().sum::<usize>(), flushes);
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

    /// 一份只有 SST 的状态：`l0` 是 L0 各文件的编号（新的在前），`levels[i]` 是 L(i+1) 的。
    fn state_with(dir: &tempfile::TempDir, l0: &[usize], levels: &[&[usize]]) -> LsmStorageState {
        let to_ssts = |ids: &[usize]| -> Vec<Arc<SsTable>> {
            ids.iter().map(|id| tiny_sst(dir, *id)).collect()
        };
        LsmStorageState {
            memtable: Arc::new(MemTable::new(0)),
            imm_memtables: Vec::new(),
            l0_sstables: to_ssts(l0),
            levels: levels.iter().map(|ids| to_ssts(ids)).collect(),
        }
    }

    /// 一组 SST 的编号。
    fn ids(ssts: &[Arc<SsTable>]) -> Vec<usize> {
        ssts.iter().map(|sst| sst.id()).collect()
    }

    // L0 的任务：带上 L0 全部文件（新的在前，顺序不变）与整个 L1；L1 不是最底层。
    #[test]
    fn l0_task_takes_all_of_l0_and_l1() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[12, 11], &[&[5, 6], &[], &[1]]);

        let task = generate_task(&options(), &state).unwrap();

        assert_eq!(task.upper_level, 0);
        assert_eq!(ids(&task.upper_ssts), vec![12, 11]);
        assert_eq!(task.lower_level, 1);
        assert_eq!(ids(&task.lower_ssts), vec![5, 6]);
        assert!(!task.is_lower_level_bottom);
    }

    // 层与层之间的任务：带上两层各自的全部文件；下层是最后一层时标为最底层。
    #[test]
    fn level_task_takes_both_levels_and_knows_the_bottom() {
        let dir = tempfile::tempdir().unwrap();
        // L2 有 2 个，L3 有 1 个：不到 200%，合并 L2 与 L3。
        let state = state_with(&dir, &[], &[&[], &[7, 8], &[1]]);

        let task = generate_task(&options(), &state).unwrap();

        assert_eq!(task.upper_level, 2);
        assert_eq!(ids(&task.upper_ssts), vec![7, 8]);
        assert_eq!(task.lower_level, 3);
        assert_eq!(ids(&task.lower_ssts), vec![1]);
        assert!(task.is_lower_level_bottom);
    }

    // 只有一层时，L0 的任务直接合并到最底层。
    #[test]
    fn l0_task_with_a_single_level_reaches_the_bottom() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[12, 11], &[&[5]]);

        let task = generate_task(&options(), &state).unwrap();

        assert_eq!(task.lower_level, 1);
        assert!(task.is_lower_level_bottom);
    }

    // 没有要合并的：返回 None。
    #[test]
    fn no_task_when_the_shape_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with(&dir, &[11], &[&[5], &[6, 7], &[1, 2, 3, 4]]);

        assert!(generate_task(&options(), &state).is_none());
    }
}
