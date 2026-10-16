//! compaction：把一组 SST 归并成新的 SST。
//!
//! 本文件是 `lsm_storage` 的子模块。子模块能看到父模块里的私有内容，所以这里可以
//! 直接给 `LsmStorage` 加方法，并使用它的私有字段与 `write_ssts` 等私有方法，
//! 不必为了拆文件把它们改成公开的。

use std::collections::HashSet;
use std::sync::Arc;

use super::LsmStorage;
use crate::error::Result;
use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::table::{SsTable, SsTableIterator};

impl LsmStorage {
    /// 把 L0 与 L1 的全部文件合并成新的 L1（full compaction），然后删除旧文件。
    ///
    /// 步骤：记下此刻 L0 与 L1 的文件 → 在锁外合并、写新文件、同步目录 →
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
        // 同一时刻只做一次 compaction：L1 只由 compaction 改动，两次交错会互相覆盖。
        let _compaction_lock = self.compaction_lock.lock().unwrap();

        // 记下这次要合并的文件。之后 flush 可能往 L0 加新文件，它们不在这次的输入里。
        let snapshot = self.snapshot();
        let l0_inputs = snapshot.l0_sstables.clone();
        let l1_inputs = snapshot.l1_sstables.clone();
        if l0_inputs.is_empty() {
            return Ok(());
        }

        // 写新文件：慢，不占 `state_lock`，读、写、冻结、flush 都照常进行。
        let new_ssts = self.compact(&l0_inputs, &l1_inputs)?;
        if !new_ssts.is_empty() {
            self.sync_dir()?;
        }

        // 换状态：一瞬间。
        self.apply_compaction_result(&l0_inputs, &l1_inputs, new_ssts);

        // 新状态里已经没有旧文件了，删掉它们的文件名。
        for sst in l0_inputs.iter().chain(l1_inputs.iter()) {
            std::fs::remove_file(self.path_of_sst(sst.id()))?;
        }
        Ok(())
    }

    /// 把一次 compaction 的结果换进状态：从 L0 拿掉参加合并的文件，L1 整层换成新文件。
    ///
    /// `l0_inputs`、`l1_inputs` 是开始合并时记下的输入。这期间 flush 可能往 L0 的
    /// 前面加了新文件，它们比新的 L1 新，必须原样留在 L0：所以按编号拿掉输入，
    /// 不能清空 L0，也不能用开始时的快照覆盖当前状态。L1 只由 compaction 改动，
    /// 而 compaction 同一时刻只有一个，所以 L1 此时应当还是 `l1_inputs`。
    fn apply_compaction_result(
        &self,
        l0_inputs: &[Arc<SsTable>],
        l1_inputs: &[Arc<SsTable>],
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

        // L1：此时应当还是这次的输入，整层换成新文件。
        let l1_unchanged = new_state.l1_sstables.len() == l1_inputs.len()
            && new_state
                .l1_sstables
                .iter()
                .zip(l1_inputs)
                .all(|(current, input)| Arc::ptr_eq(current, input));
        assert!(l1_unchanged, "compaction 期间 L1 被别的操作改动了");
        new_state.l1_sstables = new_ssts;
        *guard = Arc::new(new_state);
    }

    /// 把 `l0` 与 `l1` 里的全部记录归并，写成新的 SST，返回它们（按 key 排好、互不重叠）。
    ///
    /// `l0` 新的在前，`l1` 按 key 排好。同一个 key 只留最新的版本。输入是全部的文件、
    /// 输出就是最底层，下面不会再有更旧的版本，所以最新版本是墓碑的 key 整个不写。
    /// 每个输出文件的大小由 `target_sst_size` 决定。
    ///
    /// 只写新文件：不改变状态，也不删除输入的文件，这两件事由调用方在之后完成。
    fn compact(&self, l0: &[Arc<SsTable>], l1: &[Arc<SsTable>]) -> Result<Vec<Arc<SsTable>>> {
        // L0 的文件范围互相重叠，用归并；列表里新的在前，归并时排在前面的胜出。
        let mut l0_iters = Vec::new();
        for sst in l0 {
            l0_iters.push(SsTableIterator::new_and_seek_to_first(Arc::clone(sst))?);
        }
        // L1 的文件按 key 排好、互不重叠，串接起来就有序。
        let l1_iter = SstConcatIterator::create_and_seek_to_first(l1.to_vec())?;
        // L0 比 L1 新，放在两路归并的前一个位置。
        let mut iter = TwoMergeIterator::new(MergeIterator::new(l0_iters), l1_iter)?;

        let ssts = self.write_ssts(&mut iter, self.options.target_sst_size, true)?;
        for sst in &ssts {
            self.stats.add_compaction_bytes_written(sst.file_size());
        }
        Ok(ssts)
    }
}

