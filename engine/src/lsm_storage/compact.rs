//! compaction 的执行：按任务把一组 SST 归并成新的 SST，换进状态，删除旧文件。
//!
//! 合并什么由 `crate::compaction` 里的策略决定，这里只负责照做。有两种入口：
//! 手动的全量合并 `force_full_compaction`，与按简单分层策略做一个任务的 `compact_once`。
//!
//! 本文件是 `lsm_storage` 的子模块。子模块能看到父模块里的私有内容，所以这里可以
//! 直接给 `LsmStorage` 加方法，并使用它的私有字段与 `write_ssts` 等私有方法，
//! 不必为了拆文件把它们改成公开的。

use std::collections::HashSet;
use std::sync::Arc;

use super::LsmStorage;
use crate::compaction::simple_leveled::{self, SimpleLeveledOptions, SimpleLeveledTask};
use crate::error::Result;
use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::table::{SsTable, SsTableIterator};

impl LsmStorage {
    /// 把 L0 与各层的全部文件合并成新的最底层（full compaction），然后删除旧文件。
    ///
    /// 步骤：记下此刻 L0 与各层的文件 → 在锁外合并、写新文件、同步目录 →
    /// 拿 `state_lock` 换状态 → 删除旧文件。合并期间 flush 照常进行，新 flush 出的
    /// L0 文件不参加这次合并，换状态之后仍留在 L0。L0 是空的时候什么也不做。
    ///
    /// 旧文件在换状态之后立刻删除。仍拿着旧快照的读者照样能读完：它们的 `SsTable`
    /// 一直开着文件，Unix 上删除只去掉文件名，最后一个打开它的一方关闭之后，文件系统
    /// 才回收空间。
    ///
    /// 写新文件失败时返回错误，状态不变，写了一半的新文件留在目录里，没有任何地方引用。
    /// 删除旧文件失败时也返回错误，这时新状态已经生效，旧文件留在目录里。
    pub fn force_full_compaction(&self) -> Result<()> {
        // 同一时刻只做一次 compaction：L1 往下的层只由 compaction 改动，两次交错会互相覆盖。
        let _compaction_lock = self.compaction_lock.lock().unwrap();

        // 记下这次要合并的文件。之后 flush 可能往 L0 加新文件，它们不在这次的输入里。
        let snapshot = self.snapshot();
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
        self.apply_full_compaction_result(&l0_inputs, &level_inputs, new_ssts);

        // 新状态里已经没有旧文件了，删掉它们的文件名。
        let old_ssts = l0_inputs.iter().chain(level_inputs.iter().flatten());
        for sst in old_ssts {
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
    ) {
        // 与冻结、flush 一样：先拿 `state_lock` 排队，在当前状态的副本上改，再换指针。
        let _state_lock = self.state_lock.lock().unwrap();
        let mut guard = self.state.write().unwrap();
        let mut new_state = guard.as_ref().clone();

        // L0：只拿掉参加了合并的文件，合并期间新 flush 出的留下。
        let l0_count_before = new_state.l0_sstables.len();
        let compacted_l0_ids: HashSet<usize> = l0_inputs.iter().map(|sst| sst.id()).collect();
        // 留下不是这次输入的文件，顺序不变。
        new_state
            .l0_sstables
            .retain(|sst| !compacted_l0_ids.contains(&sst.id()));
        assert_eq!(
            l0_count_before - new_state.l0_sstables.len(),
            l0_inputs.len(),
            "compaction 期间 L0 里参加合并的文件少了"
        );

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
        *guard = Arc::new(new_state);
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
    /// 按简单分层策略做一个合并任务，然后删除旧文件。返回这次有没有做：
    /// 没有需要合并的层时什么也不做，返回 `false`。
    ///
    /// 步骤与 `force_full_compaction` 相同：在当前状态上生成任务（记下两层的文件）→
    /// 在锁外合并、写新文件、同步目录 → 拿 `state_lock` 换状态 → 删除旧文件。
    /// 一次合并之后下面一对层常常也需要合并，调用方应当反复调用，直到返回 `false`。
    ///
    /// 出错时的情况也与 `force_full_compaction` 相同。
    pub fn compact_once(&self, options: &SimpleLeveledOptions) -> Result<bool> {
        // 同一时刻只做一次 compaction：L1 往下的层只由 compaction 改动，两次交错会互相覆盖。
        // 任务要在拿到锁之后生成，看到的才是上一次合并换完之后的状态。
        let _compaction_lock = self.compaction_lock.lock().unwrap();

        let snapshot = self.snapshot();
        let Some(task) = simple_leveled::generate_task(options, &snapshot) else {
            return Ok(false);
        };

        // 写新文件：慢，不占 `state_lock`，读、写、冻结、flush 都照常进行。
        let new_ssts = self.compact_task(&task)?;
        if !new_ssts.is_empty() {
            self.sync_dir()?;
        }

        // 换状态：一瞬间。
        self.apply_compaction_result(&task, new_ssts);

        // 新状态里已经没有这两层的旧文件了，删掉它们的文件名。
        let old_ssts = task.upper_ssts.iter().chain(task.lower_ssts.iter());
        for sst in old_ssts {
            std::fs::remove_file(self.path_of_sst(sst.id()))?;
        }
        Ok(true)
    }

    /// 把任务里上层与下层的全部记录归并，写成新的 SST，返回它们（按 key 排好、互不重叠）。
    ///
    /// 上层比下层新，同一个 key 留上层的版本。下层是最底层时，最新版本是墓碑的 key
    /// 整个不写；不是最底层时墓碑照写，更下面的层里可能还有这个 key 的旧值，要靠它盖住。
    ///
    /// 只写新文件：不改变状态，也不删除输入的文件。
    fn compact_task(&self, task: &SimpleLeveledTask) -> Result<Vec<Arc<SsTable>>> {
        let target_sst_size = self.options.target_sst_size;
        let drop_tombstones = task.is_lower_level_bottom;
        // 下层总是按 key 排好、互不重叠的一层，串接起来就有序。
        let lower_iter = SstConcatIterator::create_and_seek_to_first(task.lower_ssts.clone())?;

        // 上层是 L0 时文件范围互相重叠，要归并；是 Ln 时与下层一样串接。两种游标的类型
        // 不同，拼出的两路归并也是不同的类型，所以分两支各写一次文件。
        let ssts = if task.upper_level == 0 {
            // L0 的列表里新的在前，归并时排在前面的胜出。
            let mut l0_iters = Vec::new();
            for sst in &task.upper_ssts {
                l0_iters.push(SsTableIterator::new_and_seek_to_first(Arc::clone(sst))?);
            }
            let mut iter = TwoMergeIterator::new(MergeIterator::new(l0_iters), lower_iter)?;
            self.write_ssts(&mut iter, target_sst_size, drop_tombstones)?
        } else {
            let upper_iter = SstConcatIterator::create_and_seek_to_first(task.upper_ssts.clone())?;
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
    fn apply_compaction_result(&self, task: &SimpleLeveledTask, new_ssts: Vec<Arc<SsTable>>) {
        // 与冻结、flush 一样：先拿 `state_lock` 排队，在当前状态的副本上改，再换指针。
        let _state_lock = self.state_lock.lock().unwrap();
        let mut guard = self.state.write().unwrap();
        let mut new_state = guard.as_ref().clone();

        // 上层。
        if task.upper_level == 0 {
            // L0：只拿掉参加了合并的文件，合并期间新 flush 出的留下。
            let l0_count_before = new_state.l0_sstables.len();
            let compacted_l0_ids: HashSet<usize> =
                task.upper_ssts.iter().map(|sst| sst.id()).collect();
            // 留下不是这次输入的文件，顺序不变。
            new_state
                .l0_sstables
                .retain(|sst| !compacted_l0_ids.contains(&sst.id()));
            assert_eq!(
                l0_count_before - new_state.l0_sstables.len(),
                task.upper_ssts.len(),
                "compaction 期间 L0 里参加合并的文件少了"
            );
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

        *guard = Arc::new(new_state);
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

#[cfg(test)]
mod tests {
    use super::super::LsmStorageOptions;
    use super::super::tests::{
        TestStorage, add_l0_sst, get_str, open_with_options, open_without_auto_freeze, scan_all,
        set_level_ssts,
    };
    use super::*;
    use crate::iterators::StorageIterator;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use crate::test_util::run_with_deadline;
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
                storage.delete(key.as_bytes());
            } else {
                storage.put(key.as_bytes(), value.as_bytes());
            }
        }
        storage.freeze_memtable();
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

            storage.apply_full_compaction_result(&l0_inputs, &level_inputs, new_ssts.clone());

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
            compaction: None,
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

    // ---------- 按任务合并：compact_task、apply_compaction_result、compact_once ----------

    /// 触发值 2、比例 200%。
    fn simple_options() -> SimpleLeveledOptions {
        SimpleLeveledOptions {
            level0_file_num_compaction_trigger: 2,
            size_ratio_percent: 200,
        }
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

    /// 在另一个线程里、设了时限地执行 `compact_task`，返回引擎、任务和写出的文件。
    fn run_task(
        storage: TestStorage,
        upper_level: usize,
    ) -> (TestStorage, SimpleLeveledTask, Vec<Arc<SsTable>>) {
        run_with_deadline("compact_task", move || {
            let task = task_for(&storage, upper_level);
            let output = storage.compact_task(&task).unwrap();
            (storage, task, output)
        })
    }

    /// 在另一个线程里、设了时限地执行 `apply_compaction_result`，返回引擎。
    /// 换状态时锁的用法出错（例如拿着写锁再去取快照）会卡住，这里报"卡住"。
    fn apply_with_deadline(
        storage: TestStorage,
        task: SimpleLeveledTask,
        output: Vec<Arc<SsTable>>,
    ) -> TestStorage {
        run_with_deadline("apply_compaction_result", move || {
            storage.apply_compaction_result(&task, output);
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
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        let names_before = names_of_whole_state(&storage);

        let storage = run_with_deadline("compact_once", move || {
            assert!(!storage.compact_once(&simple_options()).unwrap());
            storage
        });

        assert_eq!(names_of_whole_state(&storage), names_before);
        assert_eq!(storage.sst_file_names(), names_before);
    }

    // L0 攒够 2 个文件：做一个任务（L0 并进 L1），旧文件从目录里删掉，数据照常读得到。
    #[test]
    fn compact_once_runs_one_task_and_deletes_its_inputs() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1"), ("b", "2")]);
        flush_records(&storage, &[("a", "new"), ("c", "3")]);
        let before = scan_all(&storage);

        let storage = run_with_deadline("compact_once", move || {
            assert!(storage.compact_once(&simple_options()).unwrap());
            storage
        });

        assert_eq!(storage.l0_sstable_count(), 0);
        assert!(storage.level_sstable_counts()[0] >= 1);
        assert_eq!(storage.sst_file_names(), names_of_whole_state(&storage));
        assert_eq!(scan_all(&storage), before);
    }

    // 反复写入、flush、合并到停下：每一轮读到的都与一个 BTreeMap 模型相同，目录里的文件
    // 正好是状态里的文件，形状满足策略（没有任务可做），最底层里没有墓碑。
    #[test]
    fn repeated_compact_once_keeps_data_and_settles() {
        run_with_deadline("反复 compact_once", || {
            let storage = open_without_auto_freeze();
            let options = simple_options();
            let mut model = std::collections::BTreeMap::new();
            // 固定种子的伪随机数，每次运行都一样。
            let mut seed: u64 = 7;
            let mut next_random = move || {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                seed >> 33
            };

            for round in 0..30 {
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
                flush_records(&storage, &borrowed);
                for (key, value) in &pairs {
                    if value.is_empty() {
                        model.remove(key);
                    } else {
                        model.insert(key.clone(), value.clone());
                    }
                }

                let mut tasks = 0;
                while storage.compact_once(&options).unwrap() {
                    tasks += 1;
                    assert!(tasks <= 100, "第 {round} 轮停不下来");
                }

                let expected_pairs: Vec<(String, String)> = model
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                assert_eq!(scan_all(&storage), expected_pairs, "第 {round} 轮");
                assert_eq!(
                    storage.sst_file_names(),
                    names_of_whole_state(&storage),
                    "第 {round} 轮"
                );
                assert!(simple_leveled::generate_task(&options, &storage.snapshot()).is_none());
                let bottom = bottom_level(&storage);
                let tombstones = records_of(&bottom)
                    .iter()
                    .filter(|(_key, value)| value.is_empty())
                    .count();
                assert_eq!(tombstones, 0, "第 {round} 轮，最底层里有墓碑");
            }
        });
    }
}
