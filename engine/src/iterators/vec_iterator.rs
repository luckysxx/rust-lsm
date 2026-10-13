//! 由内存里的一串记录做成的游标，只在测试中使用。
//!
//! 它是最简单的一种来源：记录事先排好序放在一个数组里，游标就是数组的下标。
//! 测试归并等上层游标时，用它代替真正的 memtable，想要什么内容就写什么内容。

use bytes::Bytes;

use super::StorageIterator;
use crate::error::Result;

pub struct VecIterator {
    /// 全部记录，每一项是（key，value），必须已经按 key 从小到大排好。
    records: Vec<(Bytes, Bytes)>,
    /// 游标现在指着第几条。等于 `records.len()` 表示已经走到头。
    index: usize,
}

impl VecIterator {
    pub fn new(records: Vec<(Bytes, Bytes)>) -> Self {
        Self { records, index: 0 }
    }

    /// 用字符串写出记录，测试里读起来清楚。空字符串的 value 表示墓碑。
    pub fn from_strs(pairs: &[(&str, &str)]) -> Self {
        let mut records = Vec::new();
        for (key, value) in pairs {
            let key = Bytes::copy_from_slice(key.as_bytes());
            let value = Bytes::copy_from_slice(value.as_bytes());
            records.push((key, value));
        }
        Self::new(records)
    }
}

impl StorageIterator for VecIterator {
    fn key(&self) -> &[u8] {
        assert!(self.is_valid(), "游标无效时不能读取 key");
        &self.records[self.index].0
    }

    fn value(&self) -> &[u8] {
        assert!(self.is_valid(), "游标无效时不能读取 value");
        &self.records[self.index].1
    }

    fn is_valid(&self) -> bool {
        self.index < self.records.len()
    }

    fn next(&mut self) -> Result<()> {
        self.index += 1;
        Ok(())
    }
}

/// 把一个游标从当前位置走到头，收集沿途的记录。测试里用来和预期结果比较。
///
/// 游标如果不前进，`while` 会永远转下去；这里收集到一万条就让测试失败，
/// 把"卡死"变成一条看得懂的报错。
pub fn collect_all(iter: &mut impl StorageIterator) -> Vec<(String, String)> {
    let mut collected = Vec::new();
    while iter.is_valid() {
        assert!(
            collected.len() < 10_000,
            "游标走了一万步还没有结束，next 很可能没有前进"
        );
        let key = String::from_utf8_lossy(iter.key()).into_owned();
        let value = String::from_utf8_lossy(iter.value()).into_owned();
        collected.push((key, value));
        iter.next().expect("游标前进时出错");
    }
    collected
}

/// 把预期结果写成与 `collect_all` 的返回值相同的形状。
pub fn expected(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 没有记录的来源，游标一开始就是无效的。
    #[test]
    fn empty_source_is_invalid_from_the_start() {
        let iter = VecIterator::from_strs(&[]);

        assert!(!iter.is_valid());
    }

    // 刚创建的游标指着第一条记录。
    #[test]
    fn new_iterator_points_at_first_record() {
        let iter = VecIterator::from_strs(&[("a", "1"), ("b", "2")]);

        assert!(iter.is_valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"1");
    }

    // 每调用一次 next 前进一条；key 和 value 始终来自同一条记录。
    #[test]
    fn next_moves_to_the_following_record() {
        let mut iter = VecIterator::from_strs(&[("a", "1"), ("b", "2"), ("c", "3")]);

        iter.next().unwrap();
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"2");

        iter.next().unwrap();
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"3");
    }

    // 走过最后一条之后游标变为无效。
    #[test]
    fn iterator_becomes_invalid_after_last_record() {
        let mut iter = VecIterator::from_strs(&[("a", "1")]);
        assert!(iter.is_valid());

        iter.next().unwrap();

        assert!(!iter.is_valid());
    }

    // 墓碑（空 value）原样给出，这一层不过滤。
    #[test]
    fn tombstones_are_passed_through() {
        let mut iter = VecIterator::from_strs(&[("a", "1"), ("b", ""), ("c", "3")]);

        let collected = collect_all(&mut iter);

        assert_eq!(collected, expected(&[("a", "1"), ("b", ""), ("c", "3")]));
    }
}
