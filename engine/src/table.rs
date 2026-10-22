//! SST：磁盘上的一个有序文件，由很多数据块、一份索引和一个 Bloom filter 组成，写完之后不再修改。
//!
//! 文件的样子（版本 2，所有整数都是大端）：
//!
//! ```text
//! ┌──────┬─────┬──────┬─────┬───┬──────┬─────┬────────┬─────┬────────────┬──────────────┬───────┬──────┐
//! │ 块 0 │ crc │ 块 1 │ crc │ … │ 索引 │ crc │ filter │ crc │ 索引的起点 │ filter 的起点 │ 版本号 │ 魔数 │
//! └──────┴─────┴──────┴─────┴───┴──────┴─────┴────────┴─────┴────────────┴──────────────┴───────┴──────┘
//!   数据块与它的校验和，一个接一个  每块一项     见 bloom.rs     4 字节        4 字节        4 字节  4 字节
//!
//! 索引：      块数（4 字节）│ 第 0 项 │ 第 1 项 │ …
//! 索引的一项： 块的起点（4 字节）│ 首 key 长度（2 字节）│ 首 key │ 末 key 长度（2 字节）│ 末 key
//! ```
//!
//! 与块的尾巴是同一个办法：内容在前，索引在后，最后几个字节指回索引。
//! 块的长度不记：它等于下一个块的起点减自己的起点，最后一个块用索引的起点来减。
//! 索引的长度也不记：它到 filter 的起点为止；filter 到末尾固定的 16 个字节为止。
//!
//! 每个 crc 是紧挨在它前面那一段（一个数据块、索引、filter）的 crc32，4 字节，只管那一段：
//! 一段的范围由上面的起点划出，最后 4 个字节是校验和，前面是内容。读一段时先核对校验和，
//! 对得上才解码。数据块各带各的校验和：读一个块只读这一段，不必读别的。
//!
//! 末尾的 16 个字节自己没有校验和：魔数与版本号各自核对，两个起点先检查范围，
//! 指错了位置，切出来的那一段的校验和就对不上。
//!
//! 版本 1 没有这些 crc，其余相同；版本 1 的文件照样能打开，只是读出的内容不做校验。
//!
//! 魔数确认"这是引擎写的 SST"，永远不变；版本号说明按哪一版格式写的，格式每改一次加一。
//!
//! 打开文件时把索引和 filter 读进内存，数据块留在磁盘上，用到哪个读哪个。
//! 读出来的块可以放进整个引擎共用的块缓存，下次直接从缓存里拿。

mod bloom;
mod builder;
mod iterator;

pub use bloom::{Bloom, key_hash};
pub use builder::SsTableBuilder;
pub use iterator::SsTableIterator;

use std::fs::File;
use std::io::Write;
use std::ops::Bound;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use bytes::{BufMut, Bytes};

use crate::block::Block;
use crate::error::{Error, Result};
use crate::stats::Stats;

/// 块缓存：从 `(SST 编号, 块下标)` 找到解码好的块。整个引擎共用一个。
///
/// SST 写完不再修改，编号也不重复使用，所以缓存里的块永远不会过期，不需要失效。
/// 多个线程可以同时读写；克隆它得到的是同一个缓存。
pub type BlockCache = moka::sync::Cache<(usize, usize), Arc<Block>>;

/// 新建一个块缓存，最多放 `capacity_bytes` 字节的块。
///
/// 每个块按 [`Block::memory_size`] 计入容量。超过容量时由缓存自己挑出不常用的块淘汰，
/// 淘汰稍有延迟，总量可能短暂超出。
pub fn new_block_cache(capacity_bytes: u64) -> BlockCache {
    BlockCache::builder()
        // 告诉缓存每个块"有多重"。容量是这些重量的总和上限，不是块的个数。
        .weigher(|_key, block: &Arc<Block>| {
            // 重量的类型是 u32。一个块不会接近 4 GB，超出时按最大值算。
            u32::try_from(block.memory_size()).unwrap_or(u32::MAX)
        })
        .max_capacity(capacity_bytes)
        .build()
}

/// 文件最后 4 个字节的魔数，用来确认这是引擎写的 SST。四个字节是 ASCII 的 "LSM1"。
///
/// 魔数不随格式改变；格式的变化记在 [`SST_FORMAT_VERSION`] 里。
pub const SST_MAGIC: u32 = 0x4C53_4D31;

/// 现在写出的 SST 的格式版本，放在魔数前面。打开时认得这一版和它之前的每一版。
///
/// - 版本 1：数据块、索引、Bloom filter。
/// - 版本 2：数据块、索引、filter 后面各跟一个 crc32（L15）。
pub const SST_FORMAT_VERSION: u32 = 2;

