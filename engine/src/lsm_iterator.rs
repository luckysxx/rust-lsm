//! 对外的游标：把引擎内部的记录流变成用户看到的结果。
//!
//! 里面一层（归并）给出的记录已经按 key 有序、每个 key 只有最新的一条，但墓碑还在，
//! 也可能越过扫描范围的上界。这一层把墓碑跳过去，并在上界处停下，所以用户读到的
//! 每一条都是范围内真实存在的 key 和它的值。

use std::ops::Bound;

use bytes::Bytes;

use crate::error::Result;
use crate::iterators::StorageIterator;

/// 跳过墓碑、到上界为止的游标。`I` 是里面那层游标的类型。
///
/// 不变量：只要游标有效，它指着的就不是墓碑，并且没有越过上界。
///
/// 上界放在这一层统一处理：memtable 的游标自己会在上界处停下，SST 的游标不会，
/// 它只管从某个 key 起一直往后走。
pub struct LsmIterator<I: StorageIterator> {
    inner: I,
    /// 扫描范围的上界。
    upper: Bound<Bytes>,
}

impl<I: StorageIterator> LsmIterator<I> {
    /// 包在 `inner` 外面，`upper` 是扫描范围的上界。
    ///
    /// 跳过开头的墓碑时里面那层可能要前进，所以创建也可能失败。
    pub fn new(inner: I, upper: Bound<Bytes>) -> Result<Self> {
        let mut iter = Self { inner, upper };
        // 里面那层一开始就可能指着墓碑，先跳过，建立不变量。
        iter.skip_tombstones()?;
        Ok(iter)
    }

    /// 里面那层是否还指着范围内的一条记录。
    ///
    /// key 是递增的，一旦越过上界，之后的记录也都在范围外。
    fn inner_is_in_range(&self) -> bool {
        if !self.inner.is_valid() {
            return false;
        }
        match &self.upper {
            Bound::Included(upper) => self.inner.key() <= &upper[..],
            Bound::Excluded(upper) => self.inner.key() < &upper[..],
            Bound::Unbounded => true,
        }
    }

    /// 如果当前指着墓碑，就一直前进到第一条不是墓碑的记录，或者走出范围、走到头。
    /// 当前不是墓碑时什么都不做。
    fn skip_tombstones(&mut self) -> Result<()> {
        // 先确认还在范围内再读 value：`&&` 在前一个条件为假时不会去算后一个。
        while self.inner_is_in_range() && self.inner.value().is_empty() {
            self.inner.next()?;
        }
        Ok(())
    }
}

impl<I: StorageIterator> StorageIterator for LsmIterator<I> {
    fn key(&self) -> &[u8] {
        self.inner.key()
    }

    fn value(&self) -> &[u8] {
        self.inner.value()
    }

    fn is_valid(&self) -> bool {
        self.inner_is_in_range()
    }

    fn next(&mut self) -> Result<()> {
        self.inner.next()?;
        self.skip_tombstones()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterators::vec_iterator::{VecIterator, collect_all, expected};

    /// 在一串记录外面包上这一层，收集它给出的全部结果。
    fn visible(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut iter = LsmIterator::new(VecIterator::from_strs(pairs), Bound::Unbounded).unwrap();
        collect_all(&mut iter)
    }

    // 没有墓碑时原样给出。
    #[test]
    fn records_without_tombstones_pass_through() {
        let result = visible(&[("a", "1"), ("b", "2")]);

        assert_eq!(result, expected(&[("a", "1"), ("b", "2")]));
    }

    // 中间的墓碑被跳过，它前后的记录不受影响。
    #[test]
    fn tombstone_in_the_middle_is_skipped() {
        let result = visible(&[("a", "1"), ("b", ""), ("c", "3")]);

        assert_eq!(result, expected(&[("a", "1"), ("c", "3")]));
    }

    // 开头就是墓碑：游标一创建好，指着的就已经是第一条真实的记录。
    #[test]
    fn leading_tombstone_is_skipped_at_creation() {
        let iter = LsmIterator::new(
            VecIterator::from_strs(&[("a", ""), ("b", "2")]),
            Bound::Unbounded,
        )
        .unwrap();

        assert!(iter.is_valid());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"2");
    }

