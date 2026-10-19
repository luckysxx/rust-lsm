//! compaction 的策略：看一份状态，决定要不要合并、合并哪些文件。
//!
//! 这里只做决定：输入是一份状态，输出是一个任务（或者"没事可做"），不写文件、不拿锁、
//! 不改状态。按任务读文件、写新文件、换状态的是 `lsm_storage` 的 `compact` 子模块。
//! 两边分开，策略就能不碰磁盘、只用手工搭的状态来测试；换一种策略也不必改执行的代码。
//!
//! 每种策略一个子模块。

pub mod leveled;
pub mod simple_leveled;
pub mod tiered;

use crate::table::SsTable;
use leveled::LeveledOptions;
use simple_leveled::SimpleLeveledOptions;
use std::sync::Arc;
use tiered::TieredOptions;

/// 后台自动合并用哪种策略，以及它的参数。打开引擎时选定，之后不变。
///
/// 各种策略对状态的理解不同，同一个引擎里只能用一种：简单分层与 leveled 用 L0 加固定数目的
/// 几层，数据往下沉；tiered 不用 L0，`levels` 是一列按新旧排列、个数会变的 tier。
#[derive(Debug, Clone)]
pub enum CompactionOptions {
    /// 不自动合并：不起后台的 compaction 线程，只有手动调用 `force_full_compaction`
    /// 时才合并。状态的形状与简单分层相同。
    NoCompaction,
    /// 简单分层。
    SimpleLeveled(SimpleLeveledOptions),
    /// tiered。
    Tiered(TieredOptions),
    /// leveled：按字节定每层的目标、从最底层往上建层，每次只把上层的一个文件并进下层。
    Leveled(LeveledOptions),
}

impl CompactionOptions {
    /// flush 出的 SST 放进 L0（`true`），还是在 `levels` 的最前面成为一个新的 tier（`false`）。
    /// 只有 tiered 不用 L0。
    pub fn flush_to_l0(&self) -> bool {
        !matches!(self, CompactionOptions::Tiered(_))
    }
}

/// 一组 SST 是不是一个 sorted run：按 key 从小到大排好，相邻两个的范围不重叠
/// （前一个的末 key 严格小于后一个的首 key）。空的、只有一个的也算。
///
/// 策略用它判断几个文件能不能一起挪动，执行用它检查换状态之后的层。
pub(crate) fn is_sorted_run(ssts: &[Arc<SsTable>]) -> bool {
    ssts.windows(2)
        .all(|pair| pair[0].last_key() < pair[1].first_key())
}

/// 一组 SST 按首 key 从小到大排好的副本（复制的是 `Arc`），传进来的列表不变。
///
/// L0 的列表是新的在前，不是按 key 排的：要判断几个 L0 文件彼此重不重叠、
/// 或者把它们一起挪进下层，先用它按 key 排好。
pub(crate) fn sorted_by_first_key(ssts: &[Arc<SsTable>]) -> Vec<Arc<SsTable>> {
    let mut sorted_ssts: Vec<Arc<SsTable>> = ssts.to_vec();
    sorted_ssts.sort_by(|a, b| a.first_key().cmp(b.first_key()));
    sorted_ssts
}