/// 认得的最老的格式版本。
const OLDEST_SST_FORMAT_VERSION: u32 = 1;

/// 版本 2 起每一段后面的校验和：crc32，4 字节。
const CHECKSUM_SIZE: usize = 4;

/// 写 SST 时 Bloom filter 的目标误判率。
pub const BLOOM_FALSE_POSITIVE_RATE: f64 = 0.01;

/// 文件末尾固定的部分有多长：索引的起点、filter 的起点、版本号、魔数，各 4 字节。
const FOOTER_SIZE: usize = 16;

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

/// 这个版本的文件里，每一段后面有没有校验和。
fn has_checksums(format_version: u32) -> bool {
    format_version >= 2
}

/// 把一段"内容 + 4 字节 crc32"切开并核对，返回内容。`what` 说明是哪一段，写进错误信息。
///
/// 这一段短得放不下校验和，或者校验和对不上时返回 [`Error::Corruption`]。
fn verify_checksum<'a>(section: &'a [u8], what: &str) -> Result<&'a [u8]> {
    if section.len() < CHECKSUM_SIZE {
        return Err(corrupt_sst(&format!("{what}：短得放不下校验和")));
    }
    let (content, checksum) = section.split_at(section.len() - CHECKSUM_SIZE);
    let stored = read_u32(checksum, 0);
    if stored != crc32fast::hash(content) {
        return Err(corrupt_sst(&format!("{what}：校验和不对")));
    }
    Ok(content)
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
    /// 文件里全部 key 的 Bloom filter。
    bloom: Bloom,
    /// 文件的格式版本：决定读块时有没有校验和要核对。
    format_version: u32,
    /// 读块时先查的缓存。`None` 表示不用缓存，每次都从文件读。
    block_cache: Option<Arc<BlockCache>>,
    /// 从文件读出一个块，就在这里记一次。`None` 表示不记。
    stats: Option<Arc<Stats>>,
}

impl SsTable {
    /// 打开一个已有的 SST 文件：从尾部读回索引和 Bloom filter，数据块留在磁盘上。
    ///
    /// 读块时先查 `block_cache`；给 `None` 就不用缓存。
    /// 版本 2 起，索引与 filter 先核对校验和再解码。文件不符合格式（包括不认得的版本号、
    /// 校验和对不上）时返回 [`Error::Corruption`]，读文件失败时返回 [`Error::Io`]。
    pub fn open(id: usize, block_cache: Option<Arc<BlockCache>>, file: FileObject) -> Result<Self> {
        // 从文件的尾巴倒着找。末尾固定的 16 个字节：索引的起点、filter 的起点、版本号、魔数。
        // 先确认文件够长，再做减法：无符号数相减不能得到负数。
        let file_size = file.size();
        if file_size < FOOTER_SIZE {
            return Err(corrupt_sst("文件太短，放不下末尾的 16 个字节"));
        }
        let footer_start = file_size - FOOTER_SIZE;
        let footer = file.read(footer_start, FOOTER_SIZE)?;
        let block_meta_start = read_u32(&footer, 0) as usize;
        let bloom_start = read_u32(&footer, 4) as usize;
        let format_version = read_u32(&footer, 8);
        let sst_magic = read_u32(&footer, 12);

        // 先认魔数，再认版本：魔数不对说明根本不是 SST，版本不对说明是别的版本写的 SST。
        if sst_magic != SST_MAGIC {
            return Err(corrupt_sst("末尾的魔数不对，这不是引擎写的 SST"));
        }
        if !(OLDEST_SST_FORMAT_VERSION..=SST_FORMAT_VERSION).contains(&format_version) {
            return Err(corrupt_sst(&format!(
                "格式版本是 {format_version}，只认得版本 {OLDEST_SST_FORMAT_VERSION} 到 {SST_FORMAT_VERSION}"
            )));
        }
        if block_meta_start > bloom_start || bloom_start > footer_start {
            return Err(corrupt_sst("索引或 filter 的起点超出了范围"));
        }

        // 索引占 [block_meta_start, bloom_start)，filter 占 [bloom_start, footer_start)。
        // 两段紧挨着，一次读出来再切开。
        let raw_meta_len = footer_start - block_meta_start;
        let raw_meta = file.read(block_meta_start, raw_meta_len)?;
        let (mut raw_block_meta, mut raw_bloom) = raw_meta.split_at(bloom_start - block_meta_start);
        if has_checksums(format_version) {
            raw_block_meta = verify_checksum(raw_block_meta, "索引")?;
            raw_bloom = verify_checksum(raw_bloom, "Bloom filter")?;
        }

        let block_meta = decode_block_meta(raw_block_meta)?;
        check_block_meta(&block_meta, block_meta_start)?;
        let bloom = Bloom::decode(raw_bloom)?;

        let first_key = block_meta[0].first_key.clone();
        let last_key = block_meta[block_meta.len() - 1].last_key.clone();
        Ok(Self {
            id,
            file,
            block_meta,
            block_meta_start,
            first_key,
            last_key,
            bloom,
            format_version,
            block_cache,
            stats: None,
        })
    }

