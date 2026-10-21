//! 在数据库上执行一条命令，把结果写给用户。

use std::io::Write;
use std::ops::Bound;
use std::path::Path;

use lsm::db::Db;
use lsm::iterators::StorageIterator;

use crate::command::{Command, HELP};

/// 在 `db` 上执行一条命令，把要给用户看的结果写进 `out`。
///
/// `dir` 是数据库所在的目录，`status` 用它列出 SST 文件。
/// `Quit` 由调用方处理，这里什么都不做。
pub fn execute(db: &Db, dir: &Path, command: &Command, out: &mut impl Write) -> anyhow::Result<()> {
    match command {
        Command::Put { key, value } => {
            db.put(key.as_bytes(), value.as_bytes())?;
            writeln!(out, "OK")?;
        }
        Command::Get { key } => {
            let value = db.get(key.as_bytes())?;
            match value {
                Some(value) => {
                    writeln!(out, "{}", String::from_utf8_lossy(&value))?;
                }
                None => {
                    writeln!(out, "(不存在)")?;
                }
            }
        }
        Command::Delete { key } => {
            db.delete(key.as_bytes())?;
            writeln!(out, "OK")?;
        }
        Command::Scan { lower, upper } => {
            let lower_bound = match lower {
                Some(lower) => Bound::Included(lower.as_bytes()),
                None => Bound::Unbounded,
            };
            let upper_bound = match upper {
                Some(upper) => Bound::Excluded(upper.as_bytes()),
                None => Bound::Unbounded,
            };
            let mut iter = db.scan(lower_bound, upper_bound)?;
            let mut count = 0;
            while iter.is_valid() {
                let key = String::from_utf8_lossy(iter.key());
                let value = String::from_utf8_lossy(iter.value());
                writeln!(out, "{key} = {value}")?;
                count += 1;
                iter.next()?;
            }
            writeln!(out, "(共 {count} 条)")?;
        }
        Command::Fill { count } => {
            for i in 0..*count {
                let key = format!("key-{i:06}");
                let value = format!("value-{i:06}");
                db.put(key.as_bytes(), value.as_bytes())?;
            }
            writeln!(out, "写入 {count} 条")?;
        }
        Command::Compact => {
            db.force_full_compaction()?;
            if db.uses_tiers() {
                writeln!(
                    out,
                    "合并完成：各 tier（新的在前）依次有 {} 个 SST",
                    join_counts(&db.level_sstable_counts())
                )?;
            } else {
                writeln!(
                    out,
                    "合并完成：L0 有 {} 个 SST，L1 往下各层依次有 {} 个",
                    db.l0_sstable_count(),
                    join_counts(&db.level_sstable_counts())
                )?;
            }
        }
        Command::Status => {
            writeln!(out, "冻结的 memtable：{}", db.imm_memtable_count())?;
            if db.uses_tiers() {
                // tiered 不用 L0；第 0 项是最新的 tier。
                for (index, count) in db.level_sstable_counts().iter().enumerate() {
                    let tier = index + 1;
                    writeln!(out, "tier {tier} 的 SST：{count}")?;
                }
            } else {
                writeln!(out, "L0 的 SST：{}", db.l0_sstable_count())?;
                for (index, count) in db.level_sstable_counts().iter().enumerate() {
                    // 第 0 项是 L1。
                    let level = index + 1;
                    writeln!(out, "L{level} 的 SST：{count}")?;
                }
            }
            let (cached_blocks, cached_bytes) = db.block_cache_usage();
            writeln!(out, "块缓存：{cached_blocks} 个块，{cached_bytes} 字节")?;
            let files = files_with_suffix(dir, ".sst")?;
            writeln!(out, "目录里的 SST 文件：{}", files.len())?;
            for (name, size) in files {
                writeln!(out, "  {name}  {size} 字节")?;
            }
            // 每张还在内存里的表一个 WAL；表写成 SST 之后它的 WAL 被删掉。
            let wals = files_with_suffix(dir, ".wal")?;
            writeln!(out, "目录里的 WAL 文件：{}", wals.len())?;
            for (name, size) in wals {
                writeln!(out, "  {name}  {size} 字节")?;
            }
        }
        Command::Sync => {
            db.sync()?;
            writeln!(out, "OK")?;
        }
        Command::Help => writeln!(out, "{HELP}")?,
        // 由调用方处理：一个要结束读输入的循环，一个要结束整个进程。
        Command::Quit | Command::Crash => {}
    }
    Ok(())
}

