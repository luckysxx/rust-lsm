//! 两路归并：把两个各自有序的游标合成一个有序的游标。
//!
//! 与 `MergeIterator` 做的是同一件事，区别在于两个来源可以是不同的类型，
//! 而且只有两个，所以不需要堆，每一步直接比较两边当前的 key。
//! 引擎用它把"memtable 那一组"和"SST 那一组"合起来。

use super::StorageIterator;
use crate::error::Result;

/// 两路归并游标。`a` 比 `b` 新：同一个 key 两边都有时，给出 `a` 的那条，跳过 `b` 的。
///
/// 墓碑不在这一层过滤，与 `MergeIterator` 相同。
pub struct TwoMergeIterator<A: StorageIterator, B: StorageIterator> {
    a: A,
    b: B,
    /// 当前给出的记录来自 `a`（真）还是 `b`（假）。两边都走到头时为假。
    use_a: bool,
}

impl<A: StorageIterator, B: StorageIterator> TwoMergeIterator<A, B> {
    /// 用两个游标创建。`a` 是较新的一方。
    ///
    /// 两边一开始可能指着同一个 key，要先让 `b` 跳过它，所以创建也可能失败。
    pub fn new(a: A, b: B) -> Result<Self> {
        // 先把结构体建出来（`use_a` 随便填一个初值），再做两件事建立起正确的状态：
        // 跳过 b 里与 a 相同的 key，决定当前用哪一边。
        let mut two_merge_iterator = Self { a, b, use_a: false };
        two_merge_iterator.skip_b_if_same_key()?;
        two_merge_iterator.choose();
        Ok(two_merge_iterator)
    }

    /// 两边当前的 key 相同时，让 `b` 前进一步：这个 key 以 `a` 的为准。
    ///
    /// 每个来源里同一个 key 至多一条，所以跳一步之后两边的 key 一定不同。
    fn skip_b_if_same_key(&mut self) -> Result<()> {
        if !self.a.is_valid() || !self.b.is_valid() {
            return Ok(());
        }
        if self.a.key() == self.b.key() {
            return self.b.next();
        }
        Ok(())
    }

    /// 决定当前的记录来自哪一边：只有一边有效就用那一边，两边都有效用 key 小的。
    ///
    /// 调用之前已经做过 `skip_b_if_same_key`，两边的 key 不会相同。
    fn choose(&mut self) {
        if !self.a.is_valid() {
            self.use_a = false;
            return;
        }
        if !self.b.is_valid() {
            self.use_a = true;
            return;
        }
        self.use_a = self.a.key() < self.b.key();
    }
}

impl<A: StorageIterator, B: StorageIterator> StorageIterator for TwoMergeIterator<A, B> {
    fn key(&self) -> &[u8] {
        if self.use_a {
            return self.a.key();
        }
        self.b.key()
    }

    fn value(&self) -> &[u8] {
        if self.use_a {
            return self.a.value();
        }
        self.b.value()
    }

    fn is_valid(&self) -> bool {
        self.a.is_valid() || self.b.is_valid()
    }

