//! SST 上的游标：在一个 SST 文件里按 key 从小到大走，也能直接定位到某个 key。

use std::sync::Arc;

use super::SsTable;
use crate::block::BlockIterator;
use crate::error::Result;
use crate::iterators::StorageIterator;

/// 一个 SST 上的游标。墓碑原样给出。
///
/// 它记着"现在在第几个块"，手里拿着这个块上的一个游标；任何时刻只拿着一个块。
/// 当前的块走完，就从文件里读下一个块。只要它有效，块上的游标就指着一条记录。
pub struct SsTableIterator {
    /// 让 SST（打开的文件和索引）在游标用完之前不被释放。
    table: Arc<SsTable>,
    /// 现在在第几个块。
    block_index: usize,
    /// 当前这个块上的游标。它无效，整个游标就无效。
    block_iter: BlockIterator,
}

impl SsTableIterator {
    /// 创建游标，指着文件里的第一条记录。要读第一个块，所以可能失败。
    pub fn new_and_seek_to_first(table: Arc<SsTable>) -> Result<Self> {
        let (block_index, block_iter) = Self::position_at_first(&table)?;
        Ok(Self {
            table,
            block_index,
            block_iter,
        })
    }

    /// 创建游标，指着第一条 key 大于等于 `target` 的记录；没有这样的记录时游标无效。
    pub fn new_and_seek_to_key(table: Arc<SsTable>, target: &[u8]) -> Result<Self> {
        let (block_index, block_iter) = Self::position_at_key(&table, target)?;
        Ok(Self {
            table,
            block_index,
            block_iter,
        })
    }

    /// 回到文件里的第一条记录。
    pub fn seek_to_first(&mut self) -> Result<()> {
        let (block_index, block_iter) = Self::position_at_first(&self.table)?;
        self.block_index = block_index;
        self.block_iter = block_iter;
        Ok(())
    }

    /// 定位到第一条 key 大于等于 `target` 的记录；没有这样的记录时游标变为无效。
    pub fn seek_to_key(&mut self, target: &[u8]) -> Result<()> {
        let (block_index, block_iter) = Self::position_at_key(&self.table, target)?;
        self.block_index = block_index;
        self.block_iter = block_iter;
        Ok(())
    }

    /// 文件里的第一条记录在哪：第 0 个块，块上的游标指着它的第一条。
    ///
    /// 返回（块的下标，这个块上的游标）。
    fn position_at_first(table: &SsTable) -> Result<(usize, BlockIterator)> {
        let block = table.read_block(0)?;
        let block_iter = BlockIterator::new(block);
        Ok((0, block_iter))
    }

    /// 第一条 key 大于等于 `target` 的记录在哪。
    ///
    /// 返回（块的下标，这个块上的游标）。没有这样的记录时，返回的游标是无效的。
    fn position_at_key(table: &SsTable, target: &[u8]) -> Result<(usize, BlockIterator)> {
        // 第一个末 key 大于等于 target 的块。target 比文件里所有的 key 都大时没有
        // 这样的块：停在最后一个块里，下面的块内定位自然会走到头，游标无效。
        let mut block_index = table.find_block_index(target);
        if block_index == table.block_count() {
            block_index -= 1;
        }

        let block = table.read_block(block_index)?;
        let mut block_iter = BlockIterator::new(block);
        block_iter.seek_to_key(target);
        Ok((block_index, block_iter))
    }
}

impl StorageIterator for SsTableIterator {
    fn key(&self) -> &[u8] {
        self.block_iter.key()
    }

    fn value(&self) -> &[u8] {
        self.block_iter.value()
    }

    fn is_valid(&self) -> bool {
        self.block_iter.is_valid()
    }

