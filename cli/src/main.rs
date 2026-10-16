//! 交互式命令行程序：打开一个目录里的数据库，一行一条命令地操作它。
//!
//! 用法见 `USAGE`。输入 `help` 列出命令。

mod command;
mod execute;

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use lsm::db::Db;
use lsm::lsm_storage::LsmStorageOptions;

use command::Command;
use execute::{execute, sst_files};

const USAGE: &str = "\
用法：lsm-cli 目录 [memtable 上限，字节]

  目录             数据库所在的目录，不存在就创建；里面不能已有 SST 文件
  memtable 上限    可写的 memtable 写满多少字节就冻结（默认 4096）。
                   取得小，几十条记录就能看到 SST 文件出现";

/// memtable 上限的默认值，字节。取得小，用 `fill` 写几十条就会冻结。
const DEFAULT_MEMTABLE_SIZE_LIMIT: usize = 4096;
/// SST 里每个数据块的目标大小，字节。
const BLOCK_SIZE: usize = 512;
/// 块缓存的容量，字节。
const BLOCK_CACHE_SIZE: usize = 1 << 20;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, memtable_size_limit) = match args.as_slice() {
        [dir] => (PathBuf::from(dir), DEFAULT_MEMTABLE_SIZE_LIMIT),
        [dir, limit] => {
            let limit = limit
                .parse()
                .with_context(|| format!("memtable 上限应当是正整数，实际是 {limit}"))?;
            (PathBuf::from(dir), limit)
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    refuse_existing_sst_files(&dir)?;
    let options = LsmStorageOptions {
        memtable_size_limit,
        block_size: BLOCK_SIZE,
        block_cache_size: BLOCK_CACHE_SIZE,
        // compaction 写出的文件与 flush 写出的一样大。
        target_sst_size: memtable_size_limit,
    };
    let db = Db::open(&dir, options).with_context(|| format!("打开数据库：{}", dir.display()))?;
    println!("已打开 {}。输入 help 查看命令。", dir.display());

    run(&db, &dir)?;

    db.close().context("关闭数据库：把内存里的数据写成 SST")?;
    println!("已关闭，内存里的数据都已写成 SST。");
    Ok(())
}

/// 目录里已经有 SST 文件时返回错误。目录不存在不算错误，打开时会创建。
///
/// 引擎重新打开时不读取已有的文件，SST 的编号又从 1 开始：
/// 新文件会撞上旧文件的名字，flush 一直失败。
fn refuse_existing_sst_files(dir: &Path) -> anyhow::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let files = sst_files(dir).with_context(|| format!("读取目录：{}", dir.display()))?;
    if !files.is_empty() {
        bail!(
            "{} 里已经有 {} 个 SST 文件。引擎还不会读回已有的文件（要等 manifest 与 WAL），\
             请换一个目录，或者先删掉这些文件",
            dir.display(),
            files.len()
        );
    }
    Ok(())
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
        if let Err(error) = execute(db, dir, &command, &mut stdout) {
            eprintln!("错误：{error:#}");
        }
    }
}
