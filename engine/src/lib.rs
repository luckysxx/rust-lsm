//! 学习用的 LSM 存储引擎。
//!
//! 目前只有统计计数器；memtable、SST、compaction 等按知识点顺序加入，
//! 见 `lsm/notes/README.md`。

pub mod stats;
