//! SST：磁盘上的一个有序文件，由很多数据块加一份索引组成，写完之后不再修改。
//!
//! 文件的样子（所有整数都是大端）：
//!
//! ```text
//! ┌──────┬──────┬───┬────────┬──────────────────┬────────────┬──────┐
//! │ 块 0 │ 块 1 │ … │ 块 n-1 │ 索引              │ 索引的起点  │ 魔数 │
//! └──────┴──────┴───┴────────┴──────────────────┴────────────┴──────┘
//!   数据块，一个接一个             每个块一项          4 字节       4 字节
//!
//! 索引：      块数（4 字节）│ 第 0 项 │ 第 1 项 │ …
//! 索引的一项： 块的起点（4 字节）│ 首 key 长度（2 字节）│ 首 key │ 末 key 长度（2 字节）│ 末 key
//! ```
//!
//! 与块的尾巴是同一个办法：内容在前，索引在后，最后几个字节指回索引。
//! 块的长度不记：它等于下一个块的起点减自己的起点，最后一个块用索引的起点来减。
//!
//! 打开文件时只把索引读进内存，数据块留在磁盘上，用到哪个读哪个。

mod builder;

pub use builder::SsTableBuilder;

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;

use bytes::{BufMut, Bytes};

use crate::error::Result;

/// 文件最后 4 个字节的魔数，用来确认这是引擎写的 SST。四个字节是 ASCII 的 "LSM1"。
pub const SST_MAGIC: u32 = 0x4C53_4D31;

/// 索引里的一项：一个数据块在文件的哪里，装着哪个范围的 key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockMeta {
    /// 这个块在文件里的起点。
    pub offset: usize,
    /// 块里的第一个 key，也是最小的。
    pub first_key: Bytes,
    /// 块里的最后一个 key，也是最大的。
    pub last_key: Bytes,
}

/// 把索引编码后接到 `buf` 的末尾：先是块数，然后每个块一项。
fn encode_block_meta(block_meta: &[BlockMeta], buf: &mut Vec<u8>) {
    buf.put_u32(block_meta.len() as u32);
    for meta in block_meta {
        buf.put_u32(meta.offset as u32);
        buf.put_u16(meta.first_key.len() as u16);
        buf.put_slice(&meta.first_key);
        buf.put_u16(meta.last_key.len() as u16);
        buf.put_slice(&meta.last_key);
    }
}

/// 磁盘上的一个文件，加上它的长度。SST 的读写都经过它。
pub struct FileObject {
    /// 以只读方式打开的文件。
    file: File,
    /// 文件的长度，字节。
    size: u64,
}

impl FileObject {
    /// 新建一个文件，写入全部内容并同步到磁盘，然后以只读方式重新打开。
    ///
    /// 路径上已经有文件时返回错误，不覆盖：SST 的文件名不会重复使用。
    pub fn create(path: &Path, data: &[u8]) -> Result<Self> {
        let mut file = File::create_new(path)?;
        file.write_all(data)?;
        // 相当于 fsync：让文件的内容和长度真正落到磁盘上。
        file.sync_all()?;

        let file = File::open(path)?;
        let size = data.len() as u64;
        Ok(Self { file, size })
    }

    /// 文件的长度，字节。
    pub fn size(&self) -> u64 {
        self.size
    }

    /// 读出文件里从 `offset` 开始的 `len` 个字节。
    ///
    /// 相当于 `pread`：不移动文件的读写位置，多个线程可以同时读同一个文件。
    /// 读不满 `len` 个字节（例如范围超出了文件末尾）时返回错误。
    pub fn read(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut data = vec![0; len];
        self.file.read_exact_at(&mut data, offset)?;
        Ok(data)
    }
}

/// 一个已经写好的 SST。
///
/// 手里拿着打开的文件和读进内存的索引；数据块不在内存里。
pub struct SsTable {
    /// 文件的编号，由创建它的一方分配。
    id: usize,
    file: FileObject,
    /// 索引：每个数据块一项，按块在文件里的顺序排列。
    block_meta: Vec<BlockMeta>,
    /// 索引在文件里的起点，同时也是最后一个数据块的终点。
    block_meta_offset: usize,
    /// 整个文件里最小的 key。
    first_key: Bytes,
    /// 整个文件里最大的 key。
    last_key: Bytes,
}