    // 连续的几条墓碑一次全部跳过。
    #[test]
    fn consecutive_tombstones_are_all_skipped() {
        let result = visible(&[("a", "1"), ("b", ""), ("c", ""), ("d", ""), ("e", "5")]);

        assert_eq!(result, expected(&[("a", "1"), ("e", "5")]));
    }

    // 末尾的墓碑被跳过之后，游标变为无效，而不是停在墓碑上。
    #[test]
    fn trailing_tombstones_end_the_iteration() {
        let result = visible(&[("a", "1"), ("b", ""), ("c", "")]);

        assert_eq!(result, expected(&[("a", "1")]));
    }

    // 全是墓碑，或者根本没有记录：游标一开始就是无效的。
    #[test]
    fn only_tombstones_or_nothing_is_invalid_from_the_start() {
        let only_tombstones = LsmIterator::new(
            VecIterator::from_strs(&[("a", ""), ("b", "")]),
            Bound::Unbounded,
        )
        .unwrap();
        let nothing = LsmIterator::new(VecIterator::from_strs(&[]), Bound::Unbounded).unwrap();

        assert!(!only_tombstones.is_valid());
        assert!(!nothing.is_valid());
    }

    /// 在一串记录外面包上这一层并给定上界，收集它给出的全部结果。
    fn visible_up_to(pairs: &[(&str, &str)], upper: Bound<&str>) -> Vec<(String, String)> {
        let upper = match upper {
            Bound::Included(key) => Bound::Included(Bytes::copy_from_slice(key.as_bytes())),
            Bound::Excluded(key) => Bound::Excluded(Bytes::copy_from_slice(key.as_bytes())),
            Bound::Unbounded => Bound::Unbounded,
        };
        let mut iter = LsmIterator::new(VecIterator::from_strs(pairs), upper).unwrap();
        collect_all(&mut iter)
    }

    // 上界不包含时在这个 key 之前停下，包含时把它也给出。
    #[test]
    fn upper_bound_stops_the_iteration() {
        let pairs = [("a", "1"), ("c", "3"), ("e", "5")];

        assert_eq!(
            visible_up_to(&pairs, Bound::Excluded("c")),
            expected(&[("a", "1")])
        );
        assert_eq!(
            visible_up_to(&pairs, Bound::Included("c")),
            expected(&[("a", "1"), ("c", "3")])
        );
        assert_eq!(
            visible_up_to(&pairs, Bound::Unbounded),
            expected(&[("a", "1"), ("c", "3"), ("e", "5")])
        );
    }

    // 上界落在两条记录之间：给出它之前的全部记录。
    #[test]
    fn upper_bound_between_keys() {
        let pairs = [("a", "1"), ("c", "3"), ("e", "5")];

        assert_eq!(
            visible_up_to(&pairs, Bound::Excluded("d")),
            expected(&[("a", "1"), ("c", "3")])
        );
        assert_eq!(
            visible_up_to(&pairs, Bound::Included("d")),
            expected(&[("a", "1"), ("c", "3")])
        );
    }

    // 第一条记录就在上界之外：游标一开始就是无效的。
    #[test]
    fn everything_beyond_upper_bound_is_invalid() {
        let pairs = [("m", "1"), ("n", "2")];

        assert_eq!(visible_up_to(&pairs, Bound::Excluded("m")), expected(&[]));
        assert_eq!(visible_up_to(&pairs, Bound::Included("a")), expected(&[]));
    }

    // 范围内最后几条是墓碑：跳过它们时走到了上界之外，游标无效，
    // 不会把上界之外的记录给出去。
    #[test]
    fn tombstones_before_upper_bound_do_not_leak_later_records() {
        let pairs = [("a", "1"), ("b", ""), ("c", ""), ("d", "4")];

        assert_eq!(
            visible_up_to(&pairs, Bound::Excluded("d")),
            expected(&[("a", "1")])
        );
        assert_eq!(
            visible_up_to(&pairs, Bound::Included("c")),
            expected(&[("a", "1")])
        );
    }

    // 上界处恰好是一条墓碑：包含上界也不给出它。
    #[test]
    fn tombstone_at_included_upper_bound_is_skipped() {
        let pairs = [("a", "1"), ("b", "")];

        assert_eq!(
            visible_up_to(&pairs, Bound::Included("b")),
            expected(&[("a", "1")])
        );
    }
}
