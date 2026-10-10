//! 引擎的统计计数器。
//!
//! 写放大与读放大的分子只有引擎自己知道，所以每个写文件、读文件的位置
//! 都要往这里累加。计数器之间互相独立，各自用 `Relaxed` 递增即可。

use std::sync::atomic::{AtomicU64, Ordering};

/// 引擎运行期间一直累加的计数器，可以被多个线程同时更新。
#[derive(Debug, Default)]
pub struct Stats {
    user_bytes_written: AtomicU64,
    wal_bytes_written: AtomicU64,
    flush_bytes_written: AtomicU64,
    compaction_bytes_written: AtomicU64,
    gets: AtomicU64,
    tables_probed: AtomicU64,
    blocks_read: AtomicU64,
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用户交给引擎的字节：每次写入的 key 长度加 value 长度。
    pub fn add_user_bytes_written(&self, bytes: u64) {
        self.user_bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 追加到 WAL 文件的字节。
    pub fn add_wal_bytes_written(&self, bytes: u64) {
        self.wal_bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// flush 写出的 SST 字节。
    pub fn add_flush_bytes_written(&self, bytes: u64) {
        self.flush_bytes_written.fetch_add(bytes, Ordering::Relaxed);
    }

    /// compaction 写出的 SST 字节。
    pub fn add_compaction_bytes_written(&self, bytes: u64) {
        self.compaction_bytes_written
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// 一次点查。
    pub fn add_get(&self) {
        self.gets.fetch_add(1, Ordering::Relaxed);
    }

    /// 点查中实际进入查找的 SST 个数（被 key 范围或 Bloom filter 排除的不算）。
    pub fn add_tables_probed(&self, tables: u64) {
        self.tables_probed.fetch_add(tables, Ordering::Relaxed);
    }

    /// 从文件读出的数据块个数（命中 block cache 的不算）。
    pub fn add_blocks_read(&self, blocks: u64) {
        self.blocks_read.fetch_add(blocks, Ordering::Relaxed);
    }

    /// 读出当前各计数器的值。
    ///
    /// 各计数器是分别读取的，有其他线程在写入时，读到的几个值不保证
    /// 对应同一时刻。基准驱动在一段负载结束、后台工作停下后再读。
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            user_bytes_written: self.user_bytes_written.load(Ordering::Relaxed),
            wal_bytes_written: self.wal_bytes_written.load(Ordering::Relaxed),
            flush_bytes_written: self.flush_bytes_written.load(Ordering::Relaxed),
            compaction_bytes_written: self.compaction_bytes_written.load(Ordering::Relaxed),
            gets: self.gets.load(Ordering::Relaxed),
            tables_probed: self.tables_probed.load(Ordering::Relaxed),
            blocks_read: self.blocks_read.load(Ordering::Relaxed),
        }
    }
}

/// 某一时刻读出的计数器值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub user_bytes_written: u64,
    pub wal_bytes_written: u64,
    pub flush_bytes_written: u64,
    pub compaction_bytes_written: u64,
    pub gets: u64,
    pub tables_probed: u64,
    pub blocks_read: u64,
}

impl StatsSnapshot {
    /// 两次读数之间的增量，用来单独统计一段负载。
    pub fn since(&self, earlier: &StatsSnapshot) -> StatsSnapshot {
        StatsSnapshot {
            user_bytes_written: self.user_bytes_written - earlier.user_bytes_written,
            wal_bytes_written: self.wal_bytes_written - earlier.wal_bytes_written,
            flush_bytes_written: self.flush_bytes_written - earlier.flush_bytes_written,
            compaction_bytes_written: self.compaction_bytes_written
                - earlier.compaction_bytes_written,
            gets: self.gets - earlier.gets,
            tables_probed: self.tables_probed - earlier.tables_probed,
            blocks_read: self.blocks_read - earlier.blocks_read,
        }
    }