impl SsTable {
    pub fn id(&self) -> usize {
        self.id
    }

    /// 整个文件里最小的 key。
    pub fn first_key(&self) -> &[u8] {
        &self.first_key
    }

    /// 整个文件里最大的 key。
    pub fn last_key(&self) -> &[u8] {
        &self.last_key
    }

    /// 文件里有几个数据块。
    pub fn block_count(&self) -> usize {
        self.block_meta.len()
    }

    /// 索引：每个数据块一项。
    pub fn block_meta(&self) -> &[BlockMeta] {
        &self.block_meta
    }

    /// 索引在文件里的起点。
    pub fn block_meta_offset(&self) -> usize {
        self.block_meta_offset
    }

    /// 文件的长度，字节。
    pub fn file_size(&self) -> u64 {
        self.file.size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::io::ErrorKind;

    // 写出去的文件，内容与给出的字节逐个相同，记下的长度也一致。
    #[test]
    fn create_writes_all_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");

        let file = FileObject::create(&path, b"hello sst").unwrap();

        assert_eq!(file.size(), 9);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello sst");
    }

    // 路径上已经有文件时不覆盖，返回读写错误，原来的内容不变。
    #[test]
    fn create_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        FileObject::create(&path, b"first").unwrap();

        let result = FileObject::create(&path, b"second");

        match result {
            Err(Error::Io(error)) => assert_eq!(error.kind(), ErrorKind::AlreadyExists),
            Err(other) => panic!("应当是 Io，实际是 {other:?}"),
            Ok(_) => panic!("不应该覆盖已有的文件"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
    }

    // 目录不存在时创建失败，得到读写错误而不是 panic。
    #[test]
    fn create_in_missing_directory_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("没有这个目录").join("1.sst");

        let result = FileObject::create(&path, b"data");

        assert!(matches!(result, Err(Error::Io(_))));
    }

    // 按起点和长度读出文件中间的一段，可以反复读、不按顺序读。
    #[test]
    fn read_returns_the_requested_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        let file = FileObject::create(&path, b"0123456789").unwrap();

        assert_eq!(file.read(6, 4).unwrap(), b"6789");
        assert_eq!(file.read(0, 3).unwrap(), b"012");
        assert_eq!(file.read(3, 3).unwrap(), b"345");
        assert_eq!(file.read(0, 10).unwrap(), b"0123456789");
    }

    // 要读的范围超出文件末尾时返回读写错误。
    #[test]
    fn read_past_the_end_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        let file = FileObject::create(&path, b"0123456789").unwrap();

        let result = file.read(8, 4);

        assert!(matches!(result, Err(Error::Io(_))));
    }

    // 索引的编码逐字节确定：块数，然后每项是起点、首 key、末 key。
    #[test]
    fn block_meta_encoding_is_exact() {
        let block_meta = vec![
            BlockMeta {
                offset: 0,
                first_key: Bytes::from_static(b"a"),
                last_key: Bytes::from_static(b"cc"),
            },
            BlockMeta {
                offset: 300,
                first_key: Bytes::from_static(b"d"),
                last_key: Bytes::from_static(b"d"),
            },
        ];
        let mut buf = vec![0xee];

        encode_block_meta(&block_meta, &mut buf);

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // buf 里原有的内容不动
            0xee,
            // 块数
            0, 0, 0, 2,
            // 第 0 项：起点 0，首 key "a"，末 key "cc"
            0, 0, 0, 0,   0, 1, b'a',   0, 2, b'c', b'c',
            // 第 1 项：起点 300 = 0x012c，首 key "d"，末 key "d"
            0, 0, 1, 0x2c,   0, 1, b'd',   0, 1, b'd',
        ];
        assert_eq!(buf, expected);
    }
}
