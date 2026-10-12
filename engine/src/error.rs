//! 引擎的错误类型。
//!
//! 引擎里所有可能失败的函数都返回 [`Result<T>`]，失败的原因是 [`Error`] 的某一种。
//! 调用方可以用 `match` 分辨是哪一种，再决定怎么办。

use std::fmt;

/// 引擎的操作可能失败的原因。
#[derive(Debug)]
pub enum Error {
    /// 读写文件失败。里面是操作系统报告的原因。
    Io(std::io::Error),
    /// 文件里的内容不符合格式：被截断、被改动，或者根本不是引擎写的文件。
    /// 里面是一句说明，指出哪里不对。
    Corruption(String),
}

/// 引擎里通用的返回类型：成功是 `T`，失败是 [`Error`]。
///
/// 相当于把 `std::result::Result<T, Error>` 的第二个参数固定下来，少写一遍。
pub type Result<T> = std::result::Result<T, Error>;

/// 让 `?` 能把读写文件的错误自动变成引擎的错误。
///
/// 在返回 `Result<T>` 的函数里对一个 `std::io::Result` 用 `?`，失败时编译器
/// 会调用这里的 `from` 做转换，再把结果作为函数的返回值。
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// 给人看的错误信息，`{}` 打印时用。
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(f, "读写文件失败：{error}"),
            Error::Corruption(message) => write!(f, "文件内容损坏：{message}"),
        }
    }
}

/// 标准库对"错误类型"的约定：能打印，并且能说出更底层的原因是什么。
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            Error::Corruption(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::fs::File;
    use std::io::ErrorKind;

    /// 打开一个不存在的文件。用 `?` 把标准库的错误交给调用方。
    fn open_missing_file() -> Result<File> {
        let file = File::open("/这个目录不存在/也没有这个文件.sst")?;
        Ok(file)
    }

    // 读写文件的错误转换之后是 Io 这一种，原来的错误原样留在里面。
    #[test]
    fn io_error_converts_to_io_variant() {
        let io_error = std::io::Error::new(ErrorKind::PermissionDenied, "不让读");

        let error = Error::from(io_error);

        match error {
            Error::Io(inner) => assert_eq!(inner.kind(), ErrorKind::PermissionDenied),
            other => panic!("应当是 Io，实际是 {other:?}"),
        }
    }

    // 在返回引擎 Result 的函数里，`?` 自动完成这次转换。
    #[test]
    fn question_mark_converts_io_error() {
        let result = open_missing_file();

        match result {
            Err(Error::Io(inner)) => assert_eq!(inner.kind(), ErrorKind::NotFound),
            Err(other) => panic!("应当是 Io，实际是 {other:?}"),
            Ok(_) => panic!("这个文件不应该打得开"),
        }
    }

    // 打印出来的信息说明是哪一类错误，并带上具体的原因。
    #[test]
    fn display_names_the_kind_and_the_cause() {
        let io_error = Error::from(std::io::Error::other("磁盘满了"));
        let corruption = Error::Corruption("块的条数超出了文件长度".to_string());

        assert_eq!(io_error.to_string(), "读写文件失败：磁盘满了");
        assert_eq!(
            corruption.to_string(),
            "文件内容损坏：块的条数超出了文件长度"
        );
    }

    // Io 能说出更底层的原因，Corruption 没有更底层的原因。
    #[test]
    fn source_exposes_the_underlying_io_error() {
        let io_error = Error::from(std::io::Error::other("磁盘满了"));
        let corruption = Error::Corruption("坏了".to_string());

        assert!(io_error.source().is_some());
        assert!(corruption.source().is_none());
    }
}
