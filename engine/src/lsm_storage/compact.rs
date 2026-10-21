//! compaction 的执行：按任务把一组 SST 归并成新的 SST，换进状态，删除旧文件。
//!
//! 合并什么由 `crate::compaction` 里的策略决定，这里只负责照做。有两种入口：
//! 手动的全量合并 `force_full_compaction`，与按选项里的策略做一个任务的 `compact_once`。
//! 每种任务都是同样的四步：在锁外合并、写新文件 → 同步目录 → 拿 `state_lock`，把这次的
//! 变化追加进 manifest，再换状态 → 删除旧文件。旧文件要等新状态记进 manifest 之后才能删：
//! 在这之前崩溃，重启时 manifest 里还是旧文件。
//!
//! 本文件是 `lsm_storage` 的子模块。子模块能看到父模块里的私有内容，所以这里可以
//! 直接给 `LsmStorage` 加方法，并使用它的私有字段与 `write_ssts` 等私有方法，
//! 不必为了拆文件把它们改成公开的。

use std::collections::HashSet;
use std::sync::Arc;

use super::LsmStorage;
use crate::compaction::leveled::{self, LeveledTask};
use crate::compaction::simple_leveled::{self, SimpleLeveledTask};
use crate::compaction::tiered::{self, TieredTask};
use crate::compaction::{CompactionOptions, is_sorted_run, sorted_by_first_key};
use crate::error::Result;
use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::manifest::Placement;
use crate::table::{SsTable, SsTableIterator};

impl LsmStorage {
    /// 按选项里的策略做一个合并任务，然后删除旧文件。返回这次有没有做：
    /// 没有需要合并的、或者选项是 `NoCompaction` 时什么也不做，返回 `false`。
    ///
    /// 一次合并之后常常还有下一个任务，调用方应当反复调用，直到返回 `false`。
    ///
    /// 写新文件或追加 manifest 失败时返回错误，状态不变，写好或写了一半的新文件留在目录里，
    /// 没有任何地方引用，下次打开时删掉。删除旧文件失败时也返回错误，这时新状态已经生效、
    /// 记进了 manifest，旧文件留在目录里，同样在下次打开时删掉。
    pub fn compact_once(&self) -> Result<bool> {
        // 同一时刻只做一次 compaction：L1 往下的层（tier）只由 compaction 拿掉或替换，
        // 两次交错会互相覆盖。任务要在拿到锁之后生成，看到的才是上一次合并换完之后的状态。
        let _compaction_lock = self.compaction_lock.lock().unwrap();

        let snapshot = self.snapshot();
        match &self.options.compaction {
            CompactionOptions::NoCompaction => Ok(false),
            CompactionOptions::SimpleLeveled(options) => {
                let Some(task) = simple_leveled::generate_task(options, &snapshot) else {
                    return Ok(false);
                };
                self.run_simple_leveled_task(&task)?;
                Ok(true)
            }
            CompactionOptions::Tiered(options) => {
                let Some(task) = tiered::generate_task(options, &snapshot) else {
                    return Ok(false);
                };
                self.run_tiered_task(&task)?;
                Ok(true)
            }
            CompactionOptions::Leveled(options) => {
                let Some(task) = leveled::generate_task(options, &snapshot) else {
                    return Ok(false);
                };
                self.run_leveled_task(&task)?;
                Ok(true)
            }
        }
    }

    /// 按选项里的策略，现在的形状还有没有要做的合并任务。后台正在做的那个任务换进状态
    /// 之前也算。选项是 `NoCompaction` 时总是 `false`。
    pub fn has_pending_compaction(&self) -> bool {
        let snapshot = self.snapshot();
        match &self.options.compaction {
            CompactionOptions::NoCompaction => false,
            CompactionOptions::SimpleLeveled(options) => {
                simple_leveled::generate_task(options, &snapshot).is_some()
            }
            CompactionOptions::Tiered(options) => {
                tiered::generate_task(options, &snapshot).is_some()
            }
            CompactionOptions::Leveled(options) => {
                leveled::generate_task(options, &snapshot).is_some()
            }
        }
    }

    /// 把全部文件合并成一个 sorted run（full compaction），然后删除旧文件。
    ///
    /// 分层的形状（`NoCompaction`、简单分层与 leveled）：L0 与各层合并成新的最底层，其余各层清空。
    /// 合并期间 flush 照常进行，新 flush 出的 L0 文件不参加这次合并，换状态之后仍留在 L0。
    /// L0 是空的时候什么也不做。
    ///
    /// tiered：全部 tier 合并成一个。合并期间新 flush 出的 tier 不参加，换状态之后排在它前面。
    /// 不到 2 个 tier 时什么也不做。
    ///
    /// 旧文件在换状态之后立刻删除。仍拿着旧快照的读者照样能读完：它们的 `SsTable`
    /// 一直开着文件，Unix 上删除只去掉文件名，最后一个打开它的一方关闭之后，文件系统
    /// 才回收空间。
    ///
    /// 出错时的情况与 `compact_once` 相同。
    pub fn force_full_compaction(&self) -> Result<()> {
        // 同一时刻只做一次 compaction：L1 往下的层只由 compaction 改动，两次交错会互相覆盖。
        let _compaction_lock = self.compaction_lock.lock().unwrap();
        let snapshot = self.snapshot();

        // tiered：就是一个带上全部 tier 的任务。
        if !self.options.compaction.flush_to_l0() {
            if snapshot.levels.len() < 2 {
                return Ok(());
            }
            let task = TieredTask {
                tiers: snapshot.levels.clone(),
                bottom_tier_included: true,
            };
            return self.run_tiered_task(&task);
        }

        // 记下这次要合并的文件。之后 flush 可能往 L0 加新文件，它们不在这次的输入里。
        let l0_inputs = snapshot.l0_sstables.clone();
        let level_inputs = snapshot.levels.clone();
        if l0_inputs.is_empty() {
            return Ok(());
        }

        // 写新文件：慢，不占 `state_lock`，读、写、冻结、flush 都照常进行。
        let new_ssts = self.compact_full(&l0_inputs, &level_inputs)?;
        if !new_ssts.is_empty() {
            self.sync_dir()?;
        }

        // 换状态：一瞬间。
        self.apply_full_compaction_result(&l0_inputs, &level_inputs, new_ssts)?;

        // 新状态里已经没有旧文件了，删掉它们的文件名。
        let old_ssts = l0_inputs.iter().chain(level_inputs.iter().flatten());
        self.remove_sst_files(old_ssts)
    }

    /// 删除一组已经不在状态里的 SST 的文件名。
    fn remove_sst_files<'a>(&self, ssts: impl Iterator<Item = &'a Arc<SsTable>>) -> Result<()> {
        for sst in ssts {
            std::fs::remove_file(self.path_of_sst(sst.id()))?;
        }
        Ok(())
    }

    /// 把一次 full compaction 的结果换进状态：从 L0 拿掉参加合并的文件，
    /// 各层清空，最底层换成新文件。
    ///
    /// `l0_inputs`、`level_inputs` 是开始合并时记下的输入。这期间 flush 可能往 L0 的
    /// 前面加了新文件，它们比新的最底层新，必须原样留在 L0：所以按编号拿掉输入，
    /// 不能清空 L0，也不能用开始时的快照覆盖当前状态。L1 往下的层只由 compaction 改动，
    /// 而 compaction 同一时刻只有一个，所以各层此时应当还是 `level_inputs`。
    fn apply_full_compaction_result(
        &self,
        l0_inputs: &[Arc<SsTable>],
        level_inputs: &[Vec<Arc<SsTable>>],
        new_ssts: Vec<Arc<SsTable>>,
    ) -> Result<()> {
        // 与冻结、flush 一样：先拿 `state_lock` 排队，在当前状态的副本上改，记进 manifest，
        // 再换指针。
        let mut manifest = self.state_lock.lock().unwrap();
        let mut new_state = self.snapshot().as_ref().clone();
        let removed = l0_inputs.iter().chain(level_inputs.iter().flatten());
        let placement = Placement::Level(level_inputs.len());
        let edit = self.version_edit(removed, &new_ssts, placement);

        // L0：只拿掉参加了合并的文件，合并期间新 flush 出的留下。
        remove_compacted_ssts(&mut new_state.l0_sstables, l0_inputs, 0);

        // 各层：此时应当还是这次的输入，全部清空，最底层换成新文件。
        assert_eq!(new_state.levels.len(), level_inputs.len());
        for (level, inputs) in new_state.levels.iter_mut().zip(level_inputs) {
            assert!(
                same_ssts(level, inputs),
                "compaction 期间 L1 往下的层被别的操作改动了"
            );
            level.clear();
        }
        let bottom = new_state.levels.last_mut().unwrap();
        *bottom = new_ssts;
        self.commit_state(&mut manifest, new_state, edit)
    }

    /// 把 `l0` 与 `levels` 里的全部记录归并，写成新的 SST，返回它们（按 key 排好、互不重叠）。
    ///
    /// `l0` 新的在前；`levels` 从 L1 开始，每层按 key 排好。同一个 key 只留最新的版本。
    /// 输入是全部的文件、输出就是最底层，下面不会再有更旧的版本，所以最新版本是墓碑的
    /// key 整个不写。每个输出文件的大小由 `target_sst_size` 决定。
    ///
    /// 只写新文件：不改变状态，也不删除输入的文件，这两件事由调用方在之后完成。
    fn compact_full(
        &self,
        l0: &[Arc<SsTable>],
        levels: &[Vec<Arc<SsTable>>],
    ) -> Result<Vec<Arc<SsTable>>> {
        // L0 的文件范围互相重叠，用归并；列表里新的在前，归并时排在前面的胜出。
        let mut l0_iters = Vec::new();
        for sst in l0 {
            l0_iters.push(SsTableIterator::new_and_seek_to_first(Arc::clone(sst))?);
        }
        // 每层的文件按 key 排好、互不重叠，串接起来就有序；各层再归并，上层排在前面。
        let mut level_iters = Vec::new();
        for level in levels {
            level_iters.push(SstConcatIterator::create_and_seek_to_first(level.clone())?);
        }
        // L0 比各层新，放在两路归并的前一个位置。
        let mut iter = TwoMergeIterator::new(
            MergeIterator::new(l0_iters),
            MergeIterator::new(level_iters),
        )?;

        let ssts = self.write_ssts(&mut iter, self.options.target_sst_size, true)?;
        for sst in &ssts {
            self.stats.add_compaction_bytes_written(sst.file_size());
        }
        Ok(ssts)
    }
}

impl LsmStorage {
    /// 做一个简单分层的任务：合并两层、同步目录、换状态、删除旧文件。
    /// 调用方必须持有 `compaction_lock`，任务是拿到锁之后生成的。
    fn run_simple_leveled_task(&self, task: &SimpleLeveledTask) -> Result<()> {
        // 写新文件：慢，不占 `state_lock`，读、写、冻结、flush 都照常进行。
        let new_ssts = self.compact_simple_leveled_task(task)?;
        if !new_ssts.is_empty() {
            self.sync_dir()?;
        }

        // 换状态：一瞬间。
        self.apply_simple_leveled_result(task, new_ssts)?;

        // 新状态里已经没有这两层的旧文件了，删掉它们的文件名。
        let old_ssts = task.upper_ssts.iter().chain(task.lower_ssts.iter());
        self.remove_sst_files(old_ssts)
    }