    /// 让这个 SST 把"从文件读出了几个块"记进 `stats`。引擎在打开或写出 SST 之后调用。
    pub fn with_stats(mut self, stats: Arc<Stats>) -> Self {
        self.stats = Some(stats);
        self
    }

    /// 第 `block_index` 个数据块：缓存里有就直接拿，没有就从文件读出、解码，再放进缓存。
    /// 没有缓存时每次都从文件读。
    ///
    /// `block_index` 必须小于 `block_count()`。读文件失败或块的内容损坏时返回错误，
    /// 这时缓存里不放任何东西。
    pub fn read_block(&self, block_index: usize) -> Result<Arc<Block>> {
        if let Some(cache) = &self.block_cache {
            // 只用块下标不够：每个文件都有第 0 块。
            let key = (self.id, block_index);
            if let Some(block) = cache.get(&key) {
                return Ok(block);
            }
            // 两个线程同时没命中同一个块时，两边都会读一遍文件、各放一次。
            // 块的内容一样，后放的覆盖先放的，结果仍然正确，只是多读了一次。
            let block = self.read_block_from_file(block_index)?;
            cache.insert(key, Arc::clone(&block));
            return Ok(block);
        }
        self.read_block_from_file(block_index)
    }

    /// 从文件里读出第 `block_index` 个数据块并解码，不经过缓存。版本 2 起先核对块的校验和。
    ///
    /// `block_index` 必须小于 `block_count()`。读文件失败或块的内容损坏时返回错误。
    fn read_block_from_file(&self, block_index: usize) -> Result<Arc<Block>> {
        // 下标越界是调用方写错了，不是文件坏了，所以用断言而不是返回错误。
        assert!(
            block_index < self.block_count(),
            "块的下标 {block_index} 超出了块数 {}",
            self.block_count()
        );

        // 块占 [block_start, block_end)：终点是下一个块的起点，最后一个块到索引的起点为止。
        // 版本 2 起这一段的最后 4 个字节是块的校验和。
        let block_start = self.block_meta[block_index].offset;
        let block_end = match self.block_meta.get(block_index + 1) {
            Some(next_meta) => next_meta.offset,
            None => self.block_meta_start,
        };

        // `read` 要的是起点和长度，不是起点和终点。
        let block_len = block_end - block_start;
        let section = self.file.read(block_start, block_len)?;
        let encoded = if has_checksums(self.format_version) {
            verify_checksum(&section, &format!("第 {block_index} 个数据块"))?
        } else {
            &section[..]
        };
        let block = Block::decode(encoded)?;
        if let Some(stats) = &self.stats {
            stats.add_blocks_read(1);
        }
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
            if self.block_meta[mid].last_key < key {
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

    /// `key` 有没有可能在这个文件里：它落在 `[首 key, 末 key]` 之内。
    ///
    /// 返回 `false` 时文件里一定没有它，不用读任何块；返回 `true` 时也不一定有，
    /// 它可能落在两条记录之间。
    pub fn may_contain_key(&self, key: &[u8]) -> bool {
        self.first_key() <= key && key <= self.last_key()
    }

    /// 问这个文件的 Bloom filter：哈希值为 `key_hash` 的 key 可能在不在文件里。
    ///
    /// `key_hash` 要用 [`key_hash`] 算。返回 `false` 时文件里一定没有这个 key，
    /// 不用读任何块；返回 `true` 时可能有，也可能是误报。
    pub fn bloom_may_contain(&self, key_hash: u32) -> bool {
        self.bloom.may_contain(key_hash)
    }

    /// 文件里全部 key 的 Bloom filter。
    pub fn bloom(&self) -> &Bloom {
        &self.bloom
    }

    /// 范围 `(lower, upper)` 与文件的 `[首 key, 末 key]` 有没有交集。
    ///
    /// 返回 `false` 时文件里一定没有这个范围里的记录，扫描可以不管这个文件。
    pub fn overlaps_range(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> bool {
        // 范围的下界不能在文件的末 key 之后。
        let lower_ok = match lower {
            Bound::Included(lower) => lower <= self.last_key(),
            Bound::Excluded(lower) => lower < self.last_key(),
            Bound::Unbounded => true,
        };
        // 范围的上界不能在文件的首 key 之前。
        let upper_ok = match upper {
            Bound::Included(upper) => self.first_key() <= upper,
            Bound::Excluded(upper) => self.first_key() < upper,
            Bound::Unbounded => true,
        };
        lower_ok && upper_ok
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

    /// 文件的格式版本。
    pub fn format_version(&self) -> u32 {
        self.format_version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockIterator;
    use crate::iterators::vec_iterator::{collect_all, expected};
    use std::io::ErrorKind;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// 七条记录，块的目标大小 32 恰好放三条：三个块 [a, c]、[d, f]、[g, g]，
    /// 各 32、32、12 字节，每块后面跟 4 字节的校验和，起点依次是 0、36、72，索引从 88 开始。
    /// 返回文件的路径。
    fn write_three_block_sst(dir: &TempDir) -> PathBuf {
        write_three_block_sst_with_version(dir, SST_FORMAT_VERSION)
    }

    /// 与 `write_three_block_sst` 的记录相同，按给定的格式版本写。
    fn write_three_block_sst_with_version(dir: &TempDir, format_version: u32) -> PathBuf {
        let path = dir.path().join("1.sst");
        let mut builder = SsTableBuilder::new(32).with_format_version(format_version);
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
        builder.build(1, None, &path).unwrap();
        path
    }

    /// 打开路径上的 SST。
    fn open_sst(path: &Path) -> Result<SsTable> {
        SsTable::open(1, None, FileObject::open(path)?)
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
        let mut builder = SsTableBuilder::new(32);
        for key in ["a", "b", "c", "d", "e", "f", "g"] {
            builder.add(key.as_bytes(), b"1");
        }
        let written = builder.build(7, None, &path).unwrap();

        let opened = SsTable::open(7, None, FileObject::open(&path).unwrap()).unwrap();

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
        assert_eq!(sst.block_meta_start(), 88);
        let summary: Vec<(usize, &[u8], &[u8])> = sst
            .block_meta()
            .iter()
            .map(|meta| (meta.offset, &meta.first_key[..], &meta.last_key[..]))
            .collect();
        assert_eq!(
            summary,
            vec![
                (0, &b"a"[..], &b"c"[..]),
                (36, &b"d"[..], &b"f"[..]),
                (72, &b"g"[..], &b"g"[..]),
            ]
        );
    }

    // 文件短得连末尾的 16 个字节都放不下：内容损坏，而不是越界 panic。
    #[test]
    fn open_rejects_file_shorter_than_the_footer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");

        assert_open_reports_corruption(&path, b"");
        assert_open_reports_corruption(&path, b"LSM1");
        assert_open_reports_corruption(
            &path,
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, b'L', b'S', b'M'],
        );
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

    // 末尾 16 个字节依次是索引的起点、filter 的起点、版本号、魔数；读错位置这里会发现。
    #[test]
    fn open_reads_footer_fields_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        // 索引：块数 4 字节，三项各 4 + 3 + 3 字节，共 34 字节，再加 4 字节的校验和，
        // 所以 filter 从 126 开始。
        assert_eq!(&bytes[len - 16..len - 12], &[0, 0, 0, 88]);
        assert_eq!(&bytes[len - 12..len - 8], &[0, 0, 0, 126]);
        assert_eq!(&bytes[len - 8..len - 4], &[0, 0, 0, 2]);
        assert_eq!(&bytes[len - 4..], b"LSM1");

        let sst = open_sst(&path).unwrap();

        assert_eq!(sst.block_meta_start(), 88);
        assert_eq!(sst.block_count(), 3);
    }

    // 魔数对、版本号不认得：是别的版本写的 SST，拒绝打开。
    #[test]
    fn open_rejects_unknown_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let original = std::fs::read(&path).unwrap();
        let len = original.len();

        for version in [0u32, SST_FORMAT_VERSION + 1, 0xffff_ffff] {
            let mut bytes = original.clone();
            bytes[len - 8..len - 4].copy_from_slice(&version.to_be_bytes());
            std::fs::write(&path, &bytes).unwrap();
            match open_sst(&path) {
                Err(Error::Corruption(message)) => assert!(message.contains("版本"), "{message}"),
                Err(other) => panic!("应当是 Corruption，实际是 {other:?}"),
                Ok(_) => panic!("版本 {version} 的文件不应该打得开"),
            }
        }
    }

    // 索引或 filter 的起点指到了文件外面、指进了末尾的 16 个字节，或者两者顺序颠倒。
    #[test]
    fn open_rejects_meta_offset_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let original = std::fs::read(&path).unwrap();
        let len = original.len();
        let footer_start = (len - 16) as u32;

        let mut meta_far_beyond = original.clone();
        meta_far_beyond[len - 16..len - 12].copy_from_slice(&[0, 0, 0xff, 0xff]);
        assert_open_reports_corruption(&path, &meta_far_beyond);

        // filter 的起点恰好比"末尾 16 字节的开头"大 1。
        let mut bloom_just_beyond = original.clone();
        bloom_just_beyond[len - 12..len - 8].copy_from_slice(&(footer_start + 1).to_be_bytes());
        assert_open_reports_corruption(&path, &bloom_just_beyond);

        // 索引的起点在 filter 的起点之后。
        let mut meta_after_bloom = original.clone();
        meta_after_bloom[len - 16..len - 12].copy_from_slice(&[0, 0, 0, 127]);
        assert_open_reports_corruption(&path, &meta_after_bloom);
    }

    // 索引的起点指错了地方（指到了数据块中间）：索引解不出来。
    #[test]
    fn open_rejects_meta_offset_pointing_into_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        bytes[len - 16..len - 12].copy_from_slice(&[0, 0, 0, 5]);

        assert_open_reports_corruption(&path, &bytes);
    }