#[cfg(test)]
mod tests {
    use super::super::LsmStorageOptions;
    use super::super::tests::{
        TestStorage, add_l0_sst, get_str, open_with_options, open_without_auto_freeze, scan_all,
        set_l1_ssts,
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

    /// 把当前状态里 L0 与 L1 的全部文件交给 `compact`，返回引擎和写出的文件。
    ///
    /// 放在另一个线程里、设了时限：写文件的循环漏了让游标前进时，测试报"卡住"而不是停在那里。
    fn compact_all(storage: TestStorage) -> (TestStorage, Vec<Arc<SsTable>>) {
        run_with_deadline("compact", move || {
            let snapshot = storage.snapshot();
            let output = storage
                .compact(&snapshot.l0_sstables, &snapshot.l1_sstables)
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
        set_l1_ssts(
            &storage,
            &dir,
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

    // L0 里的墓碑盖住 L1 里的值，输出是最底层，墓碑和被删的值都不写进去。
    #[test]
    fn drops_deleted_keys_and_their_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let storage = open_without_auto_freeze();
        set_l1_ssts(
            &storage,
            &dir,
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
        set_l1_ssts(&storage, &dir, &[(101, &[("a", "1"), ("b", "2")])]);
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
            new_state.l1_sstables = from_l0;
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
        set_l1_ssts(&storage, &dir, &[(101, &[("a", "1")])]);
        add_l0_sst(&storage, &dir, 109, &[("b", "2")]);

        let (storage, _output) = compact_all(storage);

        assert_eq!(storage.l0_sstable_count(), 1);
        assert_eq!(storage.l1_sstable_count(), 1);
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

    // 合并结果是一个可以直接放进 L1 的序列：串接游标能从中定位。
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

    /// 当前状态里各个 SST 的文件名，按名字排序。
    fn names_in_state(ssts: &[Arc<SsTable>]) -> Vec<String> {
        let mut names: Vec<String> = ssts
            .iter()
            .map(|sst| format!("{:05}.sst", sst.id()))
            .collect();
        names.sort();
        names
    }

    // 合并之后 L0 是空的，全部数据在 L1；读到的与合并之前一样，被删除的 key 仍然不在。
    #[test]
    fn full_compaction_moves_everything_into_l1() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1"), ("b", "2"), ("c", "3")]);
        flush_records(&storage, &[("b", "new"), ("c", ""), ("d", "4")]);
        let before = scan_all(&storage);

        let storage = full_compaction(storage);

        assert_eq!(storage.l0_sstable_count(), 0);
        assert!(storage.l1_sstable_count() >= 1);
        assert_eq!(scan_all(&storage), before);
        assert_eq!(get_str(&storage, "b"), Some("new".to_string()));
        assert_eq!(get_str(&storage, "c"), None);
    }

    // 合并之后 L1 里没有墓碑，也没有被覆盖的旧版本：每个 key 只剩一条。
    #[test]
    fn full_compaction_leaves_no_tombstones_or_old_versions() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1"), ("b", "2")]);
        flush_records(&storage, &[("a", "new"), ("b", "")]);

        let storage = full_compaction(storage);

        let l1 = storage.snapshot().l1_sstables.clone();
        assert_eq!(records_of(&l1), expected(&[("a", "new")]));
    }

    // 旧文件从目录里删掉了：目录里剩下的正好是状态里 L1 的文件。
    #[test]
    fn full_compaction_deletes_old_files() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        flush_records(&storage, &[("b", "2")]);
        assert_eq!(storage.sst_file_names().len(), 2);

        let storage = full_compaction(storage);

        let l1 = storage.snapshot().l1_sstables.clone();
        assert_eq!(storage.sst_file_names(), names_in_state(&l1));
    }

    // L0 是空的：什么也不做，L1 原样不动。
    #[test]
    fn full_compaction_with_empty_l0_does_nothing() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        let storage = full_compaction(storage);
        let l1_before = storage.snapshot().l1_sstables.clone();

        let storage = full_compaction(storage);

        let l1_after = storage.snapshot().l1_sstables.clone();
        assert_eq!(names_in_state(&l1_after), names_in_state(&l1_before));
    }

    // 合并两次：第二次把新写的、覆盖的、删除的都并进已有的 L1。
    #[test]
    fn second_full_compaction_merges_into_existing_l1() {
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
        let l1 = storage.snapshot().l1_sstables.clone();
        assert_eq!(
            records_of(&l1),
            expected(&[("b", "new"), ("c", "3"), ("d", "4")])
        );
    }

    // 合并写新文件的期间 flush 出的 L0 文件：不在这次的输入里，换状态之后仍留在 L0，
    // 而且它比新的 L1 新，读到的是它的值。
    #[test]
    fn apply_keeps_l0_files_flushed_during_compaction() {
        // 整个过程设了时限：换状态时锁的用法出错（例如拿着写锁再去读状态）会卡住。
        run_with_deadline("apply_compaction_result", || {
            let storage = open_without_auto_freeze();
            flush_records(&storage, &[("a", "1"), ("b", "2")]);
            flush_records(&storage, &[("c", "3")]);

            // 记下输入、写新文件；这时还没有换状态。
            let snapshot = storage.snapshot();
            let l0_inputs = snapshot.l0_sstables.clone();
            let l1_inputs = snapshot.l1_sstables.clone();
            let new_ssts = storage.compact(&l0_inputs, &l1_inputs).unwrap();
            // 合并的期间，又 flush 出一个文件，放在 L0 的最前面。
            flush_records(&storage, &[("a", "newer")]);
            let flushed_during = Arc::clone(&storage.snapshot().l0_sstables[0]);

            storage.apply_compaction_result(&l0_inputs, &l1_inputs, new_ssts.clone());

            let state = storage.snapshot();
            assert_eq!(state.l0_sstables.len(), 1);
            assert!(Arc::ptr_eq(&state.l0_sstables[0], &flushed_during));
            assert_eq!(
                names_in_state(&state.l1_sstables),
                names_in_state(&new_ssts)
            );
            assert_eq!(get_str(&storage, "a"), Some("newer".to_string()));
            assert_eq!(get_str(&storage, "b"), Some("2".to_string()));
        });
    }

    // 合并结果为空（全部被删除）：L1 变成空的，L0 里参加合并的文件也拿掉了。
    #[test]
    fn apply_with_empty_result_empties_l1() {
        let storage = open_without_auto_freeze();
        flush_records(&storage, &[("a", "1")]);
        let storage = full_compaction(storage);
        flush_records(&storage, &[("a", "")]);

        let storage = full_compaction(storage);

        assert_eq!(storage.l0_sstable_count(), 0);
        assert_eq!(storage.l1_sstable_count(), 0);
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
}
