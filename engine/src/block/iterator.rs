//! 块上的游标：在一个做好的块里按 key 从小到大走，也能直接定位到某个 key。

use std::sync::Arc;

use super::Block;
use crate::iterators::StorageIterator;

/// 一个块上的游标。墓碑原样给出。
///
/// 块是一段不再变化的字节，位置用"第几条记录"这个下标表示就够了，
/// 所以游标里只有一个 `Arc`（让块活着）和一个整数，没有指向块内部的引用。
pub struct BlockIterator {
    /// 让块在游标用完之前不被释放。
    block: Arc<Block>,
    /// 游标现在指着第几条记录。等于记录的条数表示已经走到头。
    index: usize,
}

impl BlockIterator {
    /// 创建游标，指着块里的第一条记录。
    pub fn new(block: Arc<Block>) -> Self {
        Self { block, index: 0 }
    }

    /// 回到块里的第一条记录。
    pub fn seek_to_first(&mut self) {
        self.index = 0;
    }

    /// 定位到第一条 key 大于等于 `target` 的记录；没有这样的记录时游标变为无效。
    ///
    /// 与游标之前指着哪里无关，每次都在整个块里找。
    pub fn seek_to_key(&mut self, target: &[u8]) {
        // 在下标上二分。始终保持：low 之前的 key 都小于 target，
        // high 以及之后的 key 都大于等于 target；[low, high) 是还没看过的。
        // 这一段空了，low 就是第一条大于等于 target 的记录；全都小于时它等于条数。
        let mut low = 0;
        let mut high = self.block.record_count();
        while low < high {
            let mid = low + (high - low) / 2;
            let (mid_key, _mid_value) = self.block.entry(mid);
            if mid_key < target {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        self.index = low;
    }
}

impl StorageIterator for BlockIterator {
    fn key(&self) -> &[u8] {
        self.block.entry(self.index).0
    }

    fn value(&self) -> &[u8] {
        self.block.entry(self.index).1
    }

    fn is_valid(&self) -> bool {
        self.index < self.block.record_count()
    }

    fn next(&mut self) {
        self.index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockBuilder;
    use crate::iterators::merge_iterator::MergeIterator;
    use crate::iterators::vec_iterator::{collect_all, expected};

    /// 用字符串写出记录，做成一个块。记录必须已经按 key 从小到大排好。
    fn block_of(pairs: &[(&str, &str)]) -> Arc<Block> {
        let mut builder = BlockBuilder::new(4096);
        for (key, value) in pairs {
            assert!(builder.add(key.as_bytes(), value.as_bytes()));
        }
        Arc::new(builder.build())
    }

    /// 定位到 `target` 之后游标指着的 key；游标无效时为 `None`。
    fn key_after_seek(block: &Arc<Block>, target: &str) -> Option<String> {
        let mut iter = BlockIterator::new(Arc::clone(block));
        iter.seek_to_key(target.as_bytes());
        if iter.is_valid() {
            Some(String::from_utf8_lossy(iter.key()).into_owned())
        } else {
            None
        }
    }

    // ---------- 顺序走 ----------

    // 刚创建的游标指着第一条记录。
    #[test]
    fn new_iterator_points_at_first_record() {
        let block = block_of(&[("a", "1"), ("b", "2")]);

        let iter = BlockIterator::new(block);

        assert!(iter.is_valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"1");
    }

    // 从头走到尾，按顺序给出全部记录，然后变为无效。
    #[test]
    fn iterator_yields_all_records_in_order() {
        let block = block_of(&[("a", "1"), ("bb", "22"), ("ccc", "333")]);
        let mut iter = BlockIterator::new(block);

        let collected = collect_all(&mut iter);

        assert_eq!(
            collected,
            expected(&[("a", "1"), ("bb", "22"), ("ccc", "333")])
        );
        assert!(!iter.is_valid());
    }

    // 只有一条记录的块：走一步就无效。
    #[test]
    fn iterator_over_single_record_becomes_invalid_after_one_step() {
        let block = block_of(&[("a", "1")]);
        let mut iter = BlockIterator::new(block);
        assert!(iter.is_valid());

        iter.next();

        assert!(!iter.is_valid());
    }

    // 墓碑（空 value）原样给出，这一层不过滤。
    #[test]
    fn tombstones_are_passed_through() {
        let block = block_of(&[("a", "1"), ("b", ""), ("c", "3")]);
        let mut iter = BlockIterator::new(block);

        let collected = collect_all(&mut iter);

        assert_eq!(collected, expected(&[("a", "1"), ("b", ""), ("c", "3")]));
    }

    // 游标自己让块活着：调用方不再持有这个块，游标照样走完。
    #[test]
    fn iterator_keeps_block_alive() {
        let block = block_of(&[("a", "1"), ("b", "2")]);
        let mut iter = BlockIterator::new(Arc::clone(&block));
        drop(block);

        let collected = collect_all(&mut iter);

        assert_eq!(collected, expected(&[("a", "1"), ("b", "2")]));
    }

    // 从编码后的字节解码出来的块，游标走出来的内容与原来的一样。
    #[test]
    fn iterator_works_on_a_decoded_block() {
        let block = block_of(&[("a", "1"), ("b", ""), ("cc", "333")]);
        let decoded = Arc::new(Block::decode(&block.encode()));
        let mut iter = BlockIterator::new(decoded);

        let collected = collect_all(&mut iter);

        assert_eq!(collected, expected(&[("a", "1"), ("b", ""), ("cc", "333")]));
    }

    // ---------- 定位 ----------

    // 给定的 key 在块里：停在它上面。
    #[test]
    fn seek_to_existing_key_stops_on_it() {
        let block = block_of(&[("b", "2"), ("d", "4"), ("f", "6"), ("h", "8")]);

        assert_eq!(key_after_seek(&block, "b"), Some("b".to_string()));
        assert_eq!(key_after_seek(&block, "d"), Some("d".to_string()));
        assert_eq!(key_after_seek(&block, "f"), Some("f".to_string()));
        assert_eq!(key_after_seek(&block, "h"), Some("h".to_string()));
    }

    // 给定的 key 不在块里：停在比它大的第一条上。
    #[test]
    fn seek_to_missing_key_stops_on_next_larger() {
        let block = block_of(&[("b", "2"), ("d", "4"), ("f", "6"), ("h", "8")]);

        assert_eq!(key_after_seek(&block, "c"), Some("d".to_string()));
        assert_eq!(key_after_seek(&block, "e"), Some("f".to_string()));
        assert_eq!(key_after_seek(&block, "g"), Some("h".to_string()));
    }

    // 给定的 key 比块里所有的 key 都小：停在第一条上。
    #[test]
    fn seek_before_all_keys_stops_on_first() {
        let block = block_of(&[("b", "2"), ("d", "4"), ("f", "6"), ("h", "8")]);

        assert_eq!(key_after_seek(&block, "a"), Some("b".to_string()));
    }

    // 给定的 key 比块里所有的 key 都大：游标无效。
    #[test]
    fn seek_past_all_keys_is_invalid() {
        let block = block_of(&[("b", "2"), ("d", "4"), ("f", "6"), ("h", "8")]);

        assert_eq!(key_after_seek(&block, "i"), None);
        assert_eq!(key_after_seek(&block, "z"), None);
    }

    // key 按字节逐个比较，较短的前缀排在前面："ab" < "abc" < "b"。
    #[test]
    fn seek_compares_keys_as_byte_strings() {
        let block = block_of(&[("ab", "1"), ("abc", "2"), ("b", "3")]);

        assert_eq!(key_after_seek(&block, "a"), Some("ab".to_string()));
        assert_eq!(key_after_seek(&block, "abb"), Some("abc".to_string()));
        assert_eq!(key_after_seek(&block, "abcd"), Some("b".to_string()));
        assert_eq!(key_after_seek(&block, "bb"), None);
    }

    // 定位之后 value 来自同一条记录，接着往后走能走完剩下的记录。
    #[test]
    fn iteration_continues_after_seek() {
        let block = block_of(&[("b", "2"), ("d", "4"), ("f", "6"), ("h", "8")]);
        let mut iter = BlockIterator::new(block);

        iter.seek_to_key(b"e");
        assert_eq!(iter.value(), b"6");
        let rest = collect_all(&mut iter);

        assert_eq!(rest, expected(&[("f", "6"), ("h", "8")]));
    }

    // 定位与游标之前的位置无关：往后定位之后还能往前定位。
    #[test]
    fn seek_does_not_depend_on_previous_position() {
        let block = block_of(&[("b", "2"), ("d", "4"), ("f", "6"), ("h", "8")]);
        let mut iter = BlockIterator::new(block);

        iter.seek_to_key(b"g");
        assert_eq!(iter.key(), b"h");
        iter.seek_to_key(b"c");
        assert_eq!(iter.key(), b"d");
        iter.seek_to_key(b"z");
        assert!(!iter.is_valid());
        iter.seek_to_key(b"a");
        assert_eq!(iter.key(), b"b");
    }

    // 走到头之后可以回到第一条重新走。
    #[test]
    fn seek_to_first_rewinds() {
        let block = block_of(&[("a", "1"), ("b", "2")]);
        let mut iter = BlockIterator::new(block);
        collect_all(&mut iter);
        assert!(!iter.is_valid());

        iter.seek_to_first();

        assert_eq!(collect_all(&mut iter), expected(&[("a", "1"), ("b", "2")]));
    }

    // 只有一条记录的块：小于等于它的停在它上面，大于它的无效。
    #[test]
    fn seek_in_single_record_block() {
        let block = block_of(&[("m", "1")]);

        assert_eq!(key_after_seek(&block, "a"), Some("m".to_string()));
        assert_eq!(key_after_seek(&block, "m"), Some("m".to_string()));
        assert_eq!(key_after_seek(&block, "n"), None);
    }

    // 记录条数从 1 到 40 的块，对每个可能的目标逐一核对：定位的结果与
    // "从头数第一条大于等于目标的记录"一致。
    #[test]
    fn seek_matches_linear_scan_for_every_size() {
        for record_count in 1..=40 {
            // 块里的 key 是奇数：001、003、005……
            let keys: Vec<String> = (0..record_count)
                .map(|i| format!("{:03}", 2 * i + 1))
                .collect();
            let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (key.as_str(), "v")).collect();
            let block = block_of(&pairs);

            // 目标取遍 000 到最大的 key 之后一个，奇数偶数都有。
            for target_number in 0..=(2 * record_count + 1) {
                let target = format!("{target_number:03}");
                let by_linear_scan = keys.iter().find(|key| **key >= target).cloned();

                let by_seek = key_after_seek(&block, &target);

                assert_eq!(
                    by_seek, by_linear_scan,
                    "块里有 {record_count} 条记录，目标是 {target}"
                );
            }
        }
    }

    // ---------- 与上层配合 ----------

    // 块上的游标实现了统一的接口，可以直接交给归并：两个块里同一个 key 取排在前面的。
    #[test]
    fn block_iterators_can_be_merged() {
        let newer = block_of(&[("a", "new"), ("c", "")]);
        let older = block_of(&[("a", "old"), ("b", "2"), ("c", "3")]);
        let mut merged =
            MergeIterator::new(vec![BlockIterator::new(newer), BlockIterator::new(older)]);

        let collected = collect_all(&mut merged);

        assert_eq!(collected, expected(&[("a", "new"), ("b", "2"), ("c", "")]));
    }
}