    /// 把任务里上层与下层的全部记录归并，写成新的 SST，返回它们（按 key 排好、互不重叠）。
    /// 做法见 `merge_upper_into_lower`。
    fn compact_simple_leveled_task(&self, task: &SimpleLeveledTask) -> Result<Vec<Arc<SsTable>>> {
        self.merge_upper_into_lower(
            task.upper_level,
            &task.upper_ssts,
            &task.lower_ssts,
            task.is_lower_level_bottom,
        )
    }

    /// 把上层的一组文件与下层的一组文件归并，写成新的 SST，返回它们（按 key 排好、互不重叠）。
    /// 简单分层与 leveled 共用：两者都是"上层的一些文件并进下层"，只是挑哪些文件不同。
    ///
    /// `upper_level` 是上层的层号，只用来区分 L0（0）：L0 的文件范围互相重叠、新的在前；
    /// 其他层的文件按 key 排好、互不重叠。`lower_ssts` 是下层的文件，按 key 排好，可以是空的。
    /// 上层比下层新，同一个 key 留上层的版本。`drop_tombstones`（下层是最底层时）为真时，
    /// 最新版本是墓碑的 key 整个不写；为假时墓碑照写，更下面的层里可能还有这个 key 的旧值，
    /// 要靠它盖住。
    ///
    /// 只写新文件：不改变状态，也不删除输入的文件。
    fn merge_upper_into_lower(
        &self,
        upper_level: usize,
        upper_ssts: &[Arc<SsTable>],
        lower_ssts: &[Arc<SsTable>],
        drop_tombstones: bool,
    ) -> Result<Vec<Arc<SsTable>>> {
        let target_sst_size = self.options.target_sst_size;
        // 下层总是按 key 排好、互不重叠的一层，串接起来就有序。
        let lower_iter = SstConcatIterator::create_and_seek_to_first(lower_ssts.to_vec())?;

        // 上层是 L0 时文件范围互相重叠，要归并；是 Ln 时与下层一样串接。两种游标的类型
        // 不同，拼出的两路归并也是不同的类型，所以分两支各写一次文件。
        let ssts = if upper_level == 0 {
            // L0 的列表里新的在前，归并时排在前面的胜出。
            let mut l0_iters = Vec::new();
            for sst in upper_ssts {
                l0_iters.push(SsTableIterator::new_and_seek_to_first(Arc::clone(sst))?);
            }
            let mut iter = TwoMergeIterator::new(MergeIterator::new(l0_iters), lower_iter)?;
            self.write_ssts(&mut iter, target_sst_size, drop_tombstones)?
        } else {
            let upper_iter = SstConcatIterator::create_and_seek_to_first(upper_ssts.to_vec())?;
            // 上层更新，放在两路归并的前一个位置。
            let mut iter = TwoMergeIterator::new(upper_iter, lower_iter)?;
            self.write_ssts(&mut iter, target_sst_size, drop_tombstones)?
        };

        for sst in &ssts {
            self.stats.add_compaction_bytes_written(sst.file_size());
        }
        Ok(ssts)
    }

    /// 把一个任务的结果换进状态：上层拿掉参加合并的文件，下层换成新文件。
    ///
    /// 上层是 L0 时，合并期间 flush 可能往 L0 的前面加了新文件，它们比合并结果新，
    /// 必须原样留在 L0：按编号拿掉输入。上层是 Ln 时，它与下层一样只由 compaction 改动，
    /// 而 compaction 同一时刻只有一个，所以两层此时应当还是任务里记下的文件：
    /// 断言之后，上层清空，下层整层换成 `new_ssts`。其他层不动。
    fn apply_simple_leveled_result(
        &self,
        task: &SimpleLeveledTask,
        new_ssts: Vec<Arc<SsTable>>,
    ) -> Result<()> {
        // 与冻结、flush 一样：先拿 `state_lock` 排队，在当前状态的副本上改，记进 manifest，
        // 再换指针。
        let mut manifest = self.state_lock.lock().unwrap();
        let mut new_state = self.snapshot().as_ref().clone();
        let removed = task.upper_ssts.iter().chain(task.lower_ssts.iter());
        let placement = Placement::Level(task.lower_level);
        let edit = self.version_edit(removed, &new_ssts, placement);

        // 上层。
        if task.upper_level == 0 {
            // L0：只拿掉参加了合并的文件，合并期间新 flush 出的留下。
            remove_compacted_ssts(&mut new_state.l0_sstables, &task.upper_ssts, 0);
        } else {
            // Ln：整层都参加了合并，此时应当原样未动，清空。
            let upper = new_state.level_mut(task.upper_level);
            assert!(
                same_ssts(upper, &task.upper_ssts),
                "compaction 期间 L{} 被别的操作改动了",
                task.upper_level
            );
            upper.clear();
        }

        // 下层：此时应当原样未动，整层换成新文件。
        let lower = new_state.level_mut(task.lower_level);
        assert!(
            same_ssts(lower, &task.lower_ssts),
            "compaction 期间 L{} 被别的操作改动了",
            task.lower_level
        );
        *lower = new_ssts;

        self.commit_state(&mut manifest, new_state, edit)
    }
}

impl LsmStorage {
    /// 做一个 tiered 的任务：合并几个 tier、同步目录、换状态、删除旧文件。
    /// 调用方必须持有 `compaction_lock`，任务是拿到锁之后生成的。
    fn run_tiered_task(&self, task: &TieredTask) -> Result<()> {
        // 写新文件：慢，不占 `state_lock`，读、写、冻结、flush 都照常进行。
        let new_ssts = self.compact_tiered_task(task)?;
        if !new_ssts.is_empty() {
            self.sync_dir()?;
        }

        // 换状态：一瞬间。
        self.apply_tiered_result(task, new_ssts)?;

        // 新状态里已经没有这几个 tier 的旧文件了，删掉它们的文件名。
        let old_ssts = task.tiers.iter().flatten();
        self.remove_sst_files(old_ssts)
    }

    /// 把任务里几个 tier 的全部记录归并，写成新的 SST，返回它们（按 key 排好、互不重叠）。
    ///
    /// 每个 tier 按 key 排好、互不重叠，串接起来就有序；各个 tier 再归并，列表里新的在前，
    /// 归并时排在前面的胜出。包含最底的 tier 时，最新版本是墓碑的 key 整个不写；
    /// 不包含时墓碑照写，更旧的 tier 里可能还有这个 key 的值，要靠它盖住。
    ///
    /// 只写新文件：不改变状态，也不删除输入的文件。
    fn compact_tiered_task(&self, task: &TieredTask) -> Result<Vec<Arc<SsTable>>> {
        let mut tier_iters = Vec::new();
        for tier in &task.tiers {
            tier_iters.push(SstConcatIterator::create_and_seek_to_first(tier.clone())?);
        }
        let mut iter = MergeIterator::new(tier_iters);

        let target_sst_size = self.options.target_sst_size;
        let drop_tombstones = task.bottom_tier_included;
        let ssts = self.write_ssts(&mut iter, target_sst_size, drop_tombstones)?;
        for sst in &ssts {
            self.stats.add_compaction_bytes_written(sst.file_size());
        }
        Ok(ssts)
    }

    /// 把一个 tiered 任务的结果换进状态：参加合并的几个 tier 换成一个新的 tier，
    /// 放在它们原来的位置。合并结果是空的（全部被删除）时，那几个 tier 直接拿掉。
    ///
    /// 合并期间 flush 可能在最前面加了新的 tier，它们比合并结果新，必须原样排在前面：
    /// 所以先找到任务里第一个 tier 现在在哪，不能假定它还在最前面。tier 只由 compaction
    /// 拿掉或替换，而 compaction 同一时刻只有一个，所以任务里的 tier 此时应当原样、
    /// 连续地排在那里，断言检查这一点。
    fn apply_tiered_result(&self, task: &TieredTask, new_ssts: Vec<Arc<SsTable>>) -> Result<()> {
        // 与冻结、flush 一样：先拿 `state_lock` 排队，在当前状态的副本上改，记进 manifest，
        // 再换指针。重放时新 tier 放在第一个被去掉的文件所在的位置，也就是这里的 `start`。
        let mut manifest = self.state_lock.lock().unwrap();
        let mut new_state = self.snapshot().as_ref().clone();
        let edit = self.version_edit(task.tiers.iter().flatten(), &new_ssts, Placement::Tier);
        let tiers = &mut new_state.levels;

        // 任务里的 tier 现在从哪里开始：前面是合并期间新 flush 出的 tier。
        let first_input = &task.tiers[0];
        let start = tiers
            .iter()
            .position(|tier| same_ssts(tier, first_input))
            .expect("compaction 期间参加合并的 tier 不见了");
        let end = start + task.tiers.len();
        assert!(
            end <= tiers.len()
                && tiers[start..end]
                    .iter()
                    .zip(&task.tiers)
                    .all(|(current, input)| same_ssts(current, input)),
            "compaction 期间参加合并的 tier 被别的操作改动了"
        );
        assert_eq!(
            end == tiers.len(),
            task.bottom_tier_included,
            "任务记下的是否包含最底与现在的状态不符"
        );

        // 几个 tier 换成一个；没有输出时只拿掉。
        let replacement = if new_ssts.is_empty() {
            Vec::new()
        } else {
            vec![new_ssts]
        };
        tiers.splice(start..end, replacement);

        self.commit_state(&mut manifest, new_state, edit)
    }
}

impl LsmStorage {
    /// 做一个 leveled 的任务。调用方必须持有 `compaction_lock`，任务是拿到锁之后生成的。
    ///
    /// - trivial move（`task.is_trivial_move`）：不读写文件，直接换状态，放进下层的就是上层
    ///   那些文件，按首 key 排好（`sorted_by_first_key`）；也不删除任何文件，它们还在状态里。
    ///   不同步目录：目录里没有新文件。在这里现排、而不是让任务里直接存排好的列表，是为了
    ///   让 `task.upper_ssts` 只有一种顺序（与状态里相同，L0 新的在前；合并时归并靠它决定
    ///   谁更新），只有"挪进下层"这一步需要按 key 排。
    /// - 否则与简单分层相同的四步：`compact_leveled_task` 写新文件 → 有新文件时
    ///   `sync_dir` → `apply_leveled_result` 换状态 → `remove_sst_files` 删除上层与下层
    ///   参加合并的旧文件。
    fn run_leveled_task(&self, task: &LeveledTask) -> Result<()> {
        if task.is_trivial_move {
            return self.apply_leveled_result(task, sorted_by_first_key(&task.upper_ssts));
        }
        let new_ssts = self.compact_leveled_task(task)?;
        if !new_ssts.is_empty() {
            self.sync_dir()?;
        }

        self.apply_leveled_result(task, new_ssts)?;
        let old_ssts = task.upper_ssts.iter().chain(task.lower_ssts.iter());
        self.remove_sst_files(old_ssts)
    }

    /// 把任务里上层与下层的全部记录归并，写成新的 SST，返回它们（按 key 排好、互不重叠）。
    /// 做法见 `merge_upper_into_lower`。
    fn compact_leveled_task(&self, task: &LeveledTask) -> Result<Vec<Arc<SsTable>>> {
        self.merge_upper_into_lower(
            task.upper_level,
            &task.upper_ssts,
            &task.lower_ssts,
            task.is_lower_level_bottom,
        )
    }

