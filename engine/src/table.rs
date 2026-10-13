//! SST：磁盘上的一个有序文件，由很多数据块加一份索引组成，写完之后不再修改。
//!
//! 文件的样子（所有整数都是大端）：
//!
//! ```text
//! ┌──────┬──────┬───┬────────┬──────────────────┬────────────┬──────┐
//! │ 块 0  │ 块 1 │ … │ 块 n-1 │ 索引              │  索引的起点  │ 魔数 │
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
mod iterator;

pub use builder::SsTableBuilder;
pub use iterator::SsTableIterator;

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use bytes::{BufMut, Bytes};

use crate::block::Block;
use crate::error::{Error, Result};

/// 文件最后 4 个字节的魔数，用来确认这是引擎写的 SST。四个字节是 ASCII 的 "LSM1"。
pub const SST_MAGIC: u32 = 0x4C53_4D31;

/// 文件末尾固定的部分有多长：索引的起点 4 字节，魔数 4 字节。
const FOOTER_SIZE: usize = 8;

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

/// 把 `encode_block_meta` 写出的字节还原成索引。
///
/// 字节不符合格式（被截断、长度对不上、后面有多余的内容）时返回 [`Error::Corruption`]。
fn decode_block_meta(mut buf: &[u8]) -> Result<Vec<BlockMeta>> {
    // `buf` 是"还没读的字节"。下面每读一段，就把它从前面切掉一段。
    let block_count = take_u32(&mut buf)? as usize;

    let mut block_meta = Vec::new();
    for _ in 0..block_count {
        let offset = take_u32(&mut buf)? as usize;
        let first_key_len = take_u16(&mut buf)? as usize;
        let first_key = take_bytes(&mut buf, first_key_len)?;
        let last_key_len = take_u16(&mut buf)? as usize;
        let last_key = take_bytes(&mut buf, last_key_len)?;
        block_meta.push(BlockMeta {
            offset,
            first_key: Bytes::copy_from_slice(first_key),
            last_key: Bytes::copy_from_slice(last_key),
        });
    }

    if !buf.is_empty() {
        return Err(corrupt_sst("索引后面有多余的字节"));
    }
    Ok(block_meta)
}

/// 从 `buf` 的开头取走 `len` 个字节，`buf` 随之缩短；不够时返回错误。
fn take_bytes<'a>(buf: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    if buf.len() < len {
        return Err(corrupt_sst("索引被截断"));
    }
    let (taken, rest) = buf.split_at(len);
    *buf = rest;
    Ok(taken)
}

