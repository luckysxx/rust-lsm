//! 归并游标：把多个各自有序的来源合成一个有序的游标。
//!
//! 同一个 key 在几个来源里都有时，只给出最新来源里的那一条，其余的跳过。
//! 墓碑不在这一层过滤：它要先和旧值比出新旧，胜出之后由更外面一层隐藏。

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use super::StorageIterator;

/// 堆里的一项：一个来源的游标，加上这个来源的新旧序号。
struct HeapItem<I: StorageIterator> {
    /// 来源的序号，越小越新。
    index: usize,
    /// 这个来源的游标。放在堆里的游标必须是有效的。
    iter: I,
}

impl<I: StorageIterator> Ord for HeapItem<I> {
    /// 决定谁先出堆。
    ///
    /// 标准库的 `BinaryHeap` 是大顶堆，"最大"的先出来。我们要的是 key 最小的先出来，
    /// key 相同时序号小（更新）的先出来，所以按（key，序号）比完之后要整体反过来。
    fn cmp(&self, other: &Self) -> Ordering {
        let by_key = self.iter.key().cmp(other.iter.key());
        let by_key_then_index = by_key.then(self.index.cmp(&other.index));
        by_key_then_index.reverse()
    }
}

// 下面三段是固定搭配：标准库要求能比大小的类型同时说明"部分有序"和"相等"，
// 这里都直接转给上面的 `cmp`。

impl<I: StorageIterator> PartialOrd for HeapItem<I> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<I: StorageIterator> PartialEq for HeapItem<I> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<I: StorageIterator> Eq for HeapItem<I> {}

/// 归并游标。`I` 是来源游标的类型，所有来源必须是同一种。
pub struct MergeIterator<I: StorageIterator> {
    /// 还没走到头的来源。堆顶就是归并游标当前指着的那条记录。
    heap: BinaryHeap<HeapItem<I>>,
}

impl<I: StorageIterator> MergeIterator<I> {
    /// 用若干来源创建归并游标。`iters` 里排在前面的来源更新。
    pub fn new(iters: Vec<I>) -> Self {
        let mut heap = BinaryHeap::new();
        for (index, iter) in iters.into_iter().enumerate() {
            // 一开始就无效的来源不进堆：堆里只放有效的游标。
            if iter.is_valid() {
                heap.push(HeapItem { index, iter });
            }
        }
        Self { heap }
    }
}

impl<I: StorageIterator> StorageIterator for MergeIterator<I> {
    fn key(&self) -> &[u8] {
        let top = self.heap.peek().expect("游标无效时不能读取 key");
        top.iter.key()
    }

    fn value(&self) -> &[u8] {
        let top = self.heap.peek().expect("游标无效时不能读取 value");
        top.iter.value()
    }

    fn is_valid(&self) -> bool {
        !self.heap.is_empty()
    }

