//! 游标的统一接口。
//!
//! 一个游标指着某个有序来源里的一条记录，可以看它、也可以前进到下一条。
//! memtable 的游标、之后 SST 的游标，以及把多个来源归并成一个的游标都实现
//! 这个接口，所以它们可以一层套一层。

pub mod merge_iterator;
#[cfg(test)]
pub mod vec_iterator;

pub trait StorageIterator {
    /// 当前记录的 key。
    ///
    /// 只在 `is_valid()` 为真时可以调用。返回的字节借自游标，
    /// 下一次调用 `next` 之后不能再使用。
    fn key(&self) -> &[u8];

    /// 当前记录的 value，空表示这是一条墓碑。
    ///
    /// 只在 `is_valid()` 为真时可以调用。返回的字节借自游标，
    /// 下一次调用 `next` 之后不能再使用。
    fn value(&self) -> &[u8];

    /// 游标是否指着一条记录。
    ///
    /// 来源里没有记录，或者已经走过最后一条时为假；此后一直为假。
    fn is_valid(&self) -> bool;

    /// 前进到下一条记录；没有下一条时游标变为无效。
    ///
    /// 只在 `is_valid()` 为真时可以调用。
    fn next(&mut self);
}