    fn next(&mut self) -> Result<()> {
        self.block_iter.next()?;

        // 当前的块走完了，后面还有块：换到下一个块的第一条。
        // 后面没有块了，就让块上的游标保持无效，整个游标随之无效。
        let has_next_block = self.block_index + 1 < self.table.block_count();
        if !self.block_iter.is_valid() && has_next_block {
            self.block_index += 1;
            let next_block = self.table.read_block(self.block_index)?;
            self.block_iter = BlockIterator::new(next_block);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::iterators::merge_iterator::MergeIterator;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use crate::table::{FileObject, SsTableBuilder};
    use std::path::Path;
    use tempfile::TempDir;

    /// 用字符串写出记录，做成一个 SST 文件，再从磁盘上重新打开它。
    fn write_and_open(
        dir: &TempDir,
        name: &str,
        block_size: usize,
        pairs: &[(&str, &str)],
    ) -> Arc<SsTable> {
        let path = dir.path().join(name);
        let mut builder = SsTableBuilder::new(block_size);
        for (key, value) in pairs {
            builder.add(key.as_bytes(), value.as_bytes());
        }
        builder.build(1, None, &path).unwrap();
        open(&path)
    }

    fn open(path: &Path) -> Arc<SsTable> {
        let file = FileObject::open(path).unwrap();
        Arc::new(SsTable::open(1, None, file).unwrap())
    }

    /// 七条记录分在三个块里：[a, c]、[d, f]、[g, g]。
    fn three_block_sst(dir: &TempDir) -> Arc<SsTable> {
        write_and_open(
            dir,
            "1.sst",
            32,
            &[
                ("a", "1"),
                ("b", "2"),
                ("c", "3"),
                ("d", "4"),
                ("e", "5"),
                ("f", "6"),
                ("g", "7"),
            ],
        )
    }

    /// 定位到 `target` 之后游标指着的 key；游标无效时为 `None`。
    fn key_after_seek(table: &Arc<SsTable>, target: &str) -> Option<String> {
        let iter =
            SsTableIterator::new_and_seek_to_key(Arc::clone(table), target.as_bytes()).unwrap();
        if iter.is_valid() {
            Some(String::from_utf8_lossy(iter.key()).into_owned())
        } else {
            None
        }
    }

    // ---------- 顺序走 ----------

    // 刚创建的游标指着文件里的第一条记录。
    #[test]
    fn new_iterator_points_at_first_record() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);

        let iter = SsTableIterator::new_and_seek_to_first(table).unwrap();

        assert!(iter.is_valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"1");
    }

    // 从头走到尾：跨过块的边界，给出全部记录，然后变为无效。
    #[test]
    fn iterator_walks_across_block_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        assert_eq!(table.block_count(), 3);
        let mut iter = SsTableIterator::new_and_seek_to_first(table).unwrap();

        let collected = collect_all(&mut iter);