    fn next(&mut self) -> Result<()> {
        if self.use_a {
            self.a.next()?;
        } else {
            self.b.next()?;
        }
        self.skip_b_if_same_key()?;
        self.choose();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::iterators::vec_iterator::{VecIterator, collect_all, expected};

    /// 用两串记录建一个两路归并游标，收集它给出的全部结果。
    fn merged(a: &[(&str, &str)], b: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut iter =
            TwoMergeIterator::new(VecIterator::from_strs(a), VecIterator::from_strs(b)).unwrap();
        collect_all(&mut iter)
    }

    // 两边都没有记录：一开始就无效。
    #[test]
    fn both_empty_is_invalid() {
        let iter = TwoMergeIterator::new(VecIterator::from_strs(&[]), VecIterator::from_strs(&[]))
            .unwrap();

        assert!(!iter.is_valid());
    }

    // 只有一边有记录：原样给出那一边。
    #[test]
    fn one_side_empty_passes_the_other_through() {
        assert_eq!(
            merged(&[("a", "1"), ("b", "2")], &[]),
            expected(&[("a", "1"), ("b", "2")])
        );
        assert_eq!(
            merged(&[], &[("a", "1"), ("b", "2")]),
            expected(&[("a", "1"), ("b", "2")])
        );
    }

    // 两边的 key 互不相同：按 key 从小到大交替给出。
    #[test]
    fn disjoint_keys_are_interleaved_in_order() {
        let result = merged(
            &[("a", "1"), ("d", "4"), ("e", "5")],
            &[("b", "2"), ("c", "3"), ("f", "6")],
        );

        assert_eq!(
            result,
            expected(&[
                ("a", "1"),
                ("b", "2"),
                ("c", "3"),
                ("d", "4"),
                ("e", "5"),
                ("f", "6"),
            ])
        );
    }

    // 同一个 key 两边都有：给出 a 的那条，b 的那条不出现。
    #[test]
    fn same_key_takes_a_and_skips_b() {
        let result = merged(&[("a", "new"), ("c", "3")], &[("a", "old"), ("b", "2")]);

        assert_eq!(result, expected(&[("a", "new"), ("b", "2"), ("c", "3")]));
    }

    // 开头、中间、结尾的 key 都撞上：每个 key 只出现一次，都是 a 的值。
    #[test]
    fn same_keys_everywhere_are_deduplicated() {
        let result = merged(
            &[("a", "A"), ("b", "B"), ("c", "C")],
            &[("a", "x"), ("b", "y"), ("c", "z")],
        );

        assert_eq!(result, expected(&[("a", "A"), ("b", "B"), ("c", "C")]));
    }

    // 谁新由位置决定：把两边对调，结果里那个 key 的值跟着换。
    #[test]
    fn winner_depends_on_which_side_is_a() {
        let a_first = merged(&[("k", "from-a")], &[("k", "from-b")]);
        let swapped = merged(&[("k", "from-b")], &[("k", "from-a")]);

        assert_eq!(a_first, expected(&[("k", "from-a")]));
        assert_eq!(swapped, expected(&[("k", "from-b")]));
    }

    // a 的墓碑照样胜出并原样给出，b 里的旧值被跳过；b 自己的墓碑也原样给出。
    #[test]
    fn tombstones_are_kept_and_still_win() {
        let result = merged(&[("a", ""), ("c", "3")], &[("a", "old"), ("b", "")]);

        assert_eq!(result, expected(&[("a", ""), ("b", ""), ("c", "3")]));
    }

    // 一边先走完之后，另一边剩下的记录继续给出。
    #[test]
    fn continues_after_one_side_is_exhausted() {
        assert_eq!(
            merged(&[("a", "1")], &[("b", "2"), ("c", "3")]),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
        assert_eq!(
            merged(&[("b", "2"), ("c", "3")], &[("a", "1")]),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // 最后一条记录恰好两边都有：给出 a 的之后游标无效，不会再给出 b 的。
    #[test]
    fn same_key_at_the_end() {
        let result = merged(&[("a", "1"), ("z", "A")], &[("z", "B")]);

        assert_eq!(result, expected(&[("a", "1"), ("z", "A")]));
    }

    // key 和 value 始终来自同一条记录。
    #[test]
    fn key_and_value_come_from_the_same_record() {
        let mut iter = TwoMergeIterator::new(
            VecIterator::from_strs(&[("b", "from-a")]),
            VecIterator::from_strs(&[("a", "from-b"), ("c", "from-b")]),
        )
        .unwrap();

        assert_eq!((iter.key(), iter.value()), (&b"a"[..], &b"from-b"[..]));
        iter.next().unwrap();
        assert_eq!((iter.key(), iter.value()), (&b"b"[..], &b"from-a"[..]));
        iter.next().unwrap();
        assert_eq!((iter.key(), iter.value()), (&b"c"[..], &b"from-b"[..]));
        iter.next().unwrap();
        assert!(!iter.is_valid());
    }

    // 两百组随机的输入：结果与"先放 b、再用 a 覆盖"得到的有序表一致。
    #[test]
    fn matches_reference_model_on_random_inputs() {
        // 一个很小的伪随机数发生器，测试的结果因此可以重现。
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next_random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for round in 0..200 {
            let mut sides: Vec<BTreeMap<String, String>> = vec![BTreeMap::new(), BTreeMap::new()];
            for (side_index, side) in sides.iter_mut().enumerate() {
                let record_count = next_random() % 12;
                for _ in 0..record_count {
                    let key = format!("k{:02}", next_random() % 15);
                    // 四分之一的记录是墓碑。
                    let value = if next_random() % 4 == 0 {
                        String::new()
                    } else {
                        format!("side{side_index}-{}", next_random() % 100)
                    };
                    side.insert(key, value);
                }
            }

            let mut model = sides[1].clone();
            for (key, value) in &sides[0] {
                model.insert(key.clone(), value.clone());
            }
            let expected_records: Vec<(String, String)> = model.into_iter().collect();

            let as_pairs = |side: &BTreeMap<String, String>| -> Vec<(String, String)> {
                side.iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            };
            let a_owned = as_pairs(&sides[0]);
            let b_owned = as_pairs(&sides[1]);
            let a_pairs: Vec<(&str, &str)> = a_owned
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            let b_pairs: Vec<(&str, &str)> = b_owned
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();

            assert_eq!(
                merged(&a_pairs, &b_pairs),
                expected_records,
                "第 {round} 组"
            );
        }
    }
}
