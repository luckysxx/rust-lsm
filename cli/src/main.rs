//! 交互式命令行程序：打开一个目录里的数据库，一行一条命令地操作它。
//!
//! 用法见 `USAGE`。输入 `help` 列出命令。

mod command;
mod execute;

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use lsm::compaction::CompactionOptions;
use lsm::compaction::leveled::LeveledOptions;
use lsm::compaction::simple_leveled::SimpleLeveledOptions;
use lsm::compaction::tiered::TieredOptions;
use lsm::db::Db;
use lsm::lsm_storage::LsmStorageOptions;

use command::Command;
use execute::execute;

const USAGE: &str = "\
用法：lsm-cli 目录 [memtable 上限，字节] [合并策略]

  目录             数据库所在的目录，不存在就创建；已有数据库时接着打开，
                   合并策略要与写入时的形状相同（simple 与 leveled 可以互换）
  memtable 上限    可写的 memtable 写满多少字节就冻结（默认 4096）。
                   取得小，几十条记录就能看到 SST 文件出现
  合并策略         simple（简单分层，默认）、tiered 或 leveled

写入先追加到 WAL（不同步）：进程崩溃或者输入 crash 退出之后，再次打开时数据都在。";

/// memtable 上限的默认值，字节。取得小，用 `fill` 写几十条就会冻结。
const DEFAULT_MEMTABLE_SIZE_LIMIT: usize = 4096;
/// SST 里每个数据块的目标大小，字节。
const BLOCK_SIZE: usize = 512;
/// 块缓存的容量，字节。
const BLOCK_CACHE_SIZE: usize = 1 << 20;
/// L0 之下有几层。
const MAX_LEVELS: usize = 4;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, memtable_size_limit, strategy) = match args.as_slice() {
        [dir] => (PathBuf::from(dir), DEFAULT_MEMTABLE_SIZE_LIMIT, "simple"),
        [dir, limit] => (PathBuf::from(dir), parse_limit(limit)?, "simple"),
        [dir, limit, strategy] => (PathBuf::from(dir), parse_limit(limit)?, strategy.as_str()),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    let compaction = match strategy {
        // 后台自动合并：L0 攒到 2 个文件就并进 L1，下层至少是上层的 2 倍。
        // 取得小，用 status 很快就能看到文件一层一层往下沉。
        "simple" => CompactionOptions::SimpleLeveled(SimpleLeveledOptions {
            level0_file_num_compaction_trigger: 2,
            size_ratio_percent: 200,
        }),
        // tier 到 4 个就开始检查，其余是 RocksDB 的默认值。
        "tiered" => CompactionOptions::Tiered(TieredOptions {
            num_tiers: 4,
            max_size_amplification_percent: 200,
            size_ratio: 1,
            min_merge_width: 2,
            max_merge_width: None,
        }),
        // L0 攒到 2 个文件就往下并，相邻层的目标之比 4，基准层约 2 张 memtable：
        // 取得小，写几百条就能看到中间层从下往上启用。
        "leveled" => CompactionOptions::Leveled(LeveledOptions {
            level0_file_num_compaction_trigger: 2,
            level_size_multiplier: 4,
            base_level_size_bytes: (memtable_size_limit * 2) as u64,
        }),
        _ => bail!("合并策略应当是 simple、tiered 或 leveled，实际是 {strategy}"),
    };

    let options = LsmStorageOptions {
        memtable_size_limit,
        block_size: BLOCK_SIZE,
        block_cache_size: BLOCK_CACHE_SIZE,
        // compaction 写出的文件与 flush 写出的一样大。
        target_sst_size: memtable_size_limit,
        max_levels: MAX_LEVELS,
        compaction,
        // 打开 WAL：可以用 crash 命令或 Ctrl-C 结束进程，再打开看数据是否都在。
        enable_wal: true,
    };
    let db = Db::open(&dir, options).with_context(|| format!("打开数据库：{}", dir.display()))?;
    println!("已打开 {}。输入 help 查看命令。", dir.display());

    run(&db, &dir)?;

    db.close().context("关闭数据库：把内存里的数据写成 SST")?;
    println!("已关闭，内存里的数据都已写成 SST。");
    Ok(())
}

/// 解析命令行里的 memtable 上限。
fn parse_limit(limit: &str) -> anyhow::Result<usize> {
    limit
        .parse()
        .with_context(|| format!("memtable 上限应当是正整数，实际是 {limit}"))
}

/// 读输入的循环：读一行、解析、执行、打印，直到 `quit` 或输入结束（Ctrl-D）。
///
/// 一条命令出错只打印错误，接着读下一行。读写终端失败时返回错误。
fn run(db: &Db, dir: &Path) -> anyhow::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout();
    let mut line = String::new();

    loop {
        // 提示符后面没有换行，标准输出不会自己送出去，要手动 flush。
        write!(stdout, "lsm> ")?;
        stdout.flush()?;

        line.clear();
        let bytes_read = stdin.read_line(&mut line)?;
        // 读到 0 个字节就是输入结束：用户按了 Ctrl-D，或者输入来自文件、已经读完。
        if bytes_read == 0 {
            println!();
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }

        let command = match command::parse(&line) {
            Ok(command) => command,
            Err(error) => {
                eprintln!("错误：{error:#}");
                continue;
            }
        };
        if command == Command::Quit {
            return Ok(());
        }
        if command == Command::Crash {
            // 不关闭数据库、不运行任何清理，进程立刻结束：与被 kill 掉一样。
            println!("进程立刻结束，没有关闭数据库。再次打开时从 WAL 恢复。");
            std::process::abort();
        }
        if let Err(error) = execute(db, dir, &command, &mut stdout) {
            eprintln!("错误：{error:#}");
        }
    }
}
