//! 对外的游标：把引擎内部的记录流变成用户看到的结果。
//!
//! 里面一层（归并）给出的记录已经按 key 有序、每个 key 只有最新的一条，但墓碑还在。
//! 这一层把墓碑跳过去，所以用户读到的每一条都是真实存在的 key 和它的值。

use crate::iterators::StorageIterator;

/// 跳过墓碑的游标。`I` 是里面那层游标的类型。
///
/// 不变量：只要游标有效，它指着的就不是墓碑。
pub struct LsmIterator<I: StorageIterator> {
    inner: I,
}

impl<I: StorageIterator> LsmIterator<I> {
    pub fn new(inner: I) -> Self {
        let mut iter = Self { inner };
        // 里面那层一开始就可能指着墓碑，先跳过，建立不变量。
        iter.skip_tombstones();
        iter
    }

    /// 如果当前指着墓碑，就一直前进到第一条不是墓碑的记录，或者走到头。
    /// 当前不是墓碑时什么都不做。
    fn skip_tombstones(&mut self) {
        // 先确认有效再读 value：`&&` 在前一个条件为假时不会去算后一个。
        while self.inner.is_valid() && self.inner.value().is_empty() {
            self.inner.next();
        }
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
        self.inner.is_valid()
    }

    fn next(&mut self) {
        self.inner.next();
        self.skip_tombstones();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterators::vec_iterator::{VecIterator, collect_all, expected};

    /// 在一串记录外面包上这一层，收集它给出的全部结果。
    fn visible(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut iter = LsmIterator::new(VecIterator::from_strs(pairs));
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
        let iter = LsmIterator::new(VecIterator::from_strs(&[("a", ""), ("b", "2")]));

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
        let only_tombstones = LsmIterator::new(VecIterator::from_strs(&[("a", ""), ("b", "")]));
        let nothing = LsmIterator::new(VecIterator::from_strs(&[]));

        assert!(!only_tombstones.is_valid());
        assert!(!nothing.is_valid());
    }
}