/// 从 `buf` 的开头取走一个 2 字节的整数。
fn take_u16(buf: &mut &[u8]) -> Result<u16> {
    let bytes = take_bytes(buf, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// 从 `buf` 的开头取走一个 4 字节的整数。
fn take_u32(buf: &mut &[u8]) -> Result<u32> {
    let bytes = take_bytes(buf, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// 读出 `bytes[at..at + 4]` 这 4 个字节表示的整数（大端）。
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// 检查读回来的索引是否自洽：至少有一个块；第一个块从 0 开始；起点严格递增；
/// 每个块都在索引之前；每个块的首 key 不大于末 key，并且大于前一个块的末 key。
fn check_block_meta(block_meta: &[BlockMeta], block_meta_start: usize) -> Result<()> {
    if block_meta.is_empty() {
        return Err(corrupt_sst("文件里没有数据块"));
    }
    if block_meta[0].offset != 0 {
        return Err(corrupt_sst("第一个数据块的起点不是 0"));
    }
    for (i, meta) in block_meta.iter().enumerate() {
        if meta.offset >= block_meta_start {
            return Err(corrupt_sst("数据块的起点落在了索引里"));
        }
        if meta.first_key.is_empty() || meta.first_key > meta.last_key {
            return Err(corrupt_sst("数据块的首 key 为空，或者大于末 key"));
        }
        if i > 0 {
            let previous = &block_meta[i - 1];
            if previous.offset >= meta.offset {
                return Err(corrupt_sst("数据块的起点没有递增"));
            }
            if previous.last_key >= meta.first_key {
                return Err(corrupt_sst("相邻两个数据块的 key 范围重叠"));
            }
        }
    }
    Ok(())
}

/// 造一个"SST 文件损坏"的错误。
fn corrupt_sst(message: &str) -> Error {
    Error::Corruption(format!("SST 文件：{message}"))
}

/// 磁盘上的一个文件，加上它的长度。SST 的读写都经过它。
pub struct FileObject {
    /// 以只读方式打开的文件。
    file: File,
    /// 文件的长度，字节。
    size: usize,
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
        let size = data.len();
        Ok(Self { file, size })
    }

    /// 以只读方式打开一个已有的文件。
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len() as usize;
        Ok(Self { file, size })
    }

    /// 文件的长度，字节。
    pub fn size(&self) -> usize {
        self.size
    }

    /// 读出文件里从 `offset` 开始的 `len` 个字节。
    ///
    /// 相当于 `pread`：不移动文件的读写位置，多个线程可以同时读同一个文件。
    /// 读不满 `len` 个字节（例如范围超出了文件末尾）时返回错误。
    pub fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let mut data = vec![0; len];
        self.file.read_exact_at(&mut data, offset as u64)?;
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
    block_meta_start: usize,
    /// 整个文件里最小的 key。
    first_key: Bytes,
    /// 整个文件里最大的 key。
    last_key: Bytes,
}

impl SsTable {
    /// 打开一个已有的 SST 文件：从尾部读回索引，数据块留在磁盘上。
    ///
    /// 文件不符合格式时返回 [`Error::Corruption`]，读文件失败时返回 [`Error::Io`]。
    pub fn open(id: usize, file: FileObject) -> Result<Self> {
        // 从文件的尾巴倒着找。末尾固定的 8 个字节：索引的起点、魔数。
        // 先确认文件够长，再做减法：无符号数相减不能得到负数。
        let file_size = file.size();
        if file_size < FOOTER_SIZE {
            return Err(corrupt_sst("文件太短，放不下末尾的 8 个字节"));
        }
        let footer_start = file_size - FOOTER_SIZE;
        let footer = file.read(footer_start, FOOTER_SIZE)?;
        let block_meta_start = read_u32(&footer, 0) as usize;
        let sst_magic = read_u32(&footer, 4);
        if sst_magic != SST_MAGIC {
            return Err(corrupt_sst("末尾的魔数不对，这不是引擎写的 SST"));
        }
        if block_meta_start > footer_start {
            return Err(corrupt_sst("索引的起点超出了文件"));
        }

        // 索引占 [block_meta_start, footer_start)。
        let raw_block_meta_len = footer_start - block_meta_start;
        let raw_block_meta = file.read(block_meta_start, raw_block_meta_len)?;

        let block_meta = decode_block_meta(&raw_block_meta)?;
        check_block_meta(&block_meta, block_meta_start)?;

        let first_key = block_meta[0].first_key.clone();
        let last_key = block_meta[block_meta.len() - 1].last_key.clone();
        Ok(Self {
            id,
            file,
            block_meta,
            block_meta_start,
            first_key,
            last_key,
        })
    }

    /// 从文件里读出第 `block_index` 个数据块并解码。
    ///
    /// `block_index` 必须小于 `block_count()`。读文件失败或块的内容损坏时返回错误。
    pub fn read_block(&self, block_index: usize) -> Result<Arc<Block>> {
        // 下标越界是调用方写错了，不是文件坏了，所以用断言而不是返回错误。
        assert!(
            block_index < self.block_count(),
            "块的下标 {block_index} 超出了块数 {}",
            self.block_count()
        );

        // 块占 [block_start, block_end)：终点是下一个块的起点，最后一个块到索引的起点为止。
        let block_start = self.block_meta[block_index].offset;
        let block_end = match self.block_meta.get(block_index + 1) {
            Some(next_meta) => next_meta.offset,
            None => self.block_meta_start,
        };

        // `read` 要的是起点和长度，不是起点和终点。
        let block_len = block_end - block_start;
        let encoded = self.file.read(block_start, block_len)?;
        let block = Block::decode(&encoded)?;
        Ok(Arc::new(block))
    }

    /// `key` 如果在这个文件里，会在第几个数据块：第一个末 key 大于等于 `key` 的块。
    ///
    /// `key` 比文件里所有的 key 都大时，没有这样的块，返回 `block_count()`。
    pub fn find_block_index(&self, key: &[u8]) -> usize {
        // 在块的下标上二分。始终保持：low 之前的块末 key 都小于 key，
        // high 以及之后的块末 key 都大于等于 key。
        // 所有块的末 key 都小于 key 时，结果自然是块数，不需要单独处理。
        let mut low = 0;
        let mut high = self.block_meta.len();
        while low < high {
            let mid = low + (high - low) / 2;
            let mid_key = &self.block_meta[mid].last_key;
            if mid_key < key {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

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
    pub fn block_meta_start(&self) -> usize {
        self.block_meta_start
    }

    /// 文件的长度，字节。
    pub fn file_size(&self) -> u64 {
        self.file.size() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockIterator;
    use crate::iterators::vec_iterator::collect_all;
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// 七条记录，块的目标大小 26 恰好放三条：三个块 [a, c]、[d, f]、[g, g]，
    /// 起点依次是 0、26、52，索引从 62 开始。返回文件的路径。
    fn write_three_block_sst(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("1.sst");
        let mut builder = SsTableBuilder::new(26);
        for (key, value) in [
            ("a", "1"),
            ("b", "2"),
            ("c", "3"),
            ("d", "4"),
            ("e", "5"),
            ("f", "6"),
            ("g", "7"),
        ] {
            builder.add(key.as_bytes(), value.as_bytes());
        }
        builder.build(1, &path).unwrap();
        path
    }

    /// 打开路径上的 SST。
    fn open_sst(path: &Path) -> Result<SsTable> {
        SsTable::open(1, FileObject::open(path)?)
    }

    /// 把文件的内容换成 `bytes`，再尝试打开；应当因为内容损坏而失败。
    fn assert_open_reports_corruption(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        match open_sst(path) {
            Err(Error::Corruption(_)) => {}
            Err(other) => panic!("应当是 Corruption，实际是 {other:?}"),
            Ok(_) => panic!("这个文件不应该打得开"),
        }
    }

    /// 读出一个块里的全部记录。
    fn records_in_block(sst: &SsTable, block_index: usize) -> Vec<(String, String)> {
        let block = sst.read_block(block_index).unwrap();
        let mut iter = BlockIterator::new(block);
        collect_all(&mut iter)
    }

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

    // 打开一个不存在的文件，得到读写错误。
    #[test]
    fn file_object_open_missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("没有这个文件.sst");

        let result = FileObject::open(&path);

        match result {
            Err(Error::Io(error)) => assert_eq!(error.kind(), ErrorKind::NotFound),
            Err(other) => panic!("应当是 Io，实际是 {other:?}"),
            Ok(_) => panic!("这个文件不应该打得开"),
        }
    }

    // 打开已有的文件，记下的长度就是文件的长度，内容可以读。
    #[test]
    fn file_object_open_knows_the_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        std::fs::write(&path, b"0123456789").unwrap();

        let file = FileObject::open(&path).unwrap();

        assert_eq!(file.size(), 10);
        assert_eq!(file.read(4, 3).unwrap(), b"456");
    }

    // 索引编码再解码，得到的各项与原来的相同。
    #[test]
    fn block_meta_decoding_is_the_inverse_of_encoding() {
        let block_meta = vec![
            BlockMeta {
                offset: 0,
                first_key: Bytes::from_static(b"a"),
                last_key: Bytes::from_static(b"cc"),
            },
            BlockMeta {
                offset: 70000,
                first_key: Bytes::from_static(b"d"),
                last_key: Bytes::from_static(b"d"),
            },
        ];
        let mut buf = Vec::new();
        encode_block_meta(&block_meta, &mut buf);

        let decoded = decode_block_meta(&buf).unwrap();

        assert_eq!(decoded, block_meta);
    }

    // 索引被截断、块数写大了、后面多出字节，解码都返回"内容损坏"。
    #[test]
    fn block_meta_decoding_rejects_malformed_bytes() {
        let block_meta = vec![BlockMeta {
            offset: 0,
            first_key: Bytes::from_static(b"a"),
            last_key: Bytes::from_static(b"b"),
        }];
        let mut valid = Vec::new();
        encode_block_meta(&block_meta, &mut valid);

        for cut in 1..=valid.len() {
            let truncated = &valid[..valid.len() - cut];
            assert!(matches!(
                decode_block_meta(truncated),
                Err(Error::Corruption(_))
            ));
        }

        let mut count_too_large = valid.clone();
        count_too_large[3] = 2;
        assert!(matches!(
            decode_block_meta(&count_too_large),
            Err(Error::Corruption(_))
        ));

        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(matches!(
            decode_block_meta(&trailing),
            Err(Error::Corruption(_))
        ));
    }

    // ---------- 第 3 步之一：打开 ----------

    // 写出去再打开，读回来的索引、首末 key、索引的起点都与写的时候一致。
    #[test]
    fn open_reads_back_what_the_builder_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("7.sst");
        let mut builder = SsTableBuilder::new(26);
        for key in ["a", "b", "c", "d", "e", "f", "g"] {
            builder.add(key.as_bytes(), b"1");
        }
        let written = builder.build(7, &path).unwrap();

        let opened = SsTable::open(7, FileObject::open(&path).unwrap()).unwrap();

        assert_eq!(opened.id(), 7);
        assert_eq!(opened.block_meta(), written.block_meta());
        assert_eq!(opened.block_meta_start(), written.block_meta_start());
        assert_eq!(opened.first_key(), b"a");
        assert_eq!(opened.last_key(), b"g");
        assert_eq!(opened.file_size(), written.file_size());
    }

    // 索引的各项逐个核对：三个块的起点与首末 key。
    #[test]
    fn open_decodes_every_block_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);

        let sst = open_sst(&path).unwrap();

        assert_eq!(sst.block_count(), 3);
        assert_eq!(sst.block_meta_start(), 62);
        let summary: Vec<(usize, &[u8], &[u8])> = sst
            .block_meta()
            .iter()
            .map(|meta| (meta.offset, &meta.first_key[..], &meta.last_key[..]))
            .collect();
        assert_eq!(
            summary,
            vec![
                (0, &b"a"[..], &b"c"[..]),
                (26, &b"d"[..], &b"f"[..]),
                (52, &b"g"[..], &b"g"[..]),
            ]
        );
    }

    // 文件短得连末尾的 8 个字节都放不下：内容损坏，而不是越界 panic。
    #[test]
    fn open_rejects_file_shorter_than_the_footer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");

        assert_open_reports_corruption(&path, b"");
        assert_open_reports_corruption(&path, b"LSM1");
        assert_open_reports_corruption(&path, &[0, 0, 0, 0, b'L', b'S', b'M']);
    }

    // 最后 4 个字节不是魔数：这不是引擎写的文件。
    #[test]
    fn open_rejects_wrong_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        bytes[len - 1] = b'2';

        assert_open_reports_corruption(&path, &bytes);
    }

    // 魔数和索引的起点如果读反了位置，这里会发现：起点是 62，不等于魔数。
    #[test]
    fn open_reads_meta_offset_before_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        assert_eq!(&bytes[len - 8..len - 4], &[0, 0, 0, 62]);
        assert_eq!(&bytes[len - 4..], b"LSM1");

        let sst = open_sst(&path).unwrap();

        assert_eq!(sst.block_meta_start(), 62);
    }

    // 索引的起点指到了文件外面，或者指进了末尾的 8 个字节里。
    #[test]
    fn open_rejects_meta_offset_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let original = std::fs::read(&path).unwrap();
        let len = original.len();

        let mut far_beyond = original.clone();
        far_beyond[len - 8..len - 4].copy_from_slice(&[0, 0, 0xff, 0xff]);
        assert_open_reports_corruption(&path, &far_beyond);

        // 起点恰好比"末尾 8 字节的开头"大 1。
        let mut just_beyond = original.clone();
        let footer_start = (len - 8) as u32;
        just_beyond[len - 8..len - 4].copy_from_slice(&(footer_start + 1).to_be_bytes());
        assert_open_reports_corruption(&path, &just_beyond);
    }

    // 索引的起点指错了地方（指到了数据块中间）：索引解不出来。
    #[test]
    fn open_rejects_meta_offset_pointing_into_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        bytes[len - 8..len - 4].copy_from_slice(&[0, 0, 0, 5]);

        assert_open_reports_corruption(&path, &bytes);
    }

    // 文件从末尾被截掉任意多个字节，都打不开，也不会 panic。
    #[test]
    fn open_rejects_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let original = std::fs::read(&path).unwrap();

        for cut in 1..=original.len() {
            let truncated = &original[..original.len() - cut];
            std::fs::write(&path, truncated).unwrap();
            assert!(
                open_sst(&path).is_err(),
                "截掉末尾 {cut} 个字节之后不应该打得开"
            );
        }
    }

    // ---------- 第 3 步之二：读块与找块 ----------

    // 每个块读出来的记录正是写进去的那几条；最后一个块以索引的起点为终点。
    #[test]
    fn read_block_returns_the_records_of_that_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let sst = open_sst(&path).unwrap();

        let keys_of = |block_index: usize| -> Vec<String> {
            records_in_block(&sst, block_index)
                .into_iter()
                .map(|(key, _value)| key)
                .collect()
        };
        assert_eq!(keys_of(0), vec!["a", "b", "c"]);
        assert_eq!(keys_of(1), vec!["d", "e", "f"]);
        assert_eq!(keys_of(2), vec!["g"]);
    }

    // 块可以不按顺序读、重复读，每次都是从文件里读出来的同样的内容。
    #[test]
    fn read_block_can_be_called_in_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let sst = open_sst(&path).unwrap();

        assert_eq!(records_in_block(&sst, 2).len(), 1);
        assert_eq!(records_in_block(&sst, 0).len(), 3);
        assert_eq!(records_in_block(&sst, 2).len(), 1);
        assert_eq!(records_in_block(&sst, 1).len(), 3);
    }

    // 刚用构建器写出来的 SST 不用重新打开，也能直接读块。
    #[test]
    fn read_block_works_on_a_freshly_built_sst() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        let mut builder = SsTableBuilder::new(4096);
        builder.add(b"a", b"1");
        builder.add(b"b", b"");
        let sst = builder.build(1, &path).unwrap();

        let records = records_in_block(&sst, 0);

        assert_eq!(
            records,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), String::new())
            ]
        );
    }

    // 打开之后数据块的内容被破坏（第一个块的条数被改成 0）：读这个块时报告内容损坏，
    // 别的块不受影响。
    #[test]
    fn read_block_reports_corrupted_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        // 第一个块占 [0, 26)，它的条数在最后两个字节。
        bytes[24] = 0;
        bytes[25] = 0;
        std::fs::write(&path, &bytes).unwrap();
        let sst = open_sst(&path).unwrap();

        let result = sst.read_block(0);

        assert!(matches!(result, Err(Error::Corruption(_))));
        assert_eq!(records_in_block(&sst, 1).len(), 3);
    }

    // 打开之后文件被截短：读块时得到读写错误。
    #[test]
    fn read_block_reports_io_error_when_file_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let sst = open_sst(&path).unwrap();
        // 把文件截到只剩 10 个字节。已经打开的文件看到的是同一个文件，长度随之变化。
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(10).unwrap();

        let result = sst.read_block(1);

        assert!(matches!(result, Err(Error::Io(_))));
    }

    // 找块：key 落在某个块的范围里，就是那个块。
    #[test]
    fn find_block_index_for_keys_inside_a_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let sst = open_sst(&path).unwrap();

        assert_eq!(sst.find_block_index(b"a"), 0);
        assert_eq!(sst.find_block_index(b"b"), 0);
        assert_eq!(sst.find_block_index(b"c"), 0);
        assert_eq!(sst.find_block_index(b"d"), 1);
        assert_eq!(sst.find_block_index(b"e"), 1);
        assert_eq!(sst.find_block_index(b"f"), 1);
        assert_eq!(sst.find_block_index(b"g"), 2);
    }

    // 找块：key 落在两个块之间时，是后面那个块（它的第一条就是第一个更大的 key）。
    #[test]
    fn find_block_index_for_keys_between_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let sst = open_sst(&path).unwrap();

        assert_eq!(sst.find_block_index(b"cc"), 1);
        assert_eq!(sst.find_block_index(b"fz"), 2);
    }

    // 找块：比所有 key 都小是第 0 个块；比所有 key 都大时没有这样的块，返回块数。
    #[test]
    fn find_block_index_for_keys_outside_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let sst = open_sst(&path).unwrap();

        assert_eq!(sst.find_block_index(b"A"), 0);
        assert_eq!(sst.find_block_index(b"gg"), 3);
        assert_eq!(sst.find_block_index(b"z"), 3);
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