    // filter 坏了（它的最后一个字节 k 变成 0，k 之后是 4 字节的校验和与末尾的 16 字节）：打不开。
    #[test]
    fn open_rejects_corrupted_bloom() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        bytes[len - 21] = 0;

        assert_open_reports_corruption(&path, &bytes);
    }

    // 打开时读回的 filter 与写的时候一样：文件里的每个 key 都回答可能有。
    #[test]
    fn open_reads_back_the_bloom_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        let mut builder = SsTableBuilder::new(32);
        for key in ["a", "b", "c", "d", "e", "f", "g"] {
            builder.add(key.as_bytes(), b"1");
        }
        let written = builder.build(1, None, &path).unwrap();

        let opened = open_sst(&path).unwrap();

        assert_eq!(opened.bloom(), written.bloom());
        for key in ["a", "b", "c", "d", "e", "f", "g"] {
            assert!(opened.bloom_may_contain(key_hash(key.as_bytes())), "{key}");
        }
    }

    // 落在文件的 key 范围里、文件里却没有的 key，filter 能排除掉绝大多数。
    #[test]
    fn bloom_rejects_most_absent_keys_inside_the_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        let mut builder = SsTableBuilder::new(4096);
        // 只写 0 到 1998 的偶数号 key；1 到 1997 的奇数号 key 落在范围里但不存在。
        for i in (0..2000).step_by(2) {
            builder.add(format!("key-{i:05}").as_bytes(), b"1");
        }
        let sst = builder.build(1, None, &path).unwrap();

        let absent_keys: Vec<String> = (1..1999)
            .step_by(2)
            .map(|i| format!("key-{i:05}"))
            .collect();
        let mut false_positives = 0;
        for key in &absent_keys {
            assert!(sst.may_contain_key(key.as_bytes()));
            if sst.bloom_may_contain(key_hash(key.as_bytes())) {
                false_positives += 1;
            }
        }
        // 999 个不存在的 key，目标误判率 1%，留出余量。
        assert!(false_positives < 30, "误报了 {false_positives} 次");
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
        let sst = builder.build(1, None, &path).unwrap();

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
        // 第一个块占 [0, 32)，它的条数在最后两个字节；校验和在 [32, 36)。
        bytes[30] = 0;
        bytes[31] = 0;
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

    // ---------- 块缓存 ----------

    /// 用 `cache` 打开 [`write_three_block_sst`] 写出的文件，编号是 `id`。
    fn open_cached(path: &Path, id: usize, cache: &Arc<BlockCache>) -> SsTable {
        SsTable::open(id, Some(Arc::clone(cache)), FileObject::open(path).unwrap()).unwrap()
    }

    // 有缓存时，同一个块第二次读到的就是第一次放进缓存的那一个，没有重新读文件。
    #[test]
    fn cached_read_returns_the_same_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let cache = Arc::new(new_block_cache(1 << 20));
        let sst = open_cached(&path, 1, &cache);

        let first = sst.read_block(1).unwrap();
        let second = sst.read_block(1).unwrap();

        assert!(Arc::ptr_eq(&first, &second), "第二次读没有用上缓存");
        assert!(cache.contains_key(&(1, 1)));
        assert_eq!(
            records_in_block(&sst, 1),
            expected(&[("d", "4"), ("e", "5"), ("f", "6")])
        );
    }

    // 没有缓存时每次都从文件读，得到的是新解码出来的块。
    #[test]
    fn uncached_read_decodes_a_new_block_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let sst = open_sst(&path).unwrap();

        let first = sst.read_block(1).unwrap();
        let second = sst.read_block(1).unwrap();

        assert!(!Arc::ptr_eq(&first, &second));
    }

    // 缓存命中之后，文件被改坏也不影响：块是从缓存里拿的。
    #[test]
    fn cached_block_survives_a_damaged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let cache = Arc::new(new_block_cache(1 << 20));
        let sst = open_cached(&path, 1, &cache);
        sst.read_block(0).unwrap();

        // 把第 0 块的字节全部改成 0xff；文件长度不变。
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..32].fill(0xff);
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(
            records_in_block(&sst, 0),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // 读文件失败时缓存里不放东西；文件恢复后能读到正确的块。
    #[test]
    fn failed_read_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let good_bytes = std::fs::read(&path).unwrap();
        let cache = Arc::new(new_block_cache(1 << 20));
        let sst = open_cached(&path, 1, &cache);

        let mut bad_bytes = good_bytes.clone();
        bad_bytes[..32].fill(0xff);
        std::fs::write(&path, &bad_bytes).unwrap();
        assert!(sst.read_block(0).is_err());
        assert!(!cache.contains_key(&(1, 0)), "读失败的块不应当放进缓存");

        std::fs::write(&path, &good_bytes).unwrap();
        assert_eq!(
            records_in_block(&sst, 0),
            expected(&[("a", "1"), ("b", "2"), ("c", "3")])
        );
    }

    // 两个 SST 共用一个缓存：块下标相同、编号不同，各读到各自的块。
    #[test]
    fn ssts_sharing_a_cache_do_not_mix_their_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(new_block_cache(1 << 20));
        let first_path = dir.path().join("00001.sst");
        let second_path = dir.path().join("00002.sst");
        for (path, value) in [(&first_path, "first"), (&second_path, "second")] {
            let mut builder = SsTableBuilder::new(4096);
            builder.add(b"k", value.as_bytes());
            builder.build(0, None, path).unwrap();
        }
        let first = open_cached(&first_path, 1, &cache);
        let second = open_cached(&second_path, 2, &cache);

        assert_eq!(records_in_block(&first, 0), expected(&[("k", "first")]));
        assert_eq!(records_in_block(&second, 0), expected(&[("k", "second")]));
    }

    // 容量按块的字节数算：放不下所有的块时会淘汰，总量不超过容量。
    #[test]
    fn cache_capacity_is_counted_in_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        let mut builder = SsTableBuilder::new(64);
        for i in 0..200 {
            builder.add(format!("key-{i:03}").as_bytes(), b"value");
        }
        builder.build(1, None, &path).unwrap();
        // 每个块约 64 字节，容量只够放十来个。
        let capacity = 1000;
        let cache = Arc::new(new_block_cache(capacity));
        let sst = open_cached(&path, 1, &cache);
        assert!(sst.block_count() > 20);

        for block_index in 0..sst.block_count() {
            sst.read_block(block_index).unwrap();
        }
        // 缓存的淘汰稍有延迟，先让它把积压的工作做完再看。
        cache.run_pending_tasks();

        assert!(
            cache.weighted_size() <= capacity,
            "缓存里有 {} 字节",
            cache.weighted_size()
        );
        assert!(
            (cache.entry_count() as usize) < sst.block_count(),
            "一个块都没有被淘汰"
        );
        assert!(cache.entry_count() > 0);
    }

    // ---------- 按 key 范围排除文件 ----------

    /// 打开 [`write_three_block_sst`] 写出的文件：key 是 a 到 g。
    fn a_to_g_sst(dir: &TempDir) -> SsTable {
        open_sst(&write_three_block_sst(dir)).unwrap()
    }

    // 首 key 与末 key 本身算在范围内；范围之外的一定没有。
    #[test]
    fn may_contain_key_checks_both_ends() {
        let dir = tempfile::tempdir().unwrap();
        let sst = a_to_g_sst(&dir);

        assert!(sst.may_contain_key(b"a"));
        assert!(sst.may_contain_key(b"d"));
        assert!(sst.may_contain_key(b"g"));
        assert!(!sst.may_contain_key(b"0"));
        assert!(!sst.may_contain_key(b"h"));
        assert!(!sst.may_contain_key(b"gg"));
    }

    // 落在范围内、文件里却没有的 key，也回答"可能有"：这里只看两端。
    #[test]
    fn may_contain_key_is_only_about_the_range() {
        let dir = tempfile::tempdir().unwrap();
        let sst = a_to_g_sst(&dir);

        assert!(sst.may_contain_key(b"cc"));
    }

    // 下界的三种写法：恰好碰到末 key 时，包含算相交，不包含不算。
    #[test]
    fn overlaps_range_checks_the_lower_bound_against_the_last_key() {
        let dir = tempfile::tempdir().unwrap();
        let sst = a_to_g_sst(&dir);

        assert!(sst.overlaps_range(Bound::Included(b"g"), Bound::Unbounded));
        assert!(!sst.overlaps_range(Bound::Excluded(b"g"), Bound::Unbounded));
        assert!(!sst.overlaps_range(Bound::Included(b"h"), Bound::Unbounded));
        assert!(sst.overlaps_range(Bound::Excluded(b"f"), Bound::Unbounded));
        assert!(sst.overlaps_range(Bound::Unbounded, Bound::Unbounded));
    }

    // 上界的三种写法：恰好碰到首 key 时，包含算相交，不包含不算。
    #[test]
    fn overlaps_range_checks_the_upper_bound_against_the_first_key() {
        let dir = tempfile::tempdir().unwrap();
        let sst = a_to_g_sst(&dir);

        assert!(sst.overlaps_range(Bound::Unbounded, Bound::Included(b"a")));
        assert!(!sst.overlaps_range(Bound::Unbounded, Bound::Excluded(b"a")));
        assert!(!sst.overlaps_range(Bound::Unbounded, Bound::Included(b"0")));
        assert!(sst.overlaps_range(Bound::Unbounded, Bound::Excluded(b"b")));
    }

    // 两端都给：范围在文件里面、盖住整个文件、在文件之前、在文件之后。
    #[test]
    fn overlaps_range_with_both_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let sst = a_to_g_sst(&dir);

        assert!(sst.overlaps_range(Bound::Included(b"c"), Bound::Excluded(b"e")));
        assert!(sst.overlaps_range(Bound::Included(b"0"), Bound::Included(b"z")));
        assert!(!sst.overlaps_range(Bound::Included(b"0"), Bound::Excluded(b"a")));
        assert!(!sst.overlaps_range(Bound::Excluded(b"g"), Bound::Included(b"z")));
        // 只看范围，不看文件里实际有没有记录：(a, b) 里虽然没有记录，也算相交。
        assert!(sst.overlaps_range(Bound::Excluded(b"a"), Bound::Excluded(b"b")));
    }

    // ---------- 校验和（L15） ----------

    /// 打开文件并读出每一个块；任何一步出错都返回错误。
    fn open_and_read_every_block(path: &Path) -> Result<Vec<(String, String)>> {
        let sst = open_sst(path)?;
        let mut records = Vec::new();
        for block_index in 0..sst.block_count() {
            let mut iter = BlockIterator::new(sst.read_block(block_index)?);
            records.extend(collect_all(&mut iter));
        }
        Ok(records)
    }

    // 改坏文件里任何一个字节：打开或读块时一定报错，不会读出错的内容，也不会 panic。
    #[test]
    fn every_corrupted_byte_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let original = std::fs::read(&path).unwrap();

        for at in 0..original.len() {
            let mut bytes = original.clone();
            bytes[at] ^= 0xff;
            std::fs::write(&path, &bytes).unwrap();

            let result = open_and_read_every_block(&path);

            assert!(
                matches!(result, Err(Error::Corruption(_))),
                "改坏第 {at} 个字节之后：{result:?}"
            );
        }
    }

    // 版本 1 没有校验和：改坏一个 value 的字节，读出的就是错的内容，没有任何报错。
    // 这正是版本 2 要解决的问题。
    #[test]
    fn version_1_does_not_notice_a_corrupted_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst_with_version(&dir, 1);
        let mut bytes = std::fs::read(&path).unwrap();
        // 第一个块的第一条记录：key 长度 2 字节、key "a"、value 长度 2 字节、value "1"。
        assert_eq!(bytes[7], b'1');
        bytes[7] = b'9';
        std::fs::write(&path, &bytes).unwrap();

        let records = open_and_read_every_block(&path).unwrap();

        assert_eq!(records[0], ("a".to_string(), "9".to_string()));
    }

    // 版本 1 的文件照样能打开、读块、问 filter。
    #[test]
    fn version_1_files_are_still_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst_with_version(&dir, 1);

        let sst = open_sst(&path).unwrap();

        assert_eq!(sst.format_version(), 1);
        assert_eq!(sst.block_meta_start(), 76);
        let records = open_and_read_every_block(&path).unwrap();
        let keys: Vec<&str> = records.iter().map(|(key, _value)| key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c", "d", "e", "f", "g"]);
        assert!(sst.bloom_may_contain(key_hash(b"d")));
    }

    // 读一个版本 1 的文件、写成新的文件（compaction 就是这样）：新文件是版本 2，内容不变。
    #[test]
    fn rewriting_a_version_1_file_produces_version_2() {
        let dir = tempfile::tempdir().unwrap();
        use crate::iterators::StorageIterator;
        let old = Arc::new(open_sst(&write_three_block_sst_with_version(&dir, 1)).unwrap());
        let mut iter = SsTableIterator::new_and_seek_to_first(old).unwrap();
        let mut builder = SsTableBuilder::new(32);
        while iter.is_valid() {
            builder.add(iter.key(), iter.value());
            iter.next().unwrap();
        }
        let new_path = dir.path().join("2.sst");

        let written = builder.build(2, None, &new_path).unwrap();

        assert_eq!(written.format_version(), SST_FORMAT_VERSION);
        let records = open_and_read_every_block(&new_path).unwrap();
        assert_eq!(records.len(), 7);
        assert_eq!(records[6], ("g".to_string(), "7".to_string()));
    }

    // 每个校验和只算它前面那一段：块、索引、filter 的字节范围各自核对得上。
    #[test]
    fn each_checksum_covers_exactly_its_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        let crc_at = |at: usize| read_u32(&bytes, at);

        // 三个块：[0, 32)、[36, 68)、[72, 84)，校验和紧跟在后面。
        for (start, end) in [(0, 32), (36, 68), (72, 84)] {
            assert_eq!(crc_at(end), crc32fast::hash(&bytes[start..end]));
        }
        // 索引 [88, 122)，filter [126, len - 20)。
        assert_eq!(crc_at(122), crc32fast::hash(&bytes[88..122]));
        assert_eq!(crc_at(len - 20), crc32fast::hash(&bytes[126..len - 20]));
    }

    // 块的内容被改坏、改完仍能解码（改的是 value）：校验和发现了，报告是哪个块。
    #[test]
    fn read_block_reports_bad_checksum_even_if_block_still_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        // 第二个块从 36 开始，第一条记录 "d" → "4" 的 value 在 36 + 7。
        assert_eq!(bytes[43], b'4');
        bytes[43] = b'9';
        std::fs::write(&path, &bytes).unwrap();
        let sst = open_sst(&path).unwrap();

        match sst.read_block(1) {
            Err(Error::Corruption(message)) => {
                assert!(message.contains("第 1 个数据块：校验和"), "{message}")
            }
            Err(other) => panic!("应当是 Corruption，实际是 {other:?}"),
            Ok(_) => panic!("改坏的块不应该读得出来"),
        }
        assert_eq!(records_in_block(&sst, 0).len(), 3);
    }

    // 索引或 filter 的内容被改坏：打开时就报告是哪一段的校验和不对。
    #[test]
    fn open_reports_bad_checksum_of_index_and_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_three_block_sst(&dir);
        let original = std::fs::read(&path).unwrap();

        for (at, what) in [(100, "索引：校验和"), (130, "Bloom filter：校验和")] {
            let mut bytes = original.clone();
            bytes[at] ^= 0x01;
            std::fs::write(&path, &bytes).unwrap();
            match open_sst(&path) {
                Err(Error::Corruption(message)) => assert!(message.contains(what), "{message}"),
                Err(other) => panic!("应当是 Corruption，实际是 {other:?}"),
                Ok(_) => panic!("改坏第 {at} 个字节之后不应该打得开"),
            }
        }
    }

    // 一段短得放不下 4 字节的校验和：报告损坏，不越界。
    #[test]
    fn verify_checksum_rejects_sections_shorter_than_the_checksum() {
        for section in [&[][..], &[1, 2, 3][..]] {
            assert!(matches!(
                verify_checksum(section, "测试"),
                Err(Error::Corruption(_))
            ));
        }
        let mut section = b"abc".to_vec();
        section.extend_from_slice(&crc32fast::hash(b"abc").to_be_bytes());
        assert_eq!(verify_checksum(&section, "测试").unwrap(), b"abc");
    }
}
