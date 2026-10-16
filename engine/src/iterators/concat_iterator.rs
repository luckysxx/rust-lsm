//! 串接游标：在一组按 key 排好、范围互不重叠的 SST 上，一个文件走完再走下一个。
//!
//! 一层里的 SST（例如 L1）合起来是一个排好序的序列，前一个文件的 key 全都小于
//! 后一个文件的 key。所以不需要像归并那样同时打开每个文件、用堆挑最小的：按顺序
//! 一个接一个地读，结果就是有序的。
//!
//! 任何时刻只打开一个文件的游标。走到某个文件时才打开它，不会一开始就给每个文件
//! 读一个块。

use crate::error::Result;
use crate::iterators::StorageIterator;
use crate::table::{SsTable, SsTableIterator};
use std::sync::Arc;

/// 在一组 SST 里，`key` 可能在第几个文件：第一个末 key 大于等于 `key` 的文件。
///
/// `sstables` 必须按 key 排好、范围互不重叠。`key` 比所有文件的末 key 都大时，
/// 没有这样的文件，返回 `sstables.len()`。返回的文件也不一定有 `key`：
/// `key` 可能落在它和前一个文件之间，比它的首 key 还小。
pub fn find_sst_index(sstables: &[Arc<SsTable>], key: &[u8]) -> usize {
    // 在文件的下标上二分，与 `SsTable::find_block_index` 在块上的二分相同。
    let mut low = 0;
    let mut high = sstables.len();
    while low < high {
        let mid = low + (high - low) / 2;
        if sstables[mid].last_key() >= key {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    low
}

/// 一组按 key 排好、范围互不重叠的 SST 上的游标。墓碑原样给出。
pub struct SstConcatIterator {
    /// 当前这个文件上的游标；所有文件都走完时为 `None`。
    ///
    /// 只要它是 `Some`，它就指着一条记录：走完的文件会立刻换成下一个。
    current: Option<SsTableIterator>,
    /// 下一个要打开的文件在 `sstables` 里的下标。
    next_sst_index: usize,
    /// 这一组文件，按 key 从小到大，范围互不重叠。
    sstables: Vec<Arc<SsTable>>,
}

impl SstConcatIterator {
    /// 创建游标，指着第一个文件的第一条记录。没有文件时游标无效。
    ///
    /// 要读第一个文件的第一个块，所以可能失败。
    pub fn create_and_seek_to_first(sstables: Vec<Arc<SsTable>>) -> Result<Self> {
        check_sorted_and_disjoint(&sstables);
        let mut iterator = Self {
            current: None,
            next_sst_index: 0,
            sstables,
        };
        iterator.move_to_next_sst()?;
        Ok(iterator)
    }

    /// 创建游标，指着第一条 key 大于等于 `key` 的记录；没有这样的记录时游标无效。
    ///
    /// 只打开 `key` 所在的那个文件，要读它的一个块，所以可能失败。
    pub fn create_and_seek_to_key(sstables: Vec<Arc<SsTable>>, key: &[u8]) -> Result<Self> {
        check_sorted_and_disjoint(&sstables);
        // 末 key 小于 key 的文件整个跳过。找到的文件末 key 大于等于 key，
        // 在它里面定位一定有效；找不到时游标无效。
        let index = find_sst_index(&sstables, key);
        let mut current = None;
        if let Some(sst) = sstables.get(index) {
            let sst_iter = SsTableIterator::new_and_seek_to_key(Arc::clone(sst), key)?;
            current = Some(sst_iter);
        }
        Ok(Self {
            current,
            next_sst_index: index + 1,
            sstables,
        })
    }

    /// 打开下一个文件，指着它的第一条记录；没有下一个文件时游标变为无效。
    fn move_to_next_sst(&mut self) -> Result<()> {
        if self.next_sst_index == self.sstables.len() {
            self.current = None;
            return Ok(());
        }
        self.current = Some(SsTableIterator::new_and_seek_to_first(Arc::clone(
            &self.sstables[self.next_sst_index],
        ))?);
        // 先用，后加：下标指的是"下一个要打开的"文件。
        self.next_sst_index += 1;
        Ok(())
    }
}

impl StorageIterator for SstConcatIterator {
    fn key(&self) -> &[u8] {
        let current = self.current.as_ref().expect("游标无效时不能读取 key");
        current.key()
    }

    fn value(&self) -> &[u8] {
        let current = self.current.as_ref().expect("游标无效时不能读取 value");
        current.value()
    }

    fn is_valid(&self) -> bool {
        self.current.is_some()
    }

    fn next(&mut self) -> Result<()> {
        match self.current.as_mut() {
            Some(iter) => {
                iter.next()?;
                // 这个文件走完了，换下一个。文件不会是空的，换上的游标一定指着一条记录。
                if !iter.is_valid() {
                    self.move_to_next_sst()?;
                }
            }
            None => panic!("游标无效时不能前进"),
        }
        Ok(())
    }
}

/// 调试编译下检查这组文件按 key 排好、范围互不重叠：前一个的末 key 小于后一个的首 key。
fn check_sorted_and_disjoint(sstables: &[Arc<SsTable>]) {
    debug_assert!(
        sstables
            .windows(2)
            .all(|pair| pair[0].last_key() < pair[1].first_key()),
        "串接游标的文件没有按 key 排好，或者范围有重叠"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use crate::stats::Stats;
    use crate::table::SsTableBuilder;
    use tempfile::TempDir;

    /// 把一串记录写成一个 SST 文件。记录必须已经按 key 从小到大排好。
    /// 读块时记进 `stats`，用来数游标打开了几个文件。
    fn sst_of(
        dir: &TempDir,
        id: usize,
        pairs: &[(&str, &str)],
        stats: &Arc<Stats>,
    ) -> Arc<SsTable> {
        let path = dir.path().join(format!("{id:05}.sst"));
        let mut builder = SsTableBuilder::new(4096);
        for (key, value) in pairs {
            builder.add(key.as_bytes(), value.as_bytes());
        }
        let sst = builder.build(id, None, &path).unwrap();
        Arc::new(sst.with_stats(Arc::clone(stats)))
    }

    /// 三个文件：[a, c]、[e, g]、[i, k]，文件之间留着空隙（d、h 不存在）。
    fn three_ssts(dir: &TempDir, stats: &Arc<Stats>) -> Vec<Arc<SsTable>> {
        vec![
            sst_of(dir, 1, &[("a", "1"), ("b", "2"), ("c", "3")], stats),
            sst_of(dir, 2, &[("e", "5"), ("f", ""), ("g", "7")], stats),
            sst_of(dir, 3, &[("i", "9"), ("k", "11")], stats),
        ]
    }

    /// 定位到 `key` 之后游标指着的 key；游标无效时为 `None`。
    fn key_after_seek(sstables: &[Arc<SsTable>], key: &str) -> Option<String> {
        let iter =
            SstConcatIterator::create_and_seek_to_key(sstables.to_vec(), key.as_bytes()).unwrap();
        if iter.is_valid() {
            Some(String::from_utf8_lossy(iter.key()).into_owned())
        } else {
            None
        }
    }

    // ---------- 找文件 ----------

    // 找的是第一个末 key 大于等于 key 的文件：落在文件里、落在空隙里、比全部都大。
    #[test]
    fn find_sst_index_picks_first_file_not_ending_before_key() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let sstables = three_ssts(&dir, &stats);

        assert_eq!(find_sst_index(&sstables, b"0"), 0);
        assert_eq!(find_sst_index(&sstables, b"a"), 0);
        assert_eq!(find_sst_index(&sstables, b"c"), 0);
        assert_eq!(find_sst_index(&sstables, b"d"), 1);
        assert_eq!(find_sst_index(&sstables, b"g"), 1);
        assert_eq!(find_sst_index(&sstables, b"h"), 2);
        assert_eq!(find_sst_index(&sstables, b"k"), 2);
        assert_eq!(find_sst_index(&sstables, b"z"), 3);
    }

    // 没有文件时，任何 key 都返回 0，也就是文件的个数。
    #[test]
    fn find_sst_index_on_empty_list_is_zero() {
        assert_eq!(find_sst_index(&[], b"a"), 0);
    }

    // ---------- 顺序走 ----------

    // 从头走到尾：一个文件接一个文件，给出全部记录（墓碑原样给出），然后变为无效。
    #[test]
    fn walks_all_files_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let mut iter =
            SstConcatIterator::create_and_seek_to_first(three_ssts(&dir, &stats)).unwrap();

        let collected = collect_all(&mut iter);

        assert_eq!(
            collected,
            expected(&[
                ("a", "1"),
                ("b", "2"),
                ("c", "3"),
                ("e", "5"),
                ("f", ""),
                ("g", "7"),
                ("i", "9"),
                ("k", "11"),
            ])
        );
        assert!(!iter.is_valid());
    }

    // 没有文件：游标一开始就无效。
    #[test]
    fn empty_list_is_invalid() {
        let iter = SstConcatIterator::create_and_seek_to_first(Vec::new()).unwrap();
        assert!(!iter.is_valid());

        let iter = SstConcatIterator::create_and_seek_to_key(Vec::new(), b"a").unwrap();
        assert!(!iter.is_valid());
    }

    // 只有一个文件时与这个文件自己的游标一样。
    #[test]
    fn single_file_behaves_like_its_own_iterator() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let sstables = vec![sst_of(&dir, 1, &[("a", "1"), ("b", "2")], &stats)];
        let mut iter = SstConcatIterator::create_and_seek_to_first(sstables).unwrap();

        assert_eq!(collect_all(&mut iter), expected(&[("a", "1"), ("b", "2")]));
    }

    // ---------- 定位 ----------

    // 定位到文件里存在的 key、文件内不存在的 key、文件之间的空隙、两端之外。
    #[test]
    fn seek_lands_on_first_key_not_less_than_target() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let sstables = three_ssts(&dir, &stats);

        assert_eq!(key_after_seek(&sstables, "0"), Some("a".to_string()));
        assert_eq!(key_after_seek(&sstables, "b"), Some("b".to_string()));
        assert_eq!(key_after_seek(&sstables, "bb"), Some("c".to_string()));
        // 落在第一个和第二个文件之间：给出第二个文件的第一条。
        assert_eq!(key_after_seek(&sstables, "d"), Some("e".to_string()));
        assert_eq!(key_after_seek(&sstables, "g"), Some("g".to_string()));
        assert_eq!(key_after_seek(&sstables, "h"), Some("i".to_string()));
        assert_eq!(key_after_seek(&sstables, "k"), Some("k".to_string()));
        assert_eq!(key_after_seek(&sstables, "z"), None);
    }

    // 定位到某个文件的最后一条之后接着走，要换到下一个文件。
    #[test]
    fn next_after_seek_crosses_into_following_files() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let mut iter =
            SstConcatIterator::create_and_seek_to_key(three_ssts(&dir, &stats), b"c").unwrap();

        let collected = collect_all(&mut iter);

        assert_eq!(
            collected,
            expected(&[
                ("c", "3"),
                ("e", "5"),
                ("f", ""),
                ("g", "7"),
                ("i", "9"),
                ("k", "11"),
            ])
        );
    }

    // ---------- 只打开用到的文件 ----------

    // 建立游标时只读一个块：只打开第一个文件，后面的文件走到时才打开。
    #[test]
    fn opens_only_the_first_file_until_it_is_needed() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let sstables = three_ssts(&dir, &stats);

        let mut iter = SstConcatIterator::create_and_seek_to_first(sstables).unwrap();
        assert_eq!(stats.snapshot().blocks_read, 1);

        // 走完第一个文件的三条，换到第二个文件时才读它的块。
        iter.next().unwrap();
        iter.next().unwrap();
        assert_eq!(stats.snapshot().blocks_read, 1);
        iter.next().unwrap();
        assert_eq!(stats.snapshot().blocks_read, 2);
    }

    // 定位时只打开目标所在的那一个文件，前面的文件一个块都不读。
    #[test]
    fn seek_opens_only_the_target_file() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let sstables = three_ssts(&dir, &stats);

        let iter = SstConcatIterator::create_and_seek_to_key(sstables, b"i").unwrap();

        assert_eq!(iter.key(), b"i");
        assert_eq!(stats.snapshot().blocks_read, 1);
    }

    // ---------- 与上层配合 ----------

    // 实现了统一的游标接口：可以和别的游标一起交给两路归并，同一个 key 左边胜出。
    #[test]
    fn can_be_merged_with_newer_data() {
        use crate::iterators::two_merge_iterator::TwoMergeIterator;
        use crate::iterators::vec_iterator::VecIterator;

        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let older = SstConcatIterator::create_and_seek_to_first(three_ssts(&dir, &stats)).unwrap();
        let newer = VecIterator::from_strs(&[("b", "new"), ("d", "4"), ("f", "6")]);
        let mut merged = TwoMergeIterator::new(newer, older).unwrap();

        let collected = collect_all(&mut merged);

        assert_eq!(
            collected,
            expected(&[
                ("a", "1"),
                ("b", "new"),
                ("c", "3"),
                ("d", "4"),
                ("e", "5"),
                ("f", "6"),
                ("g", "7"),
                ("i", "9"),
                ("k", "11"),
            ])
        );
    }

    // 文件的范围有重叠时，调试编译下立刻报错，而不是悄悄给出乱序的结果。
    #[test]
    #[should_panic(expected = "范围有重叠")]
    fn overlapping_files_are_rejected_in_debug_builds() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let sstables = vec![
            sst_of(&dir, 1, &[("a", "1"), ("c", "3")], &stats),
            sst_of(&dir, 2, &[("b", "2"), ("d", "4")], &stats),
        ];

        let _ = SstConcatIterator::create_and_seek_to_first(sstables);
    }
}
