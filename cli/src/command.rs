//! 一行输入与它表示的命令。
//!
//! 一行按空白切成若干个词：第一个词是命令名，其余是参数。
//! 因为按空白切分，key 与 value 里不能有空格。

use anyhow::{Context, bail};

/// 用户输入的一条命令。由 [`parse`] 从一行文本得到。
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `put KEY VALUE`：写入一条记录。
    Put { key: String, value: String },
    /// `get KEY`：查找一个 key。
    Get { key: String },
    /// `delete KEY`：删除一个 key。
    Delete { key: String },
    /// `scan [LOWER [UPPER]]`：按 key 从小到大列出记录。
    ///
    /// 范围包含 `lower`、不包含 `upper`；`None` 表示这一端不设限。
    Scan {
        lower: Option<String>,
        upper: Option<String>,
    },
    /// `fill COUNT`：写入 `COUNT` 条 `key-000000 = value-000000` 这样的记录，
    /// 用来很快写满 memtable、看到 SST 文件出现。
    Fill { count: u64 },
    /// `status`：冻结的 memtable 个数、L0 的 SST 个数、块缓存的用量，以及目录里的 SST 文件。
    Status,
    /// `help`：列出所有命令。
    Help,
    /// `quit`：关闭数据库并退出。
    Quit,
}

/// 所有的命令名。用来区分"不认识的命令"和"认识的命令、参数个数不对"。
const COMMAND_NAMES: [&str; 8] = [
    "put", "get", "delete", "scan", "fill", "status", "help", "quit",
];

/// `help` 打印的内容。
pub const HELP: &str = "\
put KEY VALUE        写入一条记录
get KEY              查找一个 key
delete KEY           删除一个 key
scan [LOWER [UPPER]] 按 key 从小到大列出记录，包含 LOWER、不包含 UPPER
fill COUNT           写入 COUNT 条 key-000000 = value-000000 这样的记录
status               冻结的 memtable 个数、L0 的 SST 个数、块缓存的用量、目录里的 SST 文件
help                 显示本说明
quit                 关闭数据库并退出（Ctrl-D 也一样）";

/// 把一行输入解析成命令。行首、行尾和词之间多出来的空白不影响结果。
///
/// 下面几种情况返回错误，错误信息说明哪里不对：
/// 空行；不认识的命令名；参数个数不对；`fill` 的条数不是非负整数。
pub fn parse(line: &str) -> anyhow::Result<Command> {
    let words: Vec<&str> = line.split_whitespace().collect();

    let command = match words.as_slice() {
        ["put", key, value] => Command::Put {
            key: key.to_string(),
            value: value.to_string(),
        },
        ["get", key] => Command::Get {
            key: key.to_string(),
        },
        ["delete", key] => Command::Delete {
            key: key.to_string(),
        },
        ["scan", lower, upper] => Command::Scan {
            lower: Some(lower.to_string()),
            upper: Some(upper.to_string()),
        },
        ["scan", lower] => Command::Scan {
            lower: Some(lower.to_string()),
            upper: None,
        },
        ["scan"] => Command::Scan {
            lower: None,
            upper: None,
        },
        ["fill", count] => {
            let count = count
                .parse()
                .with_context(|| format!("fill 的条数应当是非负整数，实际是 {count}"))?;
            Command::Fill { count }
        }
        ["status"] => Command::Status,
        ["help"] => Command::Help,
        ["quit"] => Command::Quit,
        [] => bail!("空行"),
        // 认识的命令名、参数个数不对。要放在下一个分支之前，否则会被当成不认识的命令。
        [name, ..] if COMMAND_NAMES.contains(name) => {
            bail!("{name} 的参数个数不对，输入 help 查看用法")
        }
        [name, ..] => bail!("不认识的命令：{name}，输入 help 查看用法"),
    };
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析失败时的错误信息。解析成功时测试失败。
    fn parse_error(line: &str) -> String {
        match parse(line) {
            Ok(command) => panic!("{line:?} 不应当解析成功，实际得到 {command:?}"),
            Err(error) => format!("{error:#}"),
        }
    }

    // 每种命令的基本写法都能解析，参数原样放进对应的字段。
    #[test]
    fn parses_every_command() {
        assert_eq!(
            parse("put a 1").unwrap(),
            Command::Put {
                key: "a".to_string(),
                value: "1".to_string()
            }
        );
        assert_eq!(
            parse("get a").unwrap(),
            Command::Get {
                key: "a".to_string()
            }
        );
        assert_eq!(
            parse("delete a").unwrap(),
            Command::Delete {
                key: "a".to_string()
            }
        );
        assert_eq!(parse("fill 100").unwrap(), Command::Fill { count: 100 });
        assert_eq!(parse("status").unwrap(), Command::Status);
        assert_eq!(parse("help").unwrap(), Command::Help);
        assert_eq!(parse("quit").unwrap(), Command::Quit);
    }

    // scan 的三种写法：不给范围、只给下界、两端都给。
    #[test]
    fn parses_scan_with_zero_one_or_two_bounds() {
        assert_eq!(
            parse("scan").unwrap(),
            Command::Scan {
                lower: None,
                upper: None
            }
        );
        assert_eq!(
            parse("scan b").unwrap(),
            Command::Scan {
                lower: Some("b".to_string()),
                upper: None
            }
        );
        assert_eq!(
            parse("scan b d").unwrap(),
            Command::Scan {
                lower: Some("b".to_string()),
                upper: Some("d".to_string())
            }
        );
    }

    // 行首行尾的空白、换行符、词之间的多个空格或制表符都不影响结果。
    #[test]
    fn extra_whitespace_is_ignored() {
        assert_eq!(
            parse("  put \t a    1  \n").unwrap(),
            Command::Put {
                key: "a".to_string(),
                value: "1".to_string()
            }
        );
        assert_eq!(parse("quit\n").unwrap(), Command::Quit);
    }

    // 空行和只有空白的行是错误，不是某条命令。
    #[test]
    fn empty_line_is_an_error() {
        assert!(parse("").is_err());
        assert!(parse("   \n").is_err());
    }

    // 不认识的命令名：错误信息里有这个名字。命令名区分大小写。
    #[test]
    fn unknown_command_is_an_error() {
        assert!(parse_error("remove a").contains("remove"));
        assert!(parse_error("PUT a 1").contains("PUT"));
    }

    // 认识的命令、参数多了或少了：报的是参数个数不对，而不是不认识。
    #[test]
    fn wrong_argument_count_is_an_error() {
        for line in [
            "put a",
            "put a 1 2",
            "get",
            "get a b",
            "delete",
            "scan a b c",
            "fill",
            "status now",
            "quit now",
        ] {
            let message = parse_error(line);
            assert!(
                message.contains("参数个数"),
                "{line:?} 的错误信息是 {message:?}"
            );
        }
    }

    // fill 的条数必须是非负整数：错误信息里有原来的输入。
    #[test]
    fn fill_count_must_be_a_number() {
        assert!(parse_error("fill abc").contains("abc"));
        assert!(parse_error("fill -1").contains("-1"));
        assert_eq!(parse("fill 0").unwrap(), Command::Fill { count: 0 });
    }
}