    fn next(&mut self) {
        // 当前这条记录的 key。接下来要改动堆，所以先复制一份留着比较。
        let current_key = self.key().to_vec();

        // 堆顶上所有 key 等于它的来源都要前进：第一个是刚给出的胜者，其余的是旧版本。
        while let Some(top) = self.heap.peek() {
            if top.iter.key() != current_key {
                break;
            }

            // 取出、前进、放回：前进后 key 变了，放回时堆会给它重新排位置。
            // 走到头的来源不放回，堆里只留有效的游标。
            let mut item = self.heap.pop().unwrap();
            item.iter.next();
            if item.iter.is_valid() {
                self.heap.push(item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ops::Bound;
    use std::sync::Arc;

    use super::*;
    use crate::iterators::vec_iterator::{VecIterator, collect_all, expected};
    use crate::mem_table::{MemTable, MemTableIterator};

    /// 用几组字符串记录建一个归并游标，排在前面的一组更新。
    fn merge_of(sources: &[&[(&str, &str)]]) -> MergeIterator<VecIterator> {
        let mut iters = Vec::new();
        for pairs in sources {
            iters.push(VecIterator::from_strs(pairs));
        }
        MergeIterator::new(iters)
    }

    /// 把若干堆项按出堆的先后列出来，每项写成（key，序号）。
    fn pop_order(items: &[(&str, usize)]) -> Vec<(String, usize)> {
        let mut heap = BinaryHeap::new();
        for (key, index) in items {
            let iter = VecIterator::from_strs(&[(key, "v")]);
            heap.push(HeapItem {
                index: *index,
                iter,
            });
        }

        let mut order = Vec::new();
        while let Some(item) = heap.pop() {
            let key = String::from_utf8_lossy(item.iter.key()).into_owned();
            order.push((key, item.index));
        }
        order
    }

    // ---------- 堆里的比较 ----------

    // key 小的先出堆，与放进去的顺序和来源序号无关。
    #[test]
    fn heap_pops_smallest_key_first() {
        let order = pop_order(&[("c", 0), ("a", 2), ("b", 1)]);

        let keys: Vec<&str> = order.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    // key 相同时，序号小（更新）的来源先出堆。
    #[test]
    fn heap_pops_newer_source_first_on_equal_keys() {
        let order = pop_order(&[("b", 2), ("b", 0), ("b", 1)]);

        let indexes: Vec<usize> = order.iter().map(|(_, index)| *index).collect();
        assert_eq!(indexes, vec![0, 1, 2]);
    }

    // 先比 key、再比序号：key 更小的旧来源仍然排在 key 更大的新来源前面。
    #[test]
    fn heap_compares_key_before_index() {
        let order = pop_order(&[("b", 0), ("a", 5)]);

        assert_eq!(order, vec![("a".to_string(), 5), ("b".to_string(), 0)]);
    }

    // ---------- 归并 ----------

    // 没有来源，或者来源全是空的，归并游标一开始就是无效的。
    #[test]
    fn merge_of_nothing_is_invalid() {
        let no_sources = merge_of(&[]);
        let empty_sources = merge_of(&[&[], &[]]);

        assert!(!no_sources.is_valid());
        assert!(!empty_sources.is_valid());
    }

    // 只有一个来源时，原样给出。
    #[test]
    fn merge_of_single_source_passes_through() {
        let mut iter = merge_of(&[&[("a", "1"), ("b", "2")]]);

        assert_eq!(collect_all(&mut iter), expected(&[("a", "1"), ("b", "2")]));
    }

    // key 互不相同的几个来源，合成一个按 key 有序的序列。
    #[test]
    fn merge_interleaves_disjoint_keys_in_order() {
        let mut iter = merge_of(&[
            &[("b", "2"), ("e", "5")],
            &[("a", "1"), ("d", "4")],
            &[("c", "3")],
        ]);

        assert_eq!(
            collect_all(&mut iter),
            expected(&[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4"), ("e", "5")])
        );
    }

    // 两个来源有同一个 key：只给出一次，取较新来源（排在前面）里的值。
    #[test]
    fn merge_takes_newer_source_on_duplicate_key() {
        let mut iter = merge_of(&[&[("a", "new")], &[("a", "old")]]);

        assert_eq!(collect_all(&mut iter), expected(&[("a", "new")]));
    }

    // 三个来源都有同一个 key：仍然只给出一次，旧的两条都被跳过。
    #[test]
    fn merge_skips_all_older_versions() {
        let mut iter = merge_of(&[
            &[("a", "v0"), ("b", "v0")],
            &[("a", "v1"), ("b", "v1"), ("c", "v1")],
            &[("a", "v2"), ("c", "v2")],
        ]);

        assert_eq!(
            collect_all(&mut iter),
            expected(&[("a", "v0"), ("b", "v0"), ("c", "v1")])
        );
    }

    // 新旧只看来源的先后：把两个来源的顺序对调，胜出的值也跟着换。
    #[test]
    fn merge_winner_depends_on_source_order() {
        let mut first_is_x = merge_of(&[&[("a", "x")], &[("a", "y")]]);
        let mut first_is_y = merge_of(&[&[("a", "y")], &[("a", "x")]]);

        assert_eq!(collect_all(&mut first_is_x), expected(&[("a", "x")]));
        assert_eq!(collect_all(&mut first_is_y), expected(&[("a", "y")]));
    }

    // 墓碑不在这一层过滤：较新来源里的墓碑胜出并被原样给出，旧值不会出现。
    #[test]
    fn merge_keeps_winning_tombstone_and_hides_older_value() {
        let mut iter = merge_of(&[
            &[("b", ""), ("c", "4")],
            &[("a", "1"), ("b", "2"), ("c", "3")],
        ]);

        assert_eq!(
            collect_all(&mut iter),
            expected(&[("a", "1"), ("b", ""), ("c", "4")])
        );
    }

    // 笔记里逐轮走过的例子：T0 最新，T2 最旧。
    #[test]
    fn merge_worked_example_with_three_tables() {
        let mut iter = merge_of(&[
            &[("b", ""), ("c", "4")],
            &[("a", "1"), ("c", "3")],
            &[("b", "2"), ("d", "5")],
        ]);

        assert_eq!(
            collect_all(&mut iter),
            expected(&[("a", "1"), ("b", ""), ("c", "4"), ("d", "5")])
        );
    }

    // 有的来源先走完：它退出之后，其余来源继续归并。
    #[test]
    fn merge_continues_after_a_source_is_exhausted() {
        let mut iter = merge_of(&[&[("a", "1")], &[("b", "2"), ("c", "3"), ("d", "4")]]);

        assert_eq!(
            collect_all(&mut iter),
            expected(&[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")])
        );
    }

    // 来源换成 memtable 的游标也一样：较新的表里的值与墓碑胜出。
    #[test]
    fn merge_works_over_memtable_iterators() {
        let newer = MemTable::new();
        newer.put(b"b", b"");
        newer.put(b"c", b"4");
        let older = MemTable::new();
        older.put(b"a", b"1");
        older.put(b"b", b"2");
        older.put(b"c", b"3");

        let iters = vec![
            MemTableIterator::new(Arc::new(newer), Bound::Unbounded, Bound::Unbounded),
            MemTableIterator::new(Arc::new(older), Bound::Unbounded, Bound::Unbounded),
        ];
        let mut iter = MergeIterator::new(iters);

        assert_eq!(
            collect_all(&mut iter),
            expected(&[("a", "1"), ("b", ""), ("c", "4")])
        );
    }

    // 随机生成几组来源，归并的结果与"逐个 key 取最新来源"的直接算法一致。
    #[test]
    fn merge_matches_reference_model_on_random_sources() {
        // 简单的伪随机数，固定种子，结果可复现。
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut random = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };

        for _round in 0..200 {
            let source_count = random(6) as usize;

            // 每个来源：从 30 个可能的 key 里随机挑一些，value 记下它来自哪个来源。
            let mut sources: Vec<Vec<(String, String)>> = Vec::new();
            for source_index in 0..source_count {
                let mut records = Vec::new();
                for key_number in 0..30 {
                    if random(3) == 0 {
                        let key = format!("k{key_number:02}");
                        // 偶尔放一条墓碑。
                        let value = if random(5) == 0 {
                            String::new()
                        } else {
                            format!("from{source_index}")
                        };
                        records.push((key, value));
                    }
                }
                sources.push(records);
            }

            // 直接算法：从最旧的来源开始往一张有序表里写，新的覆盖旧的。
            let mut model: BTreeMap<String, String> = BTreeMap::new();
            for records in sources.iter().rev() {
                for (key, value) in records {
                    model.insert(key.clone(), value.clone());
                }
            }
            let expected_records: Vec<(String, String)> = model.into_iter().collect();

            let mut iters = Vec::new();
            for records in &sources {
                let pairs: Vec<(&str, &str)> = records
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str()))
                    .collect();
                iters.push(VecIterator::from_strs(&pairs));
            }
            let mut iter = MergeIterator::new(iters);

            assert_eq!(collect_all(&mut iter), expected_records);
        }
    }
}