/// 把各层的文件数写成 `0 / 2 / 5` 的样子。
fn join_counts(counts: &[usize]) -> String {
    let texts: Vec<String> = counts.iter().map(|count| count.to_string()).collect();
    texts.join(" / ")
}

/// 目录里文件名以 `suffix` 结尾的文件（例如 `.sst`）：文件名与长度（字节），按文件名排序。
pub fn files_with_suffix(dir: &Path, suffix: &str) -> std::io::Result<Vec<(String, u64)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(suffix) {
            let size = entry.metadata()?.len();
            files.push((name, size));
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse;
    use lsm::compaction::CompactionOptions;
    use lsm::compaction::tiered::TieredOptions;
    use lsm::lsm_storage::LsmStorageOptions;

    /// 收集输出的缓冲区，最多收下 `limit` 字节，再写就返回错误。
    ///
    /// 扫描的循环忘了前进时，输出会无限增长；有了上限，测试很快失败而不是卡住。
    struct LimitedOutput {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Write for LimitedOutput {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.bytes.len() + buf.len() > self.limit {
                return Err(std::io::Error::other(
                    "输出超过了上限，很可能是循环没有前进",
                ));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 开在临时目录里的数据库。目录随它一起清理。
    struct TestDb {
        db: Db,
        dir: tempfile::TempDir,
    }

    impl TestDb {
        /// memtable 写满 `memtable_size_limit` 字节就冻结；不自动合并。
        fn open(memtable_size_limit: usize) -> Self {
            Self::open_with_strategy(memtable_size_limit, CompactionOptions::NoCompaction)
        }

        /// 与 `open` 相同，只是合并策略由调用方给出。
        fn open_with_strategy(memtable_size_limit: usize, compaction: CompactionOptions) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let options = LsmStorageOptions {
                memtable_size_limit,
                block_size: 256,
                block_cache_size: 1 << 20,
                target_sst_size: 4096,
                max_levels: 3,
                compaction,
                enable_wal: true,
            };
            let db = Db::open(dir.path(), options).unwrap();
            Self { db, dir }
        }

        /// 解析并执行一行输入，返回写给用户的全部文字。
        fn run(&self, line: &str) -> String {
            let command = parse(line).unwrap();
            let mut out = LimitedOutput {
                bytes: Vec::new(),
                limit: 64 * 1024,
            };
            execute(&self.db, self.dir.path(), &command, &mut out).unwrap();
            String::from_utf8(out.bytes).unwrap()
        }
    }

    // put 之后 get 得到写入的 value；没写过的 key 显示不存在。
    #[test]
    fn put_then_get() {
        let db = TestDb::open(usize::MAX);

        assert_eq!(db.run("put a 1"), "OK\n");

        assert_eq!(db.run("get a"), "1\n");
        assert_eq!(db.run("get b"), "(不存在)\n");
    }

    // delete 之后 get 显示不存在。
    #[test]
    fn delete_then_get() {
        let db = TestDb::open(usize::MAX);
        db.run("put a 1");

        assert_eq!(db.run("delete a"), "OK\n");

        assert_eq!(db.run("get a"), "(不存在)\n");
    }

    // scan 每条记录一行，最后一行是条数；范围包含下界、不包含上界。
    #[test]
    fn scan_prints_records_in_range() {
        let db = TestDb::open(usize::MAX);
        for key in ["a", "b", "c", "d"] {
            db.run(&format!("put {key} v{key}"));
        }
        db.run("delete c");

        assert_eq!(db.run("scan"), "a = va\nb = vb\nd = vd\n(共 3 条)\n");
        assert_eq!(db.run("scan b"), "b = vb\nd = vd\n(共 2 条)\n");
        assert_eq!(db.run("scan b d"), "b = vb\n(共 1 条)\n");
        assert_eq!(db.run("scan x"), "(共 0 条)\n");
    }

    // fill 写入编了号的记录，编号补零到 6 位，所以 key 的字典序就是编号的顺序。
    #[test]
    fn fill_writes_numbered_records() {
        let db = TestDb::open(usize::MAX);

        assert_eq!(db.run("fill 3"), "写入 3 条\n");

        assert_eq!(
            db.run("scan"),
            "key-000000 = value-000000\nkey-000001 = value-000001\nkey-000002 = value-000002\n(共 3 条)\n"
        );
    }

    // 什么都没写过时 status 全是 0；关闭之后数据全部写成 SST，status 列出文件。
    #[test]
    fn status_shows_counts_and_files() {
        let db = TestDb::open(usize::MAX);
        let status = db.run("status");
        assert!(
            status.starts_with(
                "冻结的 memtable：0\nL0 的 SST：0\nL1 的 SST：0\nL2 的 SST：0\nL3 的 SST：0\n块缓存：0 个块，0 字节\n目录里的 SST 文件：0\n目录里的 WAL 文件：1\n"
            ),
            "{status}"
        );
        db.run("put a 1");

        db.db.close().unwrap();

        let status = db.run("status");
        assert!(
            status.starts_with(
                "冻结的 memtable：0\nL0 的 SST：1\nL1 的 SST：0\nL2 的 SST：0\nL3 的 SST：0\n块缓存：0 个块，0 字节\n目录里的 SST 文件：1\n"
            ),
            "{status}"
        );
        // 文件名是编号，memtable 也占编号，所以不写死是哪一个。
        let file_lines = status.lines().filter(|line| line.contains(".sst ")).count();
        assert_eq!(file_lines, 1, "{status}");
    }

    // compact 把全部文件合并进最底层，旧文件从目录里删掉；数据照常读得到。
    #[test]
    fn compact_merges_everything_into_the_bottom_level() {
        // memtable 上限很小，fill 写出好几个文件；关闭让它们全部落盘、后台线程停下。
        let db = TestDb::open(4096);
        db.run("fill 500");
        db.db.close().unwrap();
        assert!(db.db.l0_sstable_count() > 1);

        let output = db.run("compact");

        let bottom_count = db.db.level_sstable_counts()[2];
        assert_eq!(
            output,
            format!("合并完成：L0 有 0 个 SST，L1 往下各层依次有 0 / 0 / {bottom_count} 个\n")
        );
        assert!(bottom_count > 0);
        assert_eq!(
            files_with_suffix(db.dir.path(), ".sst").unwrap().len(),
            bottom_count
        );
        assert_eq!(db.run("get key-000123"), "value-000123\n");
    }

    // tiered：status 列出各个 tier 而不是 L0 与各层；compact 把全部 tier 合成一个。
    // num_tiers 给得很大，后台不会自己合并，fill 出的每个文件都是一个 tier。
    #[test]
    fn status_and_compact_with_tiered() {
        let tiered = CompactionOptions::Tiered(TieredOptions {
            num_tiers: 1000,
            max_size_amplification_percent: 200,
            size_ratio: 1,
            min_merge_width: 2,
            max_merge_width: None,
        });
        let db = TestDb::open_with_strategy(4096, tiered);
        db.run("fill 500");
        db.db.close().unwrap();
        let tier_count = db.db.level_sstable_counts().len();
        assert!(tier_count > 1);

        let status = db.run("status");
        assert!(!status.contains("L0"), "{status}");
        assert!(status.contains("tier 1 的 SST：1\n"), "{status}");
        assert!(
            status.contains(&format!("tier {tier_count} 的 SST：1\n")),
            "{status}"
        );

        let output = db.run("compact");
        let only_tier_count = db.db.level_sstable_counts()[0];
        assert_eq!(
            output,
            format!("合并完成：各 tier（新的在前）依次有 {only_tier_count} 个 SST\n")
        );
        assert_eq!(db.run("get key-000123"), "value-000123\n");
    }

    // help 打印命令说明。
    #[test]
    fn help_prints_usage() {
        let db = TestDb::open(usize::MAX);

        assert_eq!(db.run("help"), format!("{HELP}\n"));
    }

    // 只列出给定后缀的文件（这里是 .sst），按文件名排序，长度是文件的实际长度。
    #[test]
    fn files_with_suffix_lists_only_matching_files_sorted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("00002.sst"), b"xx").unwrap();
        std::fs::write(dir.path().join("00001.sst"), b"x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();

        let files = files_with_suffix(dir.path(), ".sst").unwrap();

        assert_eq!(
            files,
            vec![("00001.sst".to_string(), 1), ("00002.sst".to_string(), 2)]
        );
    }

    // 写入之后，可写的表的 WAL 变大；sync 回答 OK。
    #[test]
    fn status_shows_the_wal_growing_and_sync_succeeds() {
        let db = TestDb::open(usize::MAX);
        let wal_size = || files_with_suffix(db.dir.path(), ".wal").unwrap()[0].1;
        assert_eq!(wal_size(), 0);

        db.run("put a 1");

        assert!(wal_size() > 0);
        assert_eq!(db.run("sync"), "OK\n");
        assert!(db.run("status").contains("目录里的 WAL 文件：1"));
    }
}