        assert_eq!(
            collected,
            expected(&[
                ("a", "1"),
                ("b", "2"),
                ("c", "3"),
                ("d", "4"),
                ("e", "5"),
                ("f", "6"),
                ("g", "7"),
            ])
        );
        assert!(!iter.is_valid());
    }

    // 只有一个块、一条记录的文件：走一步就无效。
    #[test]
    fn iterator_over_single_record_file() {
        let dir = tempfile::tempdir().unwrap();
        let table = write_and_open(&dir, "1.sst", 4096, &[("only", "one")]);
        let mut iter = SsTableIterator::new_and_seek_to_first(table).unwrap();
        assert_eq!(iter.key(), b"only");

        iter.next().unwrap();

        assert!(!iter.is_valid());
    }

    // 一千条记录、很多个块：读回来的与写进去的完全相同，顺序不变。
    #[test]
    fn iterator_reads_back_a_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let owned: Vec<(String, String)> = (0..1000)
            .map(|i| (format!("key-{i:04}"), format!("value-{i:04}")))
            .collect();
        let pairs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let table = write_and_open(&dir, "1.sst", 256, &pairs);
        assert!(table.block_count() > 50);
        let mut iter = SsTableIterator::new_and_seek_to_first(table).unwrap();

        let collected = collect_all(&mut iter);

        assert_eq!(collected, owned);
    }

    // 墓碑（空 value）原样给出，这一层不过滤。
    #[test]
    fn tombstones_are_passed_through() {
        let dir = tempfile::tempdir().unwrap();
        let table = write_and_open(&dir, "1.sst", 16, &[("a", "1"), ("b", ""), ("c", "3")]);
        let mut iter = SsTableIterator::new_and_seek_to_first(table).unwrap();

        let collected = collect_all(&mut iter);

        assert_eq!(collected, expected(&[("a", "1"), ("b", ""), ("c", "3")]));
    }

    // 游标自己让 SST 活着：调用方不再持有它，游标照样走完。
    #[test]
    fn iterator_keeps_table_alive() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        let mut iter = SsTableIterator::new_and_seek_to_first(Arc::clone(&table)).unwrap();
        drop(table);

        let collected = collect_all(&mut iter);

        assert_eq!(collected.len(), 7);
    }

    // 走到头之后可以回到第一条重新走。
    #[test]
    fn seek_to_first_rewinds() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        let mut iter = SsTableIterator::new_and_seek_to_first(table).unwrap();
        collect_all(&mut iter);
        assert!(!iter.is_valid());

        iter.seek_to_first().unwrap();

        assert_eq!(iter.key(), b"a");
        assert_eq!(collect_all(&mut iter).len(), 7);
    }

    // ---------- 定位 ----------

    // 给定的 key 在文件里：停在它上面，不管它是块的第一条、中间还是最后一条。
    #[test]
    fn seek_to_existing_key_stops_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);

        for key in ["a", "b", "c", "d", "e", "f", "g"] {
            assert_eq!(key_after_seek(&table, key), Some(key.to_string()));
        }
    }

    // 给定的 key 落在两个块之间：停在后一个块的第一条上。
    #[test]
    fn seek_between_blocks_stops_on_first_record_of_next_block() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);

        assert_eq!(key_after_seek(&table, "cc"), Some("d".to_string()));
        assert_eq!(key_after_seek(&table, "fz"), Some("g".to_string()));
    }

    // 给定的 key 在块的中间但不存在：停在同一个块里比它大的第一条上。
    #[test]
    fn seek_to_missing_key_inside_a_block() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);

        assert_eq!(key_after_seek(&table, "aa"), Some("b".to_string()));
        assert_eq!(key_after_seek(&table, "dz"), Some("e".to_string()));
    }

    // 比文件里所有的 key 都小：停在第一条上。比所有的都大：游标无效。
    #[test]
    fn seek_outside_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);

        assert_eq!(key_after_seek(&table, "A"), Some("a".to_string()));
        assert_eq!(key_after_seek(&table, "gg"), None);
        assert_eq!(key_after_seek(&table, "z"), None);
    }

    // 定位之后 value 来自同一条记录，接着往后走能跨块走完剩下的记录。
    #[test]
    fn iteration_continues_after_seek() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        let mut iter = SsTableIterator::new_and_seek_to_key(table, b"bb").unwrap();
        assert_eq!(iter.value(), b"3");

        let rest = collect_all(&mut iter);

        assert_eq!(
            rest,
            expected(&[("c", "3"), ("d", "4"), ("e", "5"), ("f", "6"), ("g", "7")])
        );
    }

    // 定位到后面的块之后继续走：接下来读的是它的下一个块，不是从头数的第二个块。
    #[test]
    fn iteration_continues_after_seek_into_a_later_block() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        let mut iter = SsTableIterator::new_and_seek_to_key(table, b"e").unwrap();

        let rest = collect_all(&mut iter);

        assert_eq!(rest, expected(&[("e", "5"), ("f", "6"), ("g", "7")]));
    }

    // 已有的游标可以反复定位，与之前指着哪里无关：往后、往前、越过末尾、再回来。
    #[test]
    fn seek_to_key_can_be_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        let mut iter = SsTableIterator::new_and_seek_to_first(table).unwrap();

        iter.seek_to_key(b"f").unwrap();
        assert_eq!(iter.key(), b"f");
        iter.seek_to_key(b"b").unwrap();
        assert_eq!(iter.key(), b"b");
        iter.seek_to_key(b"z").unwrap();
        assert!(!iter.is_valid());
        iter.seek_to_key(b"cc").unwrap();
        assert_eq!(iter.key(), b"d");
    }

    // 三百条记录、很多个块，对每个可能的目标逐一核对：定位的结果与
    // "从头数第一条大于等于目标的记录"一致。
    #[test]
    fn seek_matches_linear_scan_for_every_target() {
        let dir = tempfile::tempdir().unwrap();
        // 文件里的 key 是奇数：0001、0003、0005……
        let keys: Vec<String> = (0..300).map(|i| format!("{:04}", 2 * i + 1)).collect();
        let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), "v")).collect();
        let table = write_and_open(&dir, "1.sst", 64, &pairs);
        assert!(table.block_count() > 30);

        // 目标取遍 0000 到最大的 key 之后一个，奇数偶数都有。
        for target_number in 0..=601 {
            let target = format!("{target_number:04}");
            let by_linear_scan = keys.iter().find(|key| **key >= target).cloned();

            let by_seek = key_after_seek(&table, &target);

            assert_eq!(by_seek, by_linear_scan, "目标是 {target}");
        }
    }

    // ---------- 出错 ----------

    // 游标创建之后文件被截短：走到要读下一个块时，`next` 返回读写错误。
    #[test]
    fn next_reports_io_error_when_next_block_cannot_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        let mut iter = SsTableIterator::new_and_seek_to_first(table).unwrap();
        // 第一个块占 [0, 32)，已经读进内存。把文件截到只剩这一个块。
        let path = dir.path().join("1.sst");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(32).unwrap();

        // 第一个块里的三条照常能走。
        assert_eq!(iter.key(), b"a");
        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        iter.next().unwrap();
        assert_eq!(iter.key(), b"c");

        // 再往前要读第二个块，读不到。
        let result = iter.next();

        assert!(matches!(result, Err(Error::Io(_))));
    }

    // 文件里的块损坏时，定位到这个块会报告内容损坏。
    #[test]
    fn seek_reports_corrupted_block() {
        let dir = tempfile::tempdir().unwrap();
        let table = three_block_sst(&dir);
        let path = dir.path().join("1.sst");
        let mut bytes = std::fs::read(&path).unwrap();
        // 第二个块占 [32, 64)，把它的条数改成 0。
        bytes[62] = 0;
        bytes[63] = 0;
        std::fs::write(&path, &bytes).unwrap();
        let table_after_damage = open(&path);
        drop(table);

        let result = SsTableIterator::new_and_seek_to_key(table_after_damage, b"e");

        assert!(matches!(result, Err(Error::Corruption(_))));
    }

    // ---------- 与上层配合 ----------

    // SST 上的游标实现了统一的接口，可以直接交给归并：两个文件里同一个 key 取排在前面的。
    #[test]
    fn sst_iterators_can_be_merged() {
        let dir = tempfile::tempdir().unwrap();
        let newer = write_and_open(&dir, "2.sst", 16, &[("a", "new"), ("c", "")]);
        let older = write_and_open(&dir, "1.sst", 16, &[("a", "old"), ("b", "2"), ("c", "3")]);
        let mut merged = MergeIterator::new(vec![
            SsTableIterator::new_and_seek_to_first(newer).unwrap(),
            SsTableIterator::new_and_seek_to_first(older).unwrap(),
        ]);

        let collected = collect_all(&mut merged);

        assert_eq!(collected, expected(&[("a", "new"), ("b", "2"), ("c", "")]));
    }
}