    /// 把一个 leveled 任务的结果换进状态：上层与下层拿掉参加合并的文件，`new_ssts`
    /// 整块插进下层按 key 的正确位置。其他层不动。
    ///
    /// `new_ssts` 是要放进下层的文件，按 key 排好、互不重叠，可以是空的（全部被删除）：
    /// 合并时是新写出的文件；trivial move 时就是 `task.upper_ssts` 按首 key 排好。
    ///
    /// 步骤（与冻结、flush 一样，先拿 `state_lock` 排队，在当前状态的副本上改，记进 manifest，
    /// 再换指针；trivial move 记成"去掉这几个文件、把它们放进下层"）：
    ///
    /// 1. 上层按编号拿掉 `task.upper_ssts`，其余文件顺序不变。上层是 L0 时，合并期间 flush
    ///    可能在前面加了新文件，它们留下。拿掉的个数不对时 panic：
    ///    `"compaction 期间 L{n} 里参加合并的文件少了"`（n 是上层的层号，L0 时是 0）。
    /// 2. 下层按编号拿掉 `task.lower_ssts`，个数不对时同样 panic（n 是下层的层号）。
    /// 3. `new_ssts` 不是空的时，插到下层第一个首 key 不小于 `new_ssts` 首 key 的文件之前
    ///    （`partition_point`），用 `splice` 整块插入。留下的下层文件都在合并范围的左边或右边，
    ///    所以整块插入之后仍然有序。
    /// 4. 检查下层仍是一个 sorted run（`is_sorted_run`），不是时 panic：
    ///    `"换状态之后 L{n} 不再按 key 排好、互不重叠"`。策略漏挑了重叠的文件时会在这里发现。
    ///
    /// L1 往下的层只由 compaction 改动，而 compaction 同一时刻只有一个，所以第 1、2 步里
    /// 任务记下的文件此时应当都还在。
    fn apply_leveled_result(&self, task: &LeveledTask, new_ssts: Vec<Arc<SsTable>>) -> Result<()> {
        let mut manifest = self.state_lock.lock().unwrap();
        let mut new_state = self.snapshot().as_ref().clone();
        let removed = task.upper_ssts.iter().chain(task.lower_ssts.iter());
        let placement = Placement::Level(task.lower_level);
        let edit = self.version_edit(removed, &new_ssts, placement);

        // 上层：只拿掉参加了合并的文件。L0 里合并期间新 flush 出的、Ln 里没被挑中的留下。
        let upper = if task.upper_level == 0 {
            &mut new_state.l0_sstables
        } else {
            new_state.level_mut(task.upper_level)
        };
        remove_compacted_ssts(upper, &task.upper_ssts, task.upper_level);

        // 下层：拿掉与上层重叠、参加了合并的文件，再把新文件整块插进空出来的位置。
        let lower = new_state.level_mut(task.lower_level);
        remove_compacted_ssts(lower, &task.lower_ssts, task.lower_level);

        if !new_ssts.is_empty() {
            let first_new_key = new_ssts[0].first_key();
            let position = lower.partition_point(|sst| sst.first_key() < first_new_key);
            lower.splice(position..position, new_ssts);
            assert!(
                is_sorted_run(lower),
                "换状态之后 L{} 不再按 key 排好、互不重叠",
                task.lower_level
            );
        }
        self.commit_state(&mut manifest, new_state, edit)
    }
}

/// 两组 SST 是否正好是同一批对象，顺序也相同。
fn same_ssts(current: &[Arc<SsTable>], expected: &[Arc<SsTable>]) -> bool {
    current.len() == expected.len()
        && current
            .iter()
            .zip(expected)
            .all(|(current, expected)| Arc::ptr_eq(current, expected))
}

/// 从一层的文件里按编号拿掉参加了合并的 `inputs`，其余文件顺序不变。`level` 是层号
/// （0 是 L0），只用在 panic 的说明里。
///
/// 拿掉的个数与 `inputs` 不同时 panic（`"compaction 期间 L{n} 里参加合并的文件少了"`）：
/// 说明合并期间这些文件被别的操作拿掉或换掉了，继续换状态会覆盖别人的改动。
fn remove_compacted_ssts(files: &mut Vec<Arc<SsTable>>, inputs: &[Arc<SsTable>], level: usize) {
    let compacted_ids: HashSet<usize> = inputs.iter().map(|sst| sst.id()).collect();
    let count_before = files.len();
    files.retain(|sst| !compacted_ids.contains(&sst.id()));
    assert_eq!(
        count_before - files.len(),
        inputs.len(),
        "compaction 期间 L{level} 里参加合并的文件少了"
    );
}

#[cfg(test)]
mod tests {
    use super::super::LsmStorageOptions;
    use super::super::tests::{
        TestStorage, add_l0_sst, assert_manifest_matches_state, get_str, open_with_options,
        open_without_auto_freeze, scan_all, set_level_ssts, state_ids,
    };
    use super::*;
    use crate::compaction::leveled::LeveledOptions;
    use crate::compaction::simple_leveled::SimpleLeveledOptions;
    use crate::compaction::tiered::TieredOptions;
    use crate::iterators::StorageIterator;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use crate::test_util::{run_with_deadline, run_with_slow_deadline};
    use std::ops::Bound;

    // 测试里手工写的 SST 编号从 101 起，避开引擎自己分配的编号（从 1 开始）：
    // 块缓存按（编号，块的下标）存块，编号相同会把别的文件的块当成自己的。

    /// 一组按 key 排好、互不重叠的 SST 里的全部记录，墓碑原样给出（value 为空）。
    fn records_of(ssts: &[Arc<SsTable>]) -> Vec<(String, String)> {
        let mut iter = SstConcatIterator::create_and_seek_to_first(ssts.to_vec()).unwrap();
        collect_all(&mut iter)
    }

    /// 把当前状态里 L0 与各层的全部文件交给 `compact_full`，返回引擎和写出的文件。
    ///
    /// 放在另一个线程里、设了时限：写文件的循环漏了让游标前进时，测试报"卡住"而不是停在那里。
    fn compact_all(storage: TestStorage) -> (TestStorage, Vec<Arc<SsTable>>) {
        run_with_deadline("compact_full", move || {
            let snapshot = storage.snapshot();
            let output = storage
                .compact_full(&snapshot.l0_sstables, &snapshot.levels)
                .unwrap();
            (storage, output)
        })
    }