    /// 引擎写到磁盘的全部字节：WAL、flush、compaction 三项之和。
    pub fn disk_bytes_written(&self) -> u64 {
        self.wal_bytes_written + self.flush_bytes_written + self.compaction_bytes_written
    }

    /// 写放大：写到磁盘的字节除以用户写入的字节。用户没有写入时无定义。
    pub fn write_amplification(&self) -> Option<f64> {
        if self.user_bytes_written == 0 {
            return None;
        }
        let disk_bytes = self.disk_bytes_written() as f64;
        let user_bytes = self.user_bytes_written as f64;
        Some(disk_bytes / user_bytes)
    }

    /// 平均每次点查进入了几个 SST。没有点查时无定义。
    pub fn tables_probed_per_get(&self) -> Option<f64> {
        if self.gets == 0 {
            return None;
        }
        Some(self.tables_probed as f64 / self.gets as f64)
    }

    /// 平均每次点查从文件读了几个数据块。没有点查时无定义。
    pub fn blocks_read_per_get(&self) -> Option<f64> {
        if self.gets == 0 {
            return None;
        }
        Some(self.blocks_read as f64 / self.gets as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    // 刚创建时所有计数器为 0；分母为 0 的比值（写放大、读放大）返回 None。
    #[test]
    fn new_stats_are_zero_and_ratios_undefined() {
        let stats = Stats::new();
        let snapshot = stats.snapshot();

        assert_eq!(snapshot, StatsSnapshot::default());
        assert_eq!(snapshot.write_amplification(), None);
        assert_eq!(snapshot.tables_probed_per_get(), None);
        assert_eq!(snapshot.blocks_read_per_get(), None);
    }

    // 写放大的分子是 WAL、flush、compaction 三项之和。
    #[test]
    fn write_amplification_counts_wal_flush_and_compaction() {
        let stats = Stats::new();
        stats.add_user_bytes_written(100);
        stats.add_wal_bytes_written(110);
        stats.add_flush_bytes_written(90);
        stats.add_compaction_bytes_written(200);

        let snapshot = stats.snapshot();

        assert_eq!(snapshot.disk_bytes_written(), 400);
        assert_eq!(snapshot.write_amplification(), Some(4.0));
    }

    // 读放大是总数除以点查次数得到的平均值。
    #[test]
    fn read_amplification_is_averaged_over_gets() {
        let stats = Stats::new();
        for _ in 0..4 {
            stats.add_get();
        }
        stats.add_tables_probed(6);
        stats.add_blocks_read(2);

        let snapshot = stats.snapshot();

        assert_eq!(snapshot.tables_probed_per_get(), Some(1.5));
        assert_eq!(snapshot.blocks_read_per_get(), Some(0.5));
    }

    // 两次读数相减，得到的只是中间那一段负载的量。
    #[test]
    fn since_isolates_one_workload() {
        let stats = Stats::new();
        stats.add_user_bytes_written(100);
        stats.add_wal_bytes_written(100);
        let before = stats.snapshot();

        stats.add_user_bytes_written(50);
        stats.add_wal_bytes_written(50);
        stats.add_compaction_bytes_written(100);
        let after = stats.snapshot();

        let delta = after.since(&before);
        assert_eq!(delta.user_bytes_written, 50);
        assert_eq!(delta.disk_bytes_written(), 150);
        assert_eq!(delta.write_amplification(), Some(3.0));
    }

    // 多个线程同时累加，最后的总数一次也不少。
    #[test]
    fn concurrent_updates_are_not_lost() {
        let stats = Arc::new(Stats::new());
        let threads = 8;
        let updates_per_thread = 10_000;

        let mut handles = Vec::new();
        for _ in 0..threads {
            let stats = Arc::clone(&stats);
            handles.push(thread::spawn(move || {
                for _ in 0..updates_per_thread {
                    stats.add_get();
                    stats.add_user_bytes_written(3);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.gets, threads * updates_per_thread);
        assert_eq!(
            snapshot.user_bytes_written,
            threads * updates_per_thread * 3
        );
    }
}
