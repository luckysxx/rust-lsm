//! 游标的统一接口。
//!
//! 一个游标指着某个有序来源里的一条记录，可以看它、也可以前进到下一条。
//! memtable 的游标、之后 SST 的游标，以及把多个来源归并成一个的游标都实现
//! 这个接口，所以它们可以一层套一层。
//!
//! 实现分两类，看类型有没有 `<I: StorageIterator>` 这个参数就能分辨：
//!
//! - 直接读数据的（没有参数），和它读的数据放在一起：
//!   `mem_table::MemTableIterator`、`block::BlockIterator`、`table::SsTableIterator`，
//!   以及只在测试里用的 `vec_iterator::VecIterator`。
//! - 包着别的游标、对它们的结果做加工的（有参数 `I`，就是里面那层的类型）：
//!   `merge_iterator::MergeIterator<I>`（多个同一种类型的合成一个）、
//!   `two_merge_iterator::TwoMergeIterator<A, B>`（两个可以不同类型的合成一个）、
//!   `lsm_iterator::LsmIterator<I>`（跳过墓碑，到上界为止）。

pub mod merge_iterator;
pub mod two_merge_iterator;
#[cfg(test)]
pub mod vec_iterator;

use crate::error::Result;

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
    ///
    /// 读磁盘的游标前进时可能要读下一个块，所以会失败。返回错误之后游标的状态
    /// 不确定，调用方不能再使用它。
    fn next(&mut self) -> Result<()>;
}