    /// 0 到 `count - 1` 号的 key，形如 `key-001`。
    fn numbered_keys(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("key-{i:03}")).collect()
    }

    // 同一个 key 在几个 L0 文件和 L1 里都有：只留最新的那个版本。
    #[test]
    fn keeps_only_the_newest_version_of_each_key() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(
            &storage,
            &dir,
            1,
            &[(101, &[("a", "1"), ("b", "old")]), (102, &[("d", "4")])],
        );
        add_l0_sst(&storage, &dir, 109, &[("b", "mid"), ("c", "3")]);
        add_l0_sst(&storage, &dir, 110, &[("b", "new")]);

        let (_storage, output) = compact_all(storage);

        assert_eq!(
            records_of(&output),
            expected(&[("a", "1"), ("b", "new"), ("c", "3"), ("d", "4")])
        );
    }

    // 各层都有文件：上层的版本胜出，L1 的墓碑盖住 L3 的值，输出里两者都没有。
    #[test]
    fn merges_every_level_with_upper_levels_winning() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(
            &storage,
            &dir,
            3,
            &[(101, &[("a", "L3"), ("b", "L3"), ("c", "L3")])],
        );
        set_level_ssts(&storage, &dir, 2, &[(102, &[("a", "L2"), ("d", "L2")])]);
        set_level_ssts(&storage, &dir, 1, &[(103, &[("a", "L1"), ("c", "")])]);
        add_l0_sst(&storage, &dir, 109, &[("d", "L0")]);

        let (_storage, output) = compact_all(storage);

        assert_eq!(
            records_of(&output),
            expected(&[("a", "L1"), ("b", "L3"), ("d", "L0")])
        );
    }

    // L0 里的墓碑盖住 L1 里的值，输出是最底层，墓碑和被删的值都不写进去。
    #[test]
    fn drops_deleted_keys_and_their_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(
            &storage,
            &dir,
            1,
            &[(101, &[("a", "1"), ("b", "2"), ("c", "3")])],
        );
        add_l0_sst(&storage, &dir, 109, &[("b", ""), ("x", "")]);

        let (_storage, output) = compact_all(storage);

        assert_eq!(records_of(&output), expected(&[("a", "1"), ("c", "3")]));
    }

    // 所有 key 都被删除了：一个文件都不写。
    #[test]
    fn writes_nothing_when_everything_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 1, &[(101, &[("a", "1"), ("b", "2")])]);
        add_l0_sst(&storage, &dir, 109, &[("a", ""), ("b", "")]);

        let (_storage, output) = compact_all(storage);

        assert!(output.is_empty());
    }

    // 只有 L0 也能合并；把结果整层放进 L1、清空 L0 之后，只有 L1 也能合并。
    #[test]
    fn works_with_only_l0_or_only_l1() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 109, &[("a", "1"), ("c", "3")]);
        add_l0_sst(&storage, &dir, 110, &[("b", "2")]);

        let (storage, from_l0) = compact_all(storage);
        assert_eq!(
            records_of(&from_l0),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );

        {
            let _state_lock = storage.state_lock.lock().unwrap();
            let mut guard = storage.state.write().unwrap();
            let mut new_state = guard.as_ref().clone();
            new_state.l0_sstables.clear();
            new_state.levels[0] = from_l0;
            *guard = Arc::new(new_state);
        }
        let (_storage, from_l1) = compact_all(storage);
        assert_eq!(
            records_of(&from_l1),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // 输出按目标大小切成多个文件：按 key 排好、互不重叠，合起来就是全部记录。
    #[test]
    fn splits_output_by_target_size() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        let keys = numbered_keys(200);
        let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), "value")).collect();
        add_l0_sst(&storage, &dir, 109, &pairs);

        let (_storage, output) = compact_all(storage);

        // 测试里的目标大小是 1024 字节，200 条记录写不进一个文件。
        assert!(output.len() > 1, "只写出了 {} 个文件", output.len());
        for pair in output.windows(2) {
            assert!(pair[0].last_key() < pair[1].first_key());
        }
        assert_eq!(records_of(&output), expected(&pairs));
    }

    // 只写新文件：状态不变，输入的文件还在。
    #[test]
    fn does_not_change_state_or_delete_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 1, &[(101, &[("a", "1")])]);
        add_l0_sst(&storage, &dir, 109, &[("b", "2")]);

        let (storage, _output) = compact_all(storage);

        assert_eq!(storage.l0_sstable_count(), 1);
        assert_eq!(storage.level_sstable_counts(), vec![1, 0, 0]);
        assert!(dir.path().join("00101.sst").exists());
        assert!(dir.path().join("00109.sst").exists());
    }

    // compaction 写出的字节计入计数器，等于输出文件的长度之和。
    #[test]
    fn counts_bytes_written_by_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        add_l0_sst(&storage, &dir, 109, &[("a", "1"), ("b", "2")]);

        let (storage, output) = compact_all(storage);

        let total: u64 = output.iter().map(|sst| sst.file_size()).sum();
        assert_eq!(storage.stats().compaction_bytes_written, total);
    }

    // 合并结果是一个可以直接放进一层的序列：串接游标能从中定位。
    #[test]
    fn output_can_be_read_as_a_sorted_run() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        let keys = numbered_keys(100);
        let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), "value")).collect();
        add_l0_sst(&storage, &dir, 109, &pairs);
        let (_storage, output) = compact_all(storage);

        let iter = SstConcatIterator::create_and_seek_to_key(output, b"key-050").unwrap();

        assert_eq!(iter.key(), b"key-050");
    }

    // ---------- 换状态与删除旧文件：force_full_compaction ----------

    /// 写入几条记录，冻结并 flush 成一个 L0 文件。空字符串的 value 表示删除。
    fn flush_records(storage: &LsmStorage, pairs: &[(&str, &str)]) {
        for (key, value) in pairs {
            if value.is_empty() {
                storage.delete(key.as_bytes()).unwrap();
            } else {
                storage.put(key.as_bytes(), value.as_bytes()).unwrap();
            }
        }
        storage.freeze_memtable().unwrap();
        storage.flush_all_imm_memtables().unwrap();
    }

    /// 在另一个线程里、设了时限地做一次 full compaction，返回引擎。
    fn full_compaction(storage: TestStorage) -> TestStorage {
        run_with_deadline("force_full_compaction", move || {
            storage.force_full_compaction().unwrap();
            storage
        })
    }

    /// 当前状态里最底层的 SST。
    fn bottom_level(storage: &LsmStorage) -> Vec<Arc<SsTable>> {
        storage.snapshot().levels.last().unwrap().clone()
    }

    /// 当前状态里各个 SST 的文件名，按名字排序。
    fn names_in_state(ssts: &[Arc<SsTable>]) -> Vec<String> {
        let mut names: Vec<String> = ssts
            .iter()
            .map(|sst| format!("{:05}.sst", sst.id()))
            .collect();
        names.sort();
        names
    }

    // 合并之后 L0 与上面几层是空的，全部数据在最底层；读到的与合并之前一样，
    // 被删除的 key 仍然不在。
    #[test]
    fn full_compaction_moves_everything_into_the_bottom_level() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1"), ("b", "2"), ("c", "3")]);
        flush_records(&storage, &[("b", "new"), ("c", ""), ("d", "4")]);
        let before = scan_all(&storage);

        let storage = full_compaction(storage);

        assert_eq!(storage.l0_sstable_count(), 0);
        let level_counts = storage.level_sstable_counts();
        assert_eq!(level_counts[..2], [0, 0]);
        assert!(level_counts[2] >= 1);
        assert_eq!(scan_all(&storage), before);
        assert_eq!(get_str(&storage, "b"), Some("new".to_string()));
        assert_eq!(get_str(&storage, "c"), None);
    }

    // 合并之后最底层里没有墓碑，也没有被覆盖的旧版本：每个 key 只剩一条。
    #[test]
    fn full_compaction_leaves_no_tombstones_or_old_versions() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1"), ("b", "2")]);
        flush_records(&storage, &[("a", "new"), ("b", "")]);

        let storage = full_compaction(storage);

        let bottom = bottom_level(&storage);
        assert_eq!(records_of(&bottom), expected(&[("a", "new")]));
    }

    // 旧文件从目录里删掉了：目录里剩下的正好是状态里最底层的文件。
    #[test]
    fn full_compaction_deletes_old_files() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        flush_records(&storage, &[("b", "2")]);
        assert_eq!(storage.sst_file_names().len(), 2);

        let storage = full_compaction(storage);

        let bottom = bottom_level(&storage);
        assert_eq!(storage.sst_file_names(), names_in_state(&bottom));
    }

    // L0 是空的：什么也不做，最底层原样不动。
    #[test]
    fn full_compaction_with_empty_l0_does_nothing() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        let storage = full_compaction(storage);
        let bottom_before = bottom_level(&storage);

        let storage = full_compaction(storage);

        let bottom_after = bottom_level(&storage);
        assert_eq!(
            names_in_state(&bottom_after),
            names_in_state(&bottom_before)
        );
    }

    // 合并两次：第二次把新写的、覆盖的、删除的都并进已有的最底层。
    #[test]
    fn second_full_compaction_merges_into_the_existing_bottom_level() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1"), ("b", "2"), ("c", "3")]);
        let storage = full_compaction(storage);
        flush_records(&storage, &[("a", ""), ("b", "new"), ("d", "4")]);

        let storage = full_compaction(storage);

        assert_eq!(storage.l0_sstable_count(), 0);
        assert_eq!(
            scan_all(&storage),
            expected(&[("b", "new"), ("c", "3"), ("d", "4")])
        );
        let bottom = bottom_level(&storage);
        assert_eq!(
            records_of(&bottom),
            expected(&[("b", "new"), ("c", "3"), ("d", "4")])
        );
    }

    // 合并写新文件的期间 flush 出的 L0 文件：不在这次的输入里，换状态之后仍留在 L0，
    // 而且它比新的最底层新，读到的是它的值。
    #[test]
    fn apply_keeps_l0_files_flushed_during_compaction() {
        // 整个过程设了时限：换状态时锁的用法出错（例如拿着写锁再去读状态）会卡住。
        run_with_deadline("apply_full_compaction_result", || {
            let storage = open_without_auto_freeze();
            flush_records(&storage, &[("a", "1"), ("b", "2")]);
            flush_records(&storage, &[("c", "3")]);

            // 记下输入、写新文件；这时还没有换状态。
            let snapshot = storage.snapshot();
            let l0_inputs = snapshot.l0_sstables.clone();
            let level_inputs = snapshot.levels.clone();
            let new_ssts = storage.compact_full(&l0_inputs, &level_inputs).unwrap();
            // 合并的期间，又 flush 出一个文件，放在 L0 的最前面。
            flush_records(&storage, &[("a", "newer")]);
            let flushed_during = Arc::clone(&storage.snapshot().l0_sstables[0]);

            storage
                .apply_full_compaction_result(&l0_inputs, &level_inputs, new_ssts.clone())
                .unwrap();

            let state = storage.snapshot();
            assert_eq!(state.l0_sstables.len(), 1);
            assert!(Arc::ptr_eq(&state.l0_sstables[0], &flushed_during));
            assert_eq!(
                names_in_state(state.levels.last().unwrap()),
                names_in_state(&new_ssts)
            );
            assert_eq!(get_str(&storage, "a"), Some("newer".to_string()));
            assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
        });
    }

    // 合并结果为空（全部被删除）：各层都变成空的，L0 里参加合并的文件也拿掉了。
    #[test]
    fn apply_with_empty_result_empties_every_level() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        let storage = full_compaction(storage);
        flush_records(&storage, &[("a", "")]);

        let storage = full_compaction(storage);

        assert_eq!(storage.l0_sstable_count(), 0);
        assert_eq!(storage.level_sstable_counts(), vec![0, 0, 0]);
        assert!(storage.sst_file_names().is_empty());
        assert_eq!(scan_all(&storage), expected(&[]));
    }

    // 合并开始之前建好的扫描游标，合并换了状态、删了旧文件之后照样能读完。
    // 块缓存关掉，保证读的确实是已经被删掉文件名的那些文件。
    #[test]
    fn open_scan_finishes_after_old_files_are_deleted() {
        let storage = open_with_options(LsmStorageOptions {
            memtable_size_limit: usize::MAX,
            block_size: 64,
            block_cache_size: 0,
            target_sst_size: 1024,
            max_levels: 3,
            compaction: CompactionOptions::NoCompaction,
            enable_wal: false,
        });
        let keys = numbered_keys(100);
        let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), "value")).collect();
        flush_records(&storage, &pairs[..50]);
        flush_records(&storage, &pairs[50..]);
        let mut scan = storage.scan(Bound::Unbounded, Bound::Unbounded).unwrap();
        scan.next().unwrap();

        let storage = full_compaction(storage);
        let old_files_left = storage
            .sst_file_names()
            .iter()
            .filter(|name| *name == "00001.sst" || *name == "00002.sst")
            .count();
        assert_eq!(old_files_left, 0, "旧文件应当已经被删掉");

        let rest = collect_all(&mut scan);
        assert_eq!(rest.len(), 99);
        assert_eq!(rest[0].0, "key-001");
        assert_eq!(rest[98].0, "key-099");
    }

    // ---------- 简单分层的任务：compact_simple_leveled_task、apply_simple_leveled_result、compact_once ----------

    /// 触发值 2、比例 200%。
    fn simple_options() -> SimpleLeveledOptions {
        SimpleLeveledOptions {
            level0_file_num_compaction_trigger: 2,
            size_ratio_percent: 200,
        }
    }

    /// 不自动冻结、按给定的策略合并的引擎，L0 之下 3 层（tiered 不用层数）。
    /// 这一层没有后台线程，合并由测试调用 `compact_once`。
    fn open_with_strategy(compaction: CompactionOptions) -> TestStorage {
        open_with_options(LsmStorageOptions {
            memtable_size_limit: usize::MAX,
            block_size: 64,
            block_cache_size: 1 << 20,
            target_sst_size: 1024,
            max_levels: 3,
            compaction,
            enable_wal: false,
        })
    }

    /// 按当前状态手工搭一个任务：把第 `upper_level` 层（0 是 L0）整层并进下一层。
    /// 不经过策略，想合并哪一对就合并哪一对。
    fn task_for(storage: &LsmStorage, upper_level: usize) -> SimpleLeveledTask {
        let snapshot = storage.snapshot();
        let lower_level = upper_level + 1;
        let upper_ssts = if upper_level == 0 {
            snapshot.l0_sstables.clone()
        } else {
            snapshot.levels[upper_level - 1].clone()
        };
        SimpleLeveledTask {
            upper_level,
            upper_ssts,
            lower_level,
            lower_ssts: snapshot.levels[lower_level - 1].clone(),
            is_lower_level_bottom: lower_level == snapshot.levels.len(),
        }
    }

    /// 在另一个线程里、设了时限地执行 `compact_simple_leveled_task`，返回引擎、任务和写出的文件。
    fn run_task(
        storage: TestStorage,
        upper_level: usize,
    ) -> (TestStorage, SimpleLeveledTask, Vec<Arc<SsTable>>) {
        run_with_deadline("compact_simple_leveled_task", move || {
            let task = task_for(&storage, upper_level);
            let output = storage.compact_simple_leveled_task(&task).unwrap();
            (storage, task, output)
        })
    }

    /// 在另一个线程里、设了时限地执行 `apply_simple_leveled_result`，返回引擎。
    /// 换状态时锁的用法出错（例如拿着写锁再去取快照）会卡住，这里报"卡住"。
    fn apply_with_deadline(
        storage: TestStorage,
        task: SimpleLeveledTask,
        output: Vec<Arc<SsTable>>,
    ) -> TestStorage {
        run_with_deadline("apply_simple_leveled_result", move || {
            storage.apply_simple_leveled_result(&task, output).unwrap();
            storage
        })
    }

    /// 当前状态里 L0 与各层全部 SST 的文件名，按名字排序。
    fn names_of_whole_state(storage: &LsmStorage) -> Vec<String> {
        let snapshot = storage.snapshot();
        let mut all = snapshot.l0_sstables.clone();
        for level in &snapshot.levels {
            all.extend(level.iter().cloned());
        }
        names_in_state(&all)
    }

    // L0 并进 L1：同一个 key 留最新的版本；L1 不是最底层，墓碑照写。
    #[test]
    fn task_from_l0_keeps_the_newest_and_keeps_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(
            &storage,
            &dir,
            1,
            &[(101, &[("a", "1"), ("b", "old"), ("c", "3")])],
        );
        add_l0_sst(&storage, &dir, 109, &[("b", "mid"), ("d", "4")]);
        add_l0_sst(&storage, &dir, 110, &[("b", "new"), ("c", "")]);

        let (_storage, _task, output) = run_task(storage, 0);

        assert_eq!(
            records_of(&output),
            expected(&[("a", "1"), ("b", "new"), ("c", ""), ("d", "4")])
        );
    }

    // Ln 并进中间的一层：上层的版本胜出；下层不是最底层，墓碑照写。
    #[test]
    fn task_into_a_middle_level_keeps_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 1, &[(101, &[("a", "L1"), ("b", "")])]);
        set_level_ssts(&storage, &dir, 2, &[(102, &[("a", "L2"), ("b", "L2")])]);

        let (_storage, _task, output) = run_task(storage, 1);

        assert_eq!(records_of(&output), expected(&[("a", "L1"), ("b", "")]));
    }

    // 并进最底层：墓碑和被它盖住的值都不写。
    #[test]
    fn task_into_the_bottom_level_drops_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 2, &[(101, &[("a", "L2"), ("b", "")])]);
        set_level_ssts(
            &storage,
            &dir,
            3,
            &[(102, &[("a", "L3"), ("b", "L3"), ("c", "L3")])],
        );

        let (_storage, _task, output) = run_task(storage, 2);

        assert_eq!(records_of(&output), expected(&[("a", "L2"), ("c", "L3")]));
    }

    // 下层是空的：输出就是上层的全部记录。
    #[test]
    fn task_into_an_empty_level_copies_the_upper_level() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(
            &storage,
            &dir,
            1,
            &[(101, &[("a", "1"), ("b", "2")]), (102, &[("c", "3")])],
        );

        let (_storage, _task, output) = run_task(storage, 1);

        assert_eq!(
            records_of(&output),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // L0 的任务换状态：合并期间 flush 出的 L0 文件留下，参加合并的拿掉；
    // L1 换成新文件；读到的是更新的那个 L0 文件里的值。
    #[test]
    fn apply_l0_task_keeps_files_flushed_during_compaction() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1"), ("b", "2")]);
        flush_records(&storage, &[("c", "3")]);
        let (storage, task, output) = run_task(storage, 0);
        // 合并的期间，又 flush 出一个文件，放在 L0 的最前面。
        flush_records(&storage, &[("a", "newer")]);
        let flushed_during = Arc::clone(&storage.snapshot().l0_sstables[0]);

        let storage = apply_with_deadline(storage, task, output.clone());

        let state = storage.snapshot();
        assert_eq!(state.l0_sstables.len(), 1);
        assert!(Arc::ptr_eq(&state.l0_sstables[0], &flushed_during));
        assert_eq!(names_in_state(&state.levels[0]), names_in_state(&output));
        assert!(state.levels[1].is_empty());
        assert!(state.levels[2].is_empty());
        assert_eq!(get_str(&storage, "a"), Some("newer".to_string()));
        assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
    }

    // 层与层之间的任务换状态：上层清空，下层换成新文件；L0 与其他层原样不动。
    #[test]
    fn apply_level_task_changes_only_the_two_levels() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 1, &[(101, &[("a", "L1")])]);
        set_level_ssts(&storage, &dir, 2, &[(102, &[("b", "L2")])]);
        set_level_ssts(&storage, &dir, 3, &[(103, &[("c", "L3")])]);
        add_l0_sst(&storage, &dir, 109, &[("d", "L0")]);
        let before = storage.snapshot();
        let (storage, task, output) = run_task(storage, 1);

        let storage = apply_with_deadline(storage, task, output.clone());

        let state = storage.snapshot();
        assert!(same_ssts(&state.l0_sstables, &before.l0_sstables));
        assert!(state.levels[0].is_empty());
        assert_eq!(names_in_state(&state.levels[1]), names_in_state(&output));
        assert!(same_ssts(&state.levels[2], &before.levels[2]));
        assert_eq!(
            scan_all(&storage),
            expected(&[("a", "L1"), ("b", "L2"), ("c", "L3"), ("d", "L0")])
        );
    }

    // 合并结果是空的（全部被删除）：上层与下层都变成空的。
    #[test]
    fn apply_with_empty_output_empties_both_levels() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 2, &[(101, &[("a", "")])]);
        set_level_ssts(&storage, &dir, 3, &[(102, &[("a", "1")])]);
        let (storage, task, output) = run_task(storage, 2);
        assert!(output.is_empty());

        let storage = apply_with_deadline(storage, task, output);

        assert_eq!(storage.level_sstable_counts(), vec![0, 0, 0]);
        assert_eq!(get_str(&storage, "a"), None);
    }

    // 合并期间下层被换掉了：换状态时断言失败，而不是把别人的改动覆盖掉。
    #[test]
    #[should_panic(expected = "L2 被别的操作改动了")]
    fn apply_panics_if_the_lower_level_changed() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 1, &[(101, &[("a", "L1")])]);
        set_level_ssts(&storage, &dir, 2, &[(102, &[("b", "L2")])]);
        let (storage, task, output) = run_task(storage, 1);
        set_level_ssts(&storage, &dir, 2, &[(103, &[("c", "changed")])]);

        apply_with_deadline(storage, task, output);
    }

    // 合并期间上层（不是 L0）被改动了：同样断言失败。
    #[test]
    #[should_panic(expected = "L1 被别的操作改动了")]
    fn apply_panics_if_the_upper_level_changed() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_level_ssts(&storage, &dir, 1, &[(101, &[("a", "L1")])]);
        set_level_ssts(&storage, &dir, 2, &[(102, &[("b", "L2")])]);
        let (storage, task, output) = run_task(storage, 1);
        set_level_ssts(&storage, &dir, 1, &[(103, &[("c", "changed")])]);

        apply_with_deadline(storage, task, output);
    }

    // 合并期间 L0 里参加合并的文件不见了：断言失败。
    #[test]
    #[should_panic(expected = "L0 里参加合并的文件少了")]
    fn apply_panics_if_l0_inputs_are_missing() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        flush_records(&storage, &[("b", "2")]);
        let (storage, task, output) = run_task(storage, 0);
        {
            let _state_lock = storage.state_lock.lock().unwrap();
            let mut guard = storage.state.write().unwrap();
            let mut new_state = guard.as_ref().clone();
            new_state.l0_sstables.pop();
            *guard = Arc::new(new_state);
        }

        apply_with_deadline(storage, task, output);
    }

    // 没有需要合并的：返回 false，状态与文件都不变。
    #[test]
    fn compact_once_does_nothing_when_no_task_is_needed() {
        let storage = open_with_strategy(CompactionOptions::SimpleLeveled(simple_options()));
        flush_records(&storage, &[("a", "1")]);
        let names_before = names_of_whole_state(&storage);

        let storage = run_with_deadline("compact_once", move || {
            assert!(!storage.compact_once().unwrap());
            storage
        });

        assert_eq!(names_of_whole_state(&storage), names_before);
        assert_eq!(storage.sst_file_names(), names_before);
    }

    // L0 攒够 2 个文件：做一个任务（L0 并进 L1），旧文件从目录里删掉，数据照常读得到。
    #[test]
    fn compact_once_runs_one_task_and_deletes_its_inputs() {
        let storage = open_with_strategy(CompactionOptions::SimpleLeveled(simple_options()));
        flush_records(&storage, &[("a", "1"), ("b", "2")]);
        flush_records(&storage, &[("a", "new"), ("c", "3")]);
        let before = scan_all(&storage);

        let storage = run_with_deadline("compact_once", move || {
            assert!(storage.compact_once().unwrap());
            storage
        });

        assert_eq!(storage.l0_sstable_count(), 0);
        assert!(storage.level_sstable_counts()[0] >= 1);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));
        assert_eq!(scan_all(&storage), before);
    }

    /// 反复写入、flush、合并到停下，共 `rounds` 轮：每轮随机写入 20 条（约四分之一是删除）
    /// 并 flush 成一个文件，然后反复调用 `compact_once` 直到没有任务。每一轮之后检查读到的
    /// 与一个 BTreeMap 模型相同、目录里的文件正好是状态里的文件、manifest 重放出的文件列表
    /// 与状态相同、没有要做的任务，再调用 `check` 做各策略自己的检查（参数是引擎与轮次）。
    fn random_rounds(storage: &TestStorage, rounds: usize, check: impl Fn(&TestStorage, usize)) {
        let mut model = std::collections::BTreeMap::new();
        // 固定种子的伪随机数，每次运行都一样。
        let mut seed: u64 = 7;
        let mut next_random = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed >> 33
        };

        for round in 0..rounds {
            let mut pairs = Vec::new();
            for _ in 0..20 {
                let key = format!("key-{:03}", next_random() % 80);
                // 大约四分之一是删除。
                let value = if next_random() % 4 == 0 {
                    String::new()
                } else {
                    format!("value-{round}")
                };
                pairs.push((key, value));
            }
            let borrowed: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            flush_records(storage, &borrowed);
            for (key, value) in &pairs {
                if value.is_empty() {
                    model.remove(key);
                } else {
                    model.insert(key.clone(), value.clone());
                }
            }

            let mut tasks = 0;
            while storage.compact_once().unwrap() {
                tasks += 1;
                assert!(tasks <= 100, "第 {round} 轮停不下来");
            }

            let expected_pairs: Vec<(String, String)> = model
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            assert_eq!(scan_all(storage), expected_pairs, "第 {round} 轮");
            for (key, value) in &model {
                assert_eq!(get_str(storage, key).as_ref(), Some(value), "第 {round} 轮");
            }
            assert_eq!(
                storage.sst_file_names(),
                names_of_whole_state(storage),
                "第 {round} 轮"
            );
            assert_manifest_matches_state(storage);
            assert!(!storage.has_pending_compaction(), "第 {round} 轮");
            check(storage, round);
        }
    }

    // 简单分层：反复写入、flush、合并，每一轮读到的都与模型相同，目录里的文件正好是状态里的，
    // 形状满足策略；最底层里没有墓碑。
    #[test]
    fn repeated_compact_once_keeps_data_and_settles() {
        run_with_slow_deadline("反复 compact_once", || {
            let storage = open_with_strategy(CompactionOptions::SimpleLeveled(simple_options()));
            random_rounds(&storage, 30, |storage, round| {
                let bottom = bottom_level(storage);
                let tombstones = records_of(&bottom)
                    .iter()
                    .filter(|(_key, value)| value.is_empty())
                    .count();
                assert_eq!(tombstones, 0, "第 {round} 轮，最底层里有墓碑");
            });
        });
    }

    // 选项是 NoCompaction：compact_once 什么也不做，也不算有待做的合并。
    #[test]
    fn compact_once_without_a_strategy_does_nothing() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        flush_records(&storage, &[("b", "2")]);

        let storage = run_with_deadline("compact_once", move || {
            assert!(!storage.compact_once().unwrap());
            storage
        });

        assert_eq!(storage.l0_sstable_count(), 2);
        assert!(!storage.has_pending_compaction());
    }

    // ---------- tiered：flush 成为新的 tier、compact_tiered_task、apply_tiered_result ----------

    /// tiered 的参数：`num_tiers` 个 tier 开始检查，其余是 RocksDB 的默认值。
    fn tiered_options(num_tiers: usize) -> TieredOptions {
        TieredOptions {
            num_tiers,
            max_size_amplification_percent: 200,
            size_ratio: 1,
            min_merge_width: 2,
            max_merge_width: None,
        }
    }

    /// 选用 tiered 策略的引擎，`num_tiers` 给得很大时测试里不会自动生成任务。
    fn open_tiered(num_tiers: usize) -> TestStorage {
        open_with_strategy(CompactionOptions::Tiered(tiered_options(num_tiers)))
    }

    /// 各个 tier 里文件的编号，新的 tier 在前。
    fn tier_ids(storage: &LsmStorage) -> Vec<Vec<usize>> {
        storage
            .snapshot()
            .levels
            .iter()
            .map(|tier| tier.iter().map(|sst| sst.id()).collect())
            .collect()
    }

    /// 按当前状态手工搭一个任务：合并最新的 `count` 个 tier。不经过策略。
    fn tiered_task_for(storage: &LsmStorage, count: usize) -> TieredTask {
        let snapshot = storage.snapshot();
        TieredTask {
            tiers: snapshot.levels[..count].to_vec(),
            bottom_tier_included: count == snapshot.levels.len(),
        }
    }

    /// 在另一个线程里、设了时限地执行 `compact_tiered_task`，返回引擎、任务和写出的文件。
    fn run_tiered_task_for(
        storage: TestStorage,
        count: usize,
    ) -> (TestStorage, TieredTask, Vec<Arc<SsTable>>) {
        run_with_deadline("compact_tiered_task", move || {
            let task = tiered_task_for(&storage, count);
            let output = storage.compact_tiered_task(&task).unwrap();
            (storage, task, output)
        })
    }

    /// 在另一个线程里、设了时限地执行 `apply_tiered_result`，返回引擎。
    fn apply_tiered_with_deadline(
        storage: TestStorage,
        task: TieredTask,
        output: Vec<Arc<SsTable>>,
    ) -> TestStorage {
        run_with_deadline("apply_tiered_result", move || {
            storage.apply_tiered_result(&task, output).unwrap();
            storage
        })
    }

    /// 三个 tier，从旧到新依次 flush：`[a=1, b=1, c=1]`、`[b=2]`、`[a=3, c=删除]`。
    fn three_tiers() -> TestStorage {
        let storage = open_tiered(100);
        flush_records(&storage, &[("a", "1"), ("b", "1"), ("c", "1")]);
        flush_records(&storage, &[("b", "2")]);
        flush_records(&storage, &[("a", "3"), ("c", "")]);
        storage
    }

    // tiered 不用 L0：每次 flush 出的文件自己成为一个 tier，放在最前面；读到的是最新的版本。
    #[test]
    fn tiered_flush_adds_a_new_tier_in_front() {
        let storage = three_tiers();

        assert_eq!(storage.l0_sstable_count(), 0);
        let tiers = tier_ids(&storage);
        assert_eq!(tiers.len(), 3);
        assert!(tiers.iter().all(|tier| tier.len() == 1));
        // 编号随 flush 递增：最前面的 tier 编号最大。
        assert!(tiers[0][0] > tiers[1][0] && tiers[1][0] > tiers[2][0]);
        assert_eq!(get_str(&storage, "a"), Some("3".to_string()));
        assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
        assert_eq!(get_str(&storage, "c"), None);
        assert_eq!(scan_all(&storage), expected(&[("a", "3"), ("b", "2")]));
    }

    // 刚打开时一个 tier 都没有，不像分层那样先有几个空层。
    #[test]
    fn tiered_starts_without_any_tier() {
        let storage = open_tiered(100);

        assert!(storage.snapshot().levels.is_empty());
        assert_eq!(scan_all(&storage), expected(&[]));
    }

    // 合并最新的 2 个 tier：新的版本胜出；没有包含最底，墓碑照写。
    #[test]
    fn tiered_task_without_the_bottom_keeps_tombstones() {
        let (_storage, task, output) = run_tiered_task_for(three_tiers(), 2);

        assert!(!task.bottom_tier_included);
        assert_eq!(
            records_of(&output),
            expected(&[("a", "3"), ("b", "2"), ("c", "")])
        );
    }

    // 合并全部 tier：包含最底，墓碑与被它盖住的值都不写。
    #[test]
    fn tiered_task_with_the_bottom_drops_tombstones() {
        let (_storage, task, output) = run_tiered_task_for(three_tiers(), 3);

        assert!(task.bottom_tier_included);
        assert_eq!(records_of(&output), expected(&[("a", "3"), ("b", "2")]));
    }

    // 换状态：合并期间新 flush 出的 tier 留在最前面，合并结果放在输入原来的位置，
    // 更旧的 tier 不动；读到的仍是最新的版本。
    #[test]
    fn apply_tiered_keeps_tiers_flushed_during_compaction_in_front() {
        let (storage, task, output) = run_tiered_task_for(three_tiers(), 2);
        // 合并的期间，又 flush 出一个 tier，放在最前面。
        flush_records(&storage, &[("b", "newer")]);
        let before = tier_ids(&storage);
        let newest = before[0].clone();
        let oldest = before[before.len() - 1].clone();

        let storage = apply_tiered_with_deadline(storage, task, output.clone());

        let output_ids: Vec<usize> = output.iter().map(|sst| sst.id()).collect();
        assert_eq!(tier_ids(&storage), vec![newest, output_ids, oldest]);
        assert_eq!(get_str(&storage, "a"), Some("3".to_string()));
        assert_eq!(get_str(&storage, "b"), Some("newer".to_string()));
        assert_eq!(get_str(&storage, "c"), None);
    }

    // 合并结果是空的（全部被删除）：那几个 tier 直接拿掉，不留下空的 tier。
    #[test]
    fn apply_tiered_with_empty_output_removes_the_tiers() {
        let storage = open_tiered(100);
        flush_records(&storage, &[("a", "1")]);
        flush_records(&storage, &[("a", "")]);
        let (storage, task, output) = run_tiered_task_for(storage, 2);
        assert!(output.is_empty());

        let storage = apply_tiered_with_deadline(storage, task, output);

        assert!(storage.snapshot().levels.is_empty());
        assert_eq!(get_str(&storage, "a"), None);
    }

    // 合并期间参加合并的 tier 被换掉了：换状态时断言失败，而不是把别人的改动覆盖掉。
    #[test]
    #[should_panic(expected = "参加合并的 tier 被别的操作改动了")]
    fn apply_tiered_panics_if_an_input_tier_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (storage, task, output) = run_tiered_task_for(three_tiers(), 2);
        let replacement = {
            let other = open_without_auto_freeze();
            set_level_ssts(&other, &dir, 1, &[(101, &[("x", "1")])]);
            other.snapshot().levels[0].clone()
        };
        {
            let _state_lock = storage.state_lock.lock().unwrap();
            let mut guard = storage.state.write().unwrap();
            let mut new_state = guard.as_ref().clone();
            new_state.levels[1] = replacement;
            *guard = Arc::new(new_state);
        }

        apply_tiered_with_deadline(storage, task, output);
    }

    // 合并期间第一个参加合并的 tier 不见了：断言失败。
    #[test]
    #[should_panic(expected = "参加合并的 tier 不见了")]
    fn apply_tiered_panics_if_the_first_input_tier_is_gone() {
        let (storage, task, output) = run_tiered_task_for(three_tiers(), 2);
        {
            let _state_lock = storage.state_lock.lock().unwrap();
            let mut guard = storage.state.write().unwrap();
            let mut new_state = guard.as_ref().clone();
            new_state.levels.remove(0);
            *guard = Arc::new(new_state);
        }

        apply_tiered_with_deadline(storage, task, output);
    }

    // compact_once 按 tiered 策略做一个任务：3 个 tier、num_tiers 为 3，空间放大成立，
    // 全部合并成一个 tier；旧文件从目录里删掉。
    #[test]
    fn tiered_compact_once_runs_one_task() {
        let storage = open_tiered(3);
        flush_records(&storage, &[("a", "1"), ("b", "1"), ("c", "1")]);
        flush_records(&storage, &[("b", "2")]);
        flush_records(&storage, &[("a", "3"), ("c", "")]);
        assert!(storage.has_pending_compaction());

        let storage = run_with_deadline("compact_once", move || {
            assert!(storage.compact_once().unwrap());
            storage
        });

        assert_eq!(storage.level_sstable_counts().len(), 1);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));
        assert_eq!(scan_all(&storage), expected(&[("a", "3"), ("b", "2")]));
        assert!(!storage.compact_once().unwrap());
    }

    // tiered：反复写入、flush、合并，每一轮读到的都与模型相同，目录里的文件正好是状态里的；
    // L0 一直是空的，停下时 tier 数不到 num_tiers，没有空的 tier。
    #[test]
    fn repeated_tiered_compact_once_keeps_data_and_settles() {
        run_with_slow_deadline("反复 tiered compact_once", || {
            let num_tiers = 4;
            let storage = open_tiered(num_tiers);
            random_rounds(&storage, 60, |storage, round| {
                let snapshot = storage.snapshot();
                assert!(snapshot.l0_sstables.is_empty(), "第 {round} 轮");
                assert!(snapshot.levels.len() < num_tiers, "第 {round} 轮");
                assert!(
                    snapshot.levels.iter().all(|tier| !tier.is_empty()),
                    "第 {round} 轮出现了空的 tier"
                );
            });
        });
    }

    // tiered 的全量合并：全部 tier 合成一个，没有墓碑与旧版本，旧文件删掉。
    #[test]
    fn tiered_full_compaction_merges_every_tier_into_one() {
        let storage = full_compaction(three_tiers());

        assert_eq!(storage.level_sstable_counts().len(), 1);
        let only_tier = storage.snapshot().levels[0].clone();
        assert_eq!(records_of(&only_tier), expected(&[("a", "3"), ("b", "2")]));
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));
    }

    // tiered 只有一个 tier 时，全量合并什么也不做。
    #[test]
    fn tiered_full_compaction_with_one_tier_does_nothing() {
        let storage = open_tiered(100);
        flush_records(&storage, &[("a", "1"), ("b", "")]);
        let before = tier_ids(&storage);

        let storage = full_compaction(storage);

        assert_eq!(before.len(), 1);
        assert_eq!(tier_ids(&storage), before);
    }

    // ---------- leveled：apply_leveled_result、run_leveled_task、compact_once ----------

    /// leveled 的参数：触发值 2、比例 10；基准大小 1 MB，测试里的数据远远不到，
    /// 基准层一直是最底层，L0 总是直接并进最底层。
    fn leveled_options() -> LeveledOptions {
        LeveledOptions {
            level0_file_num_compaction_trigger: 2,
            level_size_multiplier: 10,
            base_level_size_bytes: 1 << 20,
        }
    }

    /// 选用 leveled 策略的引擎，L0 之下 3 层，L3 是最底层。
    fn open_leveled(options: LeveledOptions) -> TestStorage {
        open_with_strategy(CompactionOptions::Leveled(options))
    }

    /// 一组 SST 的编号，顺序不变。
    fn level_ids(ssts: &[Arc<SsTable>]) -> Vec<usize> {
        ssts.iter().map(|sst| sst.id()).collect()
    }

    /// 按编号从当前状态里取文件，手工搭一个任务，不经过策略。`upper`、`lower` 是
    /// (层号, 文件编号)，层号 0 是 L0；文件按它们在状态里的顺序放进任务。
    fn leveled_task(
        storage: &LsmStorage,
        upper: (usize, &[usize]),
        lower: (usize, &[usize]),
    ) -> LeveledTask {
        let snapshot = storage.snapshot();
        let pick = |level: usize, ids: &[usize]| -> Vec<Arc<SsTable>> {
            let files = if level == 0 {
                &snapshot.l0_sstables
            } else {
                &snapshot.levels[level - 1]
            };
            let picked: Vec<Arc<SsTable>> = files
                .iter()
                .filter(|sst| ids.contains(&sst.id()))
                .cloned()
                .collect();
            assert_eq!(
                picked.len(),
                ids.len(),
                "L{level} 里没有全部这些文件：{ids:?}"
            );
            picked
        };
        let upper_ssts = pick(upper.0, upper.1);
        let lower_ssts = pick(lower.0, lower.1);
        let is_trivial_move = upper_ssts.len() == 1 && lower_ssts.is_empty();
        LeveledTask {
            upper_level: upper.0,
            upper_ssts,
            lower_level: lower.0,
            lower_ssts,
            is_lower_level_bottom: lower.0 == snapshot.levels.len(),
            is_trivial_move,
        }
    }

    /// 在另一个线程里、设了时限地写出任务的结果，返回引擎、任务和要放进下层的文件。
    /// trivial move 不写文件，放进下层的就是上层的文件按首 key 排好，与 `run_leveled_task` 相同。
    fn merge_leveled(
        storage: TestStorage,
        task: LeveledTask,
    ) -> (TestStorage, LeveledTask, Vec<Arc<SsTable>>) {
        run_with_deadline("merge_upper_into_lower", move || {
            let output = if task.is_trivial_move {
                sorted_by_first_key(&task.upper_ssts)
            } else {
                storage
                    .merge_upper_into_lower(
                        task.upper_level,
                        &task.upper_ssts,
                        &task.lower_ssts,
                        task.is_lower_level_bottom,
                    )
                    .unwrap()
            };
            (storage, task, output)
        })
    }

    /// 在另一个线程里、设了时限地执行 `apply_leveled_result`，返回引擎。
    fn apply_leveled_with_deadline(
        storage: TestStorage,
        task: LeveledTask,
        output: Vec<Arc<SsTable>>,
    ) -> TestStorage {
        run_with_deadline("apply_leveled_result", move || {
            storage.apply_leveled_result(&task, output).unwrap();
            storage
        })
    }

    // L0 的任务换状态：L0 拿掉参加合并的文件，合并期间 flush 出的留下；下层只换掉重叠的
    // 那个文件，输出插在原来的位置，两边不重叠的文件原样不动；读到的都是最新的版本。
    #[test]
    fn apply_leveled_l0_task_replaces_only_the_overlapping_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(
            &storage,
            &dir,
            3,
            &[
                (101, &[("a", "L3")]),
                (102, &[("c", "L3"), ("d", "L3")]),
                (103, &[("x", "L3")]),
            ],
        );
        add_l0_sst(&storage, &dir, 109, &[("b", "L0"), ("c", "old")]);
        add_l0_sst(&storage, &dir, 110, &[("c", "new"), ("e", "L0")]);
        let task = leveled_task(&storage, (0, &[110, 109]), (3, &[102]));
        let (storage, task, output) = merge_leveled(storage, task);
        // 合并的期间，又 flush 出一个文件，放在 L0 的最前面。
        add_l0_sst(&storage, &dir, 111, &[("a", "newest")]);

        let storage = apply_leveled_with_deadline(storage, task, output.clone());

        let state = storage.snapshot();
        assert_eq!(level_ids(&state.l0_sstables), vec![111]);
        let mut expected_l3 = vec![101];
        expected_l3.extend(level_ids(&output));
        expected_l3.push(103);
        assert_eq!(level_ids(&state.levels[2]), expected_l3);
        assert_eq!(
            scan_all(&storage),
            expected(&[
                ("a", "newest"),
                ("b", "L0"),
                ("c", "new"),
                ("d", "L3"),
                ("e", "L0"),
                ("x", "L3")
            ])
        );
    }

    // 层与层之间的任务换状态：上层只拿掉挑出的那个文件，下层只换掉与它重叠的文件；
    // 下层不是最底层，墓碑照写，读的时候仍然盖住更旧的值。
    #[test]
    fn apply_leveled_level_task_replaces_only_the_overlapping_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(
            &storage,
            &dir,
            1,
            &[(104, &[("c", "L1"), ("d", "")]), (105, &[("y", "L1")])],
        );
        set_level_ssts(
            &storage,
            &dir,
            2,
            &[
                (101, &[("a", "L2"), ("b", "L2")]),
                (102, &[("c", "L2")]),
                (103, &[("d", "L2"), ("f", "L2")]),
                (106, &[("z", "L2")]),
            ],
        );
        set_level_ssts(&storage, &dir, 3, &[(107, &[("d", "L3")])]);
        let task = leveled_task(&storage, (1, &[104]), (2, &[102, 103]));
        let (storage, task, output) = merge_leveled(storage, task);
        assert_eq!(
            records_of(&output),
            expected(&[("c", "L1"), ("d", ""), ("f", "L2")])
        );

        let storage = apply_leveled_with_deadline(storage, task, output.clone());

        let state = storage.snapshot();
        assert_eq!(level_ids(&state.levels[0]), vec![105]);
        let mut expected_l2 = vec![101];
        expected_l2.extend(level_ids(&output));
        expected_l2.push(106);
        assert_eq!(level_ids(&state.levels[1]), expected_l2);
        assert_eq!(level_ids(&state.levels[2]), vec![107]);
        assert_eq!(
            scan_all(&storage),
            expected(&[
                ("a", "L2"),
                ("b", "L2"),
                ("c", "L1"),
                ("f", "L2"),
                ("y", "L1"),
                ("z", "L2")
            ])
        );
    }

    // 合并结果是空的（并进最底层时全部被删除）：两边只拿掉输入，什么也不插入。
    #[test]
    fn apply_leveled_with_empty_output_only_removes_the_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(&storage, &dir, 2, &[(104, &[("a", "")])]);
        set_level_ssts(
            &storage,
            &dir,
            3,
            &[(101, &[("a", "1")]), (102, &[("z", "1")])],
        );
        let task = leveled_task(&storage, (2, &[104]), (3, &[101]));
        let (storage, task, output) = merge_leveled(storage, task);
        assert!(output.is_empty());

        let storage = apply_leveled_with_deadline(storage, task, output);

        let state = storage.snapshot();
        assert!(state.levels[1].is_empty());
        assert_eq!(level_ids(&state.levels[2]), vec![102]);
        assert_eq!(get_str(&storage, "a"), None);
    }

    // trivial move：同一个文件对象从上层挪到下层，插在按 key 的正确位置。
    #[test]
    fn apply_leveled_trivial_move_puts_the_same_file_in_key_order() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(&storage, &dir, 1, &[(104, &[("m", "L1"), ("n", "L1")])]);
        set_level_ssts(
            &storage,
            &dir,
            2,
            &[(101, &[("a", "L2"), ("b", "L2")]), (103, &[("x", "L2")])],
        );
        let moved = Arc::clone(&storage.snapshot().levels[0][0]);
        let task = leveled_task(&storage, (1, &[104]), (2, &[]));
        assert!(task.is_trivial_move);
        let (storage, task, output) = merge_leveled(storage, task);

        let storage = apply_leveled_with_deadline(storage, task, output);

        let state = storage.snapshot();
        assert!(state.levels[0].is_empty());
        assert_eq!(level_ids(&state.levels[1]), vec![101, 104, 103]);
        assert!(Arc::ptr_eq(&state.levels[1][1], &moved));
    }

    // trivial move 到下层的最前面、最后面，以及挪进空的下层。
    #[test]
    fn apply_leveled_trivial_move_at_the_ends_and_into_an_empty_level() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(
            &storage,
            &dir,
            1,
            &[(104, &[("a", "L1")]), (105, &[("z", "L1")])],
        );
        set_level_ssts(&storage, &dir, 2, &[(101, &[("m", "L2")])]);

        let task = leveled_task(&storage, (1, &[104]), (2, &[]));
        let (storage, task, output) = merge_leveled(storage, task);
        let storage = apply_leveled_with_deadline(storage, task, output);
        let task = leveled_task(&storage, (1, &[105]), (2, &[]));
        let (storage, task, output) = merge_leveled(storage, task);
        let storage = apply_leveled_with_deadline(storage, task, output);
        assert_eq!(
            level_ids(&storage.snapshot().levels[1]),
            vec![104, 101, 105]
        );

        let task = leveled_task(&storage, (2, &[101]), (3, &[]));
        let (storage, task, output) = merge_leveled(storage, task);
        let storage = apply_leveled_with_deadline(storage, task, output);
        let state = storage.snapshot();
        assert_eq!(level_ids(&state.levels[1]), vec![104, 105]);
        assert_eq!(level_ids(&state.levels[2]), vec![101]);
    }

    // 合并期间下层参加合并的文件被换掉了：断言失败，而不是把别人的改动覆盖掉。
    #[test]
    #[should_panic(expected = "L2 里参加合并的文件少了")]
    fn apply_leveled_panics_if_a_lower_input_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(&storage, &dir, 1, &[(104, &[("c", "L1")])]);
        set_level_ssts(&storage, &dir, 2, &[(102, &[("c", "L2")])]);
        let task = leveled_task(&storage, (1, &[104]), (2, &[102]));
        let (storage, task, output) = merge_leveled(storage, task);
        set_level_ssts(&storage, &dir, 2, &[(106, &[("c", "changed")])]);

        apply_leveled_with_deadline(storage, task, output);
    }

    // 合并期间上层（不是 L0）挑出的文件不见了：同样断言失败。
    #[test]
    #[should_panic(expected = "L1 里参加合并的文件少了")]
    fn apply_leveled_panics_if_the_upper_input_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(&storage, &dir, 1, &[(104, &[("c", "L1")])]);
        set_level_ssts(&storage, &dir, 2, &[(102, &[("c", "L2")])]);
        let task = leveled_task(&storage, (1, &[104]), (2, &[102]));
        let (storage, task, output) = merge_leveled(storage, task);
        set_level_ssts(&storage, &dir, 1, &[]);

        apply_leveled_with_deadline(storage, task, output);
    }

    // 合并期间 L0 里参加合并的文件不见了：断言失败。
    #[test]
    #[should_panic(expected = "L0 里参加合并的文件少了")]
    fn apply_leveled_panics_if_l0_inputs_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        add_l0_sst(&storage, &dir, 109, &[("a", "1")]);
        add_l0_sst(&storage, &dir, 110, &[("b", "2")]);
        let task = leveled_task(&storage, (0, &[110, 109]), (3, &[]));
        let (storage, task, output) = merge_leveled(storage, task);
        {
            let _state_lock = storage.state_lock.lock().unwrap();
            let mut guard = storage.state.write().unwrap();
            let mut new_state = guard.as_ref().clone();
            new_state.l0_sstables.pop();
            *guard = Arc::new(new_state);
        }

        apply_leveled_with_deadline(storage, task, output);
    }

    // 任务漏挑了下层与它重叠的文件（策略的错误）：插入之后下层出现重叠，换状态时发现。
    #[test]
    #[should_panic(expected = "L2 不再按 key 排好、互不重叠")]
    fn apply_leveled_panics_if_the_lower_level_would_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_leveled(leveled_options());
        set_level_ssts(&storage, &dir, 1, &[(104, &[("c", "L1"), ("d", "L1")])]);
        set_level_ssts(&storage, &dir, 2, &[(102, &[("c", "L2")])]);
        // 102 与 104 重叠，任务却没有带上它，于是被当成 trivial move。
        let task = leveled_task(&storage, (1, &[104]), (2, &[]));
        let (storage, task, output) = merge_leveled(storage, task);

        apply_leveled_with_deadline(storage, task, output);
    }

    // compact_once 的 trivial move：不写文件、不删文件，文件对象直接挪进最底层；
    // 与已有文件重叠时才真正合并，这时旧文件被删掉。
    #[test]
    fn leveled_compact_once_moves_files_without_rewriting_them() {
        let storage = open_leveled(LeveledOptions {
            level0_file_num_compaction_trigger: 1,
            ..leveled_options()
        });
        let compact_once = |storage: TestStorage| -> TestStorage {
            run_with_deadline("compact_once", move || {
                assert!(storage.compact_once().unwrap());
                assert!(!storage.compact_once().unwrap());
                storage
            })
        };

        flush_records(&storage, &[("a", "1"), ("b", "1")]);
        let first = Arc::clone(&storage.snapshot().l0_sstables[0]);
        let storage = compact_once(storage);
        let state = storage.snapshot();
        assert!(state.l0_sstables.is_empty());
        assert_eq!(state.levels[2].len(), 1);
        assert!(Arc::ptr_eq(&state.levels[2][0], &first));
        assert_eq!(storage.stats.snapshot().compaction_bytes_written, 0);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));

        // 不重叠：挪到 first 的后面。
        flush_records(&storage, &[("x", "2")]);
        let second = Arc::clone(&storage.snapshot().l0_sstables[0]);
        let storage = compact_once(storage);
        assert_eq!(
            level_ids(&storage.snapshot().levels[2]),
            vec![first.id(), second.id()]
        );
        assert_eq!(storage.stats.snapshot().compaction_bytes_written, 0);

        // 与 first 重叠：真正合并，first 与新 flush 的文件被删掉，second 原样留下。
        flush_records(&storage, &[("b", "3"), ("c", "3")]);
        let storage = compact_once(storage);
        let bottom = storage.snapshot().levels[2].clone();
        assert!(Arc::ptr_eq(bottom.last().unwrap(), &second));
        assert!(!level_ids(&bottom).contains(&first.id()));
        assert!(storage.stats.snapshot().compaction_bytes_written > 0);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));
        assert_eq!(
            scan_all(&storage),
            expected(&[("a", "1"), ("b", "3"), ("c", "3"), ("x", "2")])
        );
    }

    // 顺序写入的情形：L0 的两个文件彼此不重叠，一起挪进最底层，按 key 排好（L0 里是
    // 新的在前）；不写文件。之后两个 L0 文件彼此重叠时才真正合并，结果插在中间。
    #[test]
    fn leveled_compact_once_moves_several_l0_files_together() {
        let storage = open_leveled(leveled_options());
        let compact_once = |storage: TestStorage| -> TestStorage {
            run_with_deadline("compact_once", move || {
                assert!(storage.compact_once().unwrap());
                assert!(!storage.compact_once().unwrap());
                storage
            })
        };

        flush_records(&storage, &[("a", "1"), ("b", "1")]);
        flush_records(&storage, &[("x", "2"), ("y", "2")]);
        let older = Arc::clone(&storage.snapshot().l0_sstables[1]);
        let newer = Arc::clone(&storage.snapshot().l0_sstables[0]);
        let storage = compact_once(storage);
        let bottom = storage.snapshot().levels[2].clone();
        assert_eq!(bottom.len(), 2);
        assert!(Arc::ptr_eq(&bottom[0], &older));
        assert!(Arc::ptr_eq(&bottom[1], &newer));
        assert_eq!(storage.stats.snapshot().compaction_bytes_written, 0);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));

        flush_records(&storage, &[("m", "3"), ("p", "3")]);
        flush_records(&storage, &[("n", "4"), ("q", "4")]);
        let storage = compact_once(storage);
        let bottom = storage.snapshot().levels[2].clone();
        assert!(Arc::ptr_eq(&bottom[0], &older));
        assert!(Arc::ptr_eq(bottom.last().unwrap(), &newer));
        assert!(storage.stats.snapshot().compaction_bytes_written > 0);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));
        assert_eq!(
            scan_all(&storage),
            expected(&[
                ("a", "1"),
                ("b", "1"),
                ("m", "3"),
                ("n", "4"),
                ("p", "3"),
                ("q", "4"),
                ("x", "2"),
                ("y", "2")
            ])
        );
    }

    // leveled 的 has_pending_compaction：L0 攒够 2 个文件时有，做完之后没有。
    #[test]
    fn leveled_has_pending_compaction() {
        let storage = open_leveled(leveled_options());
        flush_records(&storage, &[("a", "1")]);
        assert!(!storage.has_pending_compaction());
        flush_records(&storage, &[("a", "2")]);
        assert!(storage.has_pending_compaction());

        let storage = run_with_deadline("compact_once", move || {
            assert!(storage.compact_once().unwrap());
            storage
        });

        assert!(!storage.has_pending_compaction());
        assert_eq!(get_str(&storage, "a"), Some("2".to_string()));
    }

    // L1 有一个 key 的旧值，基准层是最底层 L3；L0 达到触发值时要并进 L1，
    // 不能跳过 L1 落进 L3，否则点查先查到 L1 里的旧值。
    #[test]
    fn leveled_l0_does_not_skip_a_level_with_data() {
        let storage = open_leveled(leveled_options());
        // 手工把一个文件挪进 L1：基准层是最底层，策略自己不会往 L1 放数据。
        flush_records(&storage, &[("k", "old")]);
        let old_id = storage.snapshot().l0_sstables[0].id();
        let task = leveled_task(&storage, (0, &[old_id]), (1, &[]));
        let (storage, task, output) = merge_leveled(storage, task);
        let storage = apply_leveled_with_deadline(storage, task, output);
        assert_eq!(level_ids(&storage.snapshot().levels[0]), vec![old_id]);

        flush_records(&storage, &[("k", "new")]);
        flush_records(&storage, &[("z", "1")]);
        let storage = run_with_deadline("compact_once", move || {
            assert!(storage.compact_once().unwrap());
            storage
        });

        assert_eq!(get_str(&storage, "k"), Some("new".to_string()));
        let state = storage.snapshot();
        assert!(state.l0_sstables.is_empty());
        assert!(!state.levels[0].is_empty());
        assert!(state.levels[2].is_empty());
        assert_eq!(scan_all(&storage), expected(&[("k", "new"), ("z", "1")]));
    }

    /// 每一轮之后 leveled 的形状检查：L0 不到触发值，L1 往下每层都是一个 sorted run。
    fn check_leveled_shape(storage: &TestStorage, round: usize, trigger: usize) {
        let snapshot = storage.snapshot();
        assert!(snapshot.l0_sstables.len() < trigger, "第 {round} 轮");
        for (index, level) in snapshot.levels.iter().enumerate() {
            assert!(
                is_sorted_run(level),
                "第 {round} 轮，L{} 不是一个 sorted run",
                index + 1
            );
        }
    }

    // leveled（基准大小远大于数据）：反复写入、flush、合并，每一轮读到的都与模型相同，
    // 目录里的文件正好是状态里的；L0 一直直接并进最底层，上面两层始终是空的。
    #[test]
    fn repeated_leveled_compact_once_goes_straight_to_the_bottom() {
        run_with_slow_deadline("反复 leveled compact_once", || {
            let options = leveled_options();
            let trigger = options.level0_file_num_compaction_trigger;
            let storage = open_leveled(options);
            random_rounds(&storage, 30, |storage, round| {
                check_leveled_shape(storage, round, trigger);
                let snapshot = storage.snapshot();
                assert!(snapshot.levels[0].is_empty(), "第 {round} 轮");
                assert!(snapshot.levels[1].is_empty(), "第 {round} 轮");
            });
        });
    }

    // leveled（基准大小 1 字节、比例 2）：每层的目标都大于 0，L0 并进 L1（基准层），
    // 超出目标的文件再一个一个往下推，部分合并会发生；每一轮读到的都与模型相同，
    // 目录里的文件正好是状态里的，每层都是一个 sorted run；有的轮次停下时不止一层有文件。
    #[test]
    fn repeated_leveled_compact_once_uses_several_levels() {
        run_with_slow_deadline("反复 leveled compact_once（多层）", || {
            let options = LeveledOptions {
                level0_file_num_compaction_trigger: 2,
                level_size_multiplier: 2,
                base_level_size_bytes: 1,
            };
            let trigger = options.level0_file_num_compaction_trigger;
            let storage = open_leveled(options);
            let most_levels_in_use = std::cell::Cell::new(0);
            random_rounds(&storage, 60, |storage, round| {
                check_leveled_shape(storage, round, trigger);
                let counts = storage.level_sstable_counts();
                let in_use = counts.iter().filter(|&&count| count > 0).count();
                most_levels_in_use.set(most_levels_in_use.get().max(in_use));
            });
            assert!(most_levels_in_use.get() >= 2);
        });
    }

    // leveled 的全量合并与简单分层相同：L0 与各层合并成最底层。
    #[test]
    fn leveled_full_compaction_merges_into_the_bottom_level() {
        let storage = open_leveled(leveled_options());
        flush_records(&storage, &[("a", "1"), ("b", "")]);

        let storage = full_compaction(storage);

        assert_eq!(storage.l0_sstable_count(), 0);
        assert_eq!(records_of(&bottom_level(&storage)), expected(&[("a", "1")]));
    }

    // ---------- 重启：各策略换过状态之后 ----------

    /// 重启前后的状态与读到的数据相同：L0 与 tier 的顺序、各层里文件的顺序（重启时按首 key
    /// 重新排好）都一样。返回重启之后的引擎。
    fn reopen_and_compare(storage: TestStorage) -> TestStorage {
        let ids_before = state_ids(&storage);
        let data_before = scan_all(&storage);
        let storage = storage.reopen();
        assert_eq!(state_ids(&storage), ids_before);
        assert_eq!(scan_all(&storage), data_before);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));
        storage
    }

    // 每种策略反复写入、合并之后重启，状态与数据都与重启前相同；再做一次全量合并、重启，
    // 同样相同。leveled 有两组参数：只用最底层（L0 的文件常被直接挪下去），与用好几层。
    #[test]
    fn reopen_restores_the_state_of_every_strategy() {
        let strategies = vec![
            CompactionOptions::SimpleLeveled(simple_options()),
            CompactionOptions::Tiered(TieredOptions {
                num_tiers: 4,
                max_size_amplification_percent: 200,
                size_ratio: 1,
                min_merge_width: 2,
                max_merge_width: None,
            }),
            CompactionOptions::Leveled(leveled_options()),
            CompactionOptions::Leveled(LeveledOptions {
                level0_file_num_compaction_trigger: 2,
                level_size_multiplier: 2,
                base_level_size_bytes: 1,
            }),
        ];
        for compaction in strategies {
            run_with_slow_deadline("各策略重启", move || {
                let storage = open_with_strategy(compaction);
                random_rounds(&storage, 30, |_storage, _round| {});
                let storage = reopen_and_compare(storage);
                // 分层时 L0 是空的全量合并什么也不做：先 flush 一个文件，让它真的合并。
                flush_records(&storage, &[("key-000", "last"), ("key-999", "last")]);
                let storage = full_compaction(storage);
                assert_eq!(storage.l0_sstable_count(), 0);
                assert_manifest_matches_state(&storage);
                reopen_and_compare(storage);
            });
        }
    }

    // manifest 只记"放进哪一层"，不记层内的位置：leveled 把文件插到层的中间时，记录的先后
    // 与 key 的先后不同。重启时按首 key 重新排，各层的顺序与重启前相同。
    #[test]
    fn reopen_sorts_each_level_by_first_key() {
        let storage = open_leveled(LeveledOptions {
            level0_file_num_compaction_trigger: 1,
            ..leveled_options()
        });
        // 三批互不重叠的 key，先 m、再 a、最后 x：每批都被原样挪进最底层，插在按 key 的位置。
        for batch in ["m", "a", "x"] {
            let first = format!("{batch}1");
            let second = format!("{batch}2");
            flush_records(&storage, &[(first.as_str(), "v"), (second.as_str(), "v")]);
            while storage.compact_once().unwrap() {}
        }
        let bottom_first_keys: Vec<Vec<u8>> = storage.snapshot().levels[2]
            .iter()
            .map(|sst| sst.first_key().to_vec())
            .collect();
        assert_eq!(
            bottom_first_keys,
            vec![b"a1".to_vec(), b"m1".to_vec(), b"x1".to_vec()]
        );

        reopen_and_compare(storage);
    }
}
