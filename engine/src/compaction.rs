//! compaction 的策略：看一份状态，决定要不要合并、合并哪些文件。
//!
//! 这里只做决定：输入是一份状态，输出是一个任务（或者"没事可做"），不写文件、不拿锁、
//! 不改状态。按任务读文件、写新文件、换状态的是 `lsm_storage` 的 `compact` 子模块。
//! 两边分开，策略就能不碰磁盘、只用手工搭的状态来测试；换一种策略也不必改执行的代码。
//!
//! 每种策略一个子模块。

pub mod simple_leveled;
pub mod tiered;

use simple_leveled::SimpleLeveledOptions;
use tiered::TieredOptions;

/// 后台自动合并用哪种策略，以及它的参数。打开引擎时选定，之后不变。
///
/// 两种策略对状态的理解不同，同一个引擎里只能用一种：简单分层用 L0 加固定数目的几层，
/// 数据一层层往下沉；tiered 不用 L0，`levels` 是一列按新旧排列、个数会变的 tier。
#[derive(Debug, Clone)]
pub enum CompactionOptions {
    /// 不自动合并：不起后台的 compaction 线程，只有手动调用 `force_full_compaction`
    /// 时才合并。状态的形状与简单分层相同。
    NoCompaction,
    /// 简单分层。
    SimpleLeveled(SimpleLeveledOptions),
    /// tiered。
    Tiered(TieredOptions),
}

impl CompactionOptions {
    /// flush 出的 SST 放进 L0（`true`），还是在 `levels` 的最前面成为一个新的 tier（`false`）。
    /// 只有 tiered 不用 L0。
    pub fn flush_to_l0(&self) -> bool {
        !matches!(self, CompactionOptions::Tiered(_))
    }
}
