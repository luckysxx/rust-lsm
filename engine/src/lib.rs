//! 学习用的 LSM 存储引擎。
//!
//! 对外的入口是 `db::Db`：打开一个目录，写入、删除、点查、扫描，用完关闭。
//! 数据先进 memtable，写满后冻结，由后台线程写成 SST 文件放进 L0。
//! 目前没有 WAL 和 manifest：重新打开不会读回已有的文件。
//! compaction 等按知识点顺序加入，见 `lsm/notes/README.md`。

pub mod block;
pub mod db;
pub mod error;
pub mod iterators;
pub mod lsm_iterator;
pub mod lsm_storage;
pub mod mem_table;
pub mod stats;
pub mod table;
#[cfg(test)]
pub mod test_util;
