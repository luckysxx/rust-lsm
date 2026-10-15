//! 数据块：SST 里最小的读取单位，一段字节里放着若干条按 key 排好序的记录。
//!
//! 编码后的样子（所有整数都是 2 字节、大端）：
//!
//! ```text
//! ┌──────────────────────────────┬──────────────────────────────┬──────┐
//! │ 记录 0 │ 记录 1 │ … │ 记录 n-1 │ 偏移 0 │ 偏移 1 │ … │ 偏移 n-1 │ 条数  │
//! └──────────────────────────────┴──────────────────────────────┴──────┘
//!   数据区                          偏移数组                       尾
//!
//! 一条记录：  共有前缀的长度 │ 剩余部分的长度 │ key 的剩余部分 │ value 长度 │ value
//! ```
//!
//! 偏移 i 是记录 i 在数据区里的起点。偏移数组和条数放在尾部，是因为构建时
//! 记录一条一条追加，封口之前不知道一共有几条；读的时候从最后 2 字节倒着找回来。
//!
//! key 做了前缀压缩：每个 key 只存它与块里**第一个 key** 不同的部分。完整的 key 是
//! "第一个 key 的前（共有前缀的长度）个字节"接上"剩余部分"。第一条记录的共有前缀
//! 长度是 0，key 完整存放。每个 key 只依赖第一个 key，所以任何一条记录都能单独
//! 还原出 key，偏移数组上的二分查找照常可用。
//!
//! 记录的格式只有本模块的三个文件知道：块之外只通过构建器的 `add`、块的编码与解码、
//! 游标的 `key`、`value`、`seek_to_key` 访问记录。改格式时，改动只在这里。
//!
//! 三个角色各占一个文件：
//!
//! - [`BlockBuilder`]（`block/builder.rs`）：逐条追加记录，做出一个块。
//! - [`Block`]（本文件）：做好的块，可以编码成字节、从字节解码。
//! - [`BlockIterator`]（`block/iterator.rs`）：块上的游标。

mod builder;
mod iterator;

pub use builder::BlockBuilder;
pub use iterator::BlockIterator;

use bytes::BufMut;

use crate::error::{Error, Result};

/// 块里每个整数（长度、偏移、条数）占的字节数。
const SIZEOF_U16: usize = 2;

/// 单个 key 的最大长度：长度字段只有 2 字节。
pub const MAX_KEY_LEN: usize = u16::MAX as usize;

/// 单个 value 的最大长度：长度字段只有 2 字节。
pub const MAX_VALUE_LEN: usize = u16::MAX as usize;

/// 一个做好的块。创建之后不再修改。
///
/// 内存里不把记录拆成一个个（key，value），而是保留数据区的原始字节，
/// 另外只把偏移数组解出来；要看哪条记录，就按偏移到数据区里现场解析。
pub struct Block {
    /// 数据区：所有记录首尾相接，与编码后的前半段逐字节相同。
    data: Vec<u8>,
    /// 每条记录在数据区里的起点，按记录的顺序排列。
    offsets: Vec<u16>,
}

impl Block {
    /// 块里有几条记录。
    pub fn record_count(&self) -> usize {
        self.offsets.len()
    }

    /// 块在内存里大约占多少字节：数据区加偏移数组。块缓存用它计算容量。
    ///
    /// 不含两个 `Vec` 自身的字段和内存分配器的额外开销，所以是一个估计。
    pub fn memory_size(&self) -> usize {
        self.data.len() + self.offsets.len() * SIZEOF_U16
    }

    /// 编码成字节：数据区，接着是偏移数组，最后是条数。
    pub fn encode(&self) -> Vec<u8> {
        let mut encoded = self.data.clone();
        for offset in &self.offsets {
            encoded.put_u16(*offset);
        }
        encoded.put_u16(self.offsets.len() as u16);
        encoded
    }

    /// 从 `encode` 产生的字节还原出块。
    ///
    /// 传入的字节不符合格式（被截断、被改动）时返回 [`Error::Corruption`]，不会 panic。
    /// 通过检查的块，之后按下标取记录、在上面建游标都不会越界。
    /// 只检查结构，不检查 key 是否按顺序排列，也发现不了"格式合法但内容被改过"的
    /// 情况，后者要靠校验和。
    pub fn decode(encoded: &[u8]) -> Result<Self> {
        check_encoded_block(encoded)?;

        // 从尾巴倒着找：最后 2 字节是条数。
        let count_at = encoded.len() - SIZEOF_U16;
        let count = read_u16(encoded, count_at) as usize;

        // 条数前面是偏移数组；它的起点同时也是数据区的终点。
        let offsets_start = count_at - count * SIZEOF_U16;
        let mut offsets = Vec::new();
        for i in 0..count {
            offsets.push(read_u16(encoded, offsets_start + i * SIZEOF_U16));
        }

        let data = encoded[0..offsets_start].to_vec();
        Ok(Block { data, offsets })
    }

    /// 第 `index` 条记录的三段：与第一个 key 共有的前缀长度、key 的剩余部分、value。
    ///
    /// 返回的字节借自块的数据区，不拷贝。`index` 必须小于 `record_count()`。
    fn entry(&self, index: usize) -> (usize, &[u8], &[u8]) {
        let record_start = self.offsets[index] as usize;

        // 记录的最前面是两个长度：共有前缀的长度、剩余部分的长度。剩余部分紧跟其后。
        let overlap_len = read_u16(&self.data, record_start) as usize;
        let rest_len = read_u16(&self.data, record_start + SIZEOF_U16) as usize;
        let rest_start = record_start + SIZEOF_U16 * 2;
        let rest_end = rest_start + rest_len;

        // value 的长度紧跟在剩余部分后面，所以要先知道剩余部分在哪里结束。
        let value_len = read_u16(&self.data, rest_end) as usize;
        let value_start = rest_end + SIZEOF_U16;
        let value_end = value_start + value_len;

        let rest = &self.data[rest_start..rest_end];
        let value = &self.data[value_start..value_end];
        (overlap_len, rest, value)
    }

    /// 块里的第一个 key。它的共有前缀长度是 0，剩余部分就是完整的 key。
    fn first_key(&self) -> &[u8] {
        let (_overlap_len, rest, _value) = self.entry(0);
        rest
    }

    /// 把第 `index` 条记录的完整 key 写进 `buf`，`buf` 原来的内容清掉。
    ///
    /// `buf` 由调用方反复使用：容量够时不再分配内存。`index` 必须小于 `record_count()`。
    fn key_at(&self, index: usize, buf: &mut Vec<u8>) {
        let (overlap_len, rest, _value) = self.entry(index);
        let first_key = self.first_key();
        buf.clear();
        buf.extend_from_slice(&first_key[..overlap_len]);
        buf.extend_from_slice(rest);
    }

    /// 第 `index` 条记录的 value，借自块的数据区。`index` 必须小于 `record_count()`。
    fn value_at(&self, index: usize) -> &[u8] {
        let (_overlap_len, _rest, value) = self.entry(index);
        value
    }
}

/// 检查一段字节是不是一个结构完整的块；不是的话说明哪里不对。
///
/// 检查的内容：读得出条数，条数不为 0；偏移数组没有超出块；每条记录的起点从 0
/// 开始、严格递增、落在数据区里；每条记录里的三个长度恰好把这条记录占满；
/// 第一条记录的共有前缀长度为 0；共有前缀不超过第一个 key 的长度；key 不为空。
fn check_encoded_block(encoded: &[u8]) -> Result<()> {
    if encoded.len() < SIZEOF_U16 {
        return Err(corrupt_block("长度不足 2 字节，读不出条数"));
    }
    let count_at = encoded.len() - SIZEOF_U16;
    let count = read_u16(encoded, count_at) as usize;
    if count == 0 {
        return Err(corrupt_block("条数为 0"));
    }
    if count * SIZEOF_U16 > count_at {
        return Err(corrupt_block("偏移数组超出了块的长度"));
    }
    let data_end = count_at - count * SIZEOF_U16;

    // 第一个 key 的长度，读第一条记录时得到。后面的记录的共有前缀不能比它长。
    let mut first_key_len = 0;
    for i in 0..count {
        // 第 i 条记录占 [record_start, record_end)：终点是下一条的起点，最后一条到数据区末尾。
        let record_start = read_u16(encoded, data_end + i * SIZEOF_U16) as usize;
        let record_end = if i + 1 < count {
            read_u16(encoded, data_end + (i + 1) * SIZEOF_U16) as usize
        } else {
            data_end
        };
        if i == 0 && record_start != 0 {
            return Err(corrupt_block("第一条记录的起点不是 0"));
        }
        if record_start >= record_end || record_end > data_end {
            return Err(corrupt_block("记录的起点没有递增，或者超出了数据区"));
        }

        // 记录里依次是：共有前缀的长度、剩余部分的长度、剩余部分、value 长度、value，
        // 合起来要恰好等于记录的长度。
        let record_len = record_end - record_start;
        if record_len < SIZEOF_U16 * 3 {
            return Err(corrupt_block("记录太短，放不下三个长度字段"));
        }
        let overlap_len = read_u16(encoded, record_start) as usize;
        let rest_len = read_u16(encoded, record_start + SIZEOF_U16) as usize;
        if i == 0 {
            if overlap_len != 0 {
                return Err(corrupt_block("第一条记录的共有前缀长度不是 0"));
            }
            first_key_len = rest_len;
        }
        if overlap_len > first_key_len {
            return Err(corrupt_block("共有前缀比第一个 key 还长"));
        }
        if overlap_len + rest_len == 0 {
            return Err(corrupt_block("key 的长度为 0"));
        }
        let value_len_at = record_start + SIZEOF_U16 * 2 + rest_len;
        if value_len_at + SIZEOF_U16 > record_end {
            return Err(corrupt_block("key 的剩余部分超出了记录"));
        }
        let value_len = read_u16(encoded, value_len_at) as usize;
        if value_len_at + SIZEOF_U16 + value_len != record_end {
            return Err(corrupt_block("value 的长度与记录的范围对不上"));
        }
    }
    Ok(())
}

/// 造一个"数据块损坏"的错误。
fn corrupt_block(message: &str) -> Error {
    Error::Corruption(format!("数据块：{message}"))
}

/// 读出 `bytes[at]` 与 `bytes[at + 1]` 这 2 个字节表示的整数（大端：高位字节在前）。
///
/// 相当于 Go 的 `binary.BigEndian.Uint16(bytes[at:])`。
fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用字符串写出记录，做成一个块。`block_size` 给得足够大，所有记录都放得下。
    fn block_of(pairs: &[(&str, &str)]) -> Block {
        let mut builder = BlockBuilder::new(4096);
        for (key, value) in pairs {
            assert!(builder.add(key.as_bytes(), value.as_bytes()));
        }
        builder.build()
    }

    /// 第 `index` 条记录的完整 key 和 value，拷贝出来方便比较。
    fn record(block: &Block, index: usize) -> (Vec<u8>, Vec<u8>) {
        let mut key = Vec::new();
        block.key_at(index, &mut key);
        let value = block.value_at(index).to_vec();
        (key, value)
    }

    /// 期望的一条记录，写成与 `record` 相同的类型。
    fn kv(key: &[u8], value: &[u8]) -> (Vec<u8>, Vec<u8>) {
        (key.to_vec(), value.to_vec())
    }

    // ---------- 编码 ----------

    // 格式测试：编码的结果逐字节确定。第二条记录与第一个 key 共有前缀 "c"，只存 "ow"。
    #[test]
    fn encode_lays_out_data_then_offsets_then_count() {
        let block = block_of(&[("cat", "meow"), ("cow", "moo")]);

        let encoded = block.encode();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // 记录 0：共有前缀 0，剩余部分长度 3，"cat"，value 长度 4，"meow"
            0, 0,   0, 3, b'c', b'a', b't',   0, 4, b'm', b'e', b'o', b'w',
            // 记录 1：共有前缀 1（"c"），剩余部分长度 2，"ow"，value 长度 3，"moo"
            0, 1,   0, 2, b'o', b'w',         0, 3, b'm', b'o', b'o',
            // 偏移数组：记录 0 从 0 开始，记录 1 从 13 开始
            0, 0, 0, 13,
            // 条数
            0, 2,
        ];
        assert_eq!(encoded, expected);
    }

    // 构建器估算的大小与真正编码出来的长度一致，有共有前缀时也一样。
    #[test]
    fn encoded_length_matches_builder_estimate() {
        let mut builder = BlockBuilder::new(4096);
        assert!(builder.add(b"a", b"1"));
        assert!(builder.add(b"ab", b""));
        assert!(builder.add(b"abc", b"333"));
        assert!(builder.add(b"b", b"4"));
        let estimate = builder.encoded_size();

        let encoded = builder.build().encode();

        assert_eq!(encoded.len(), estimate);
    }

    // ---------- 解码 ----------

    /// 一个合法的块编码后的字节：两条记录 "ab" → "1"、"ac" → ""。第二条与第一个 key 共有 "a"。
    fn valid_bytes() -> Vec<u8> {
        #[rustfmt::skip]
        let bytes: Vec<u8> = vec![
            // 记录 0，占 [0, 9)
            0, 0,   0, 2, b'a', b'b',   0, 1, b'1',
            // 记录 1，占 [9, 16)
            0, 1,   0, 1, b'c',         0, 0,
            // 偏移数组，占 [16, 20)
            0, 0, 0, 9,
            // 条数，占 [20, 22)
            0, 2,
        ];
        bytes
    }

    // 解码后数据区、偏移数组、条数都对。
    #[test]
    fn decode_reads_count_and_offsets_from_the_tail() {
        let encoded = valid_bytes();

        let block = Block::decode(&encoded).unwrap();

        assert_eq!(block.record_count(), 2);
        assert_eq!(block.offsets, vec![0, 9]);
        assert_eq!(block.data, encoded[..16].to_vec());
    }

    // 编码再解码，得到的块与原来的逐字段相同。
    #[test]
    fn decode_is_the_inverse_of_encode() {
        let block = block_of(&[("a", "1"), ("ab", ""), ("abc", "333"), ("dddd", "4")]);

        let decoded = Block::decode(&block.encode()).unwrap();

        assert_eq!(decoded.data, block.data);
        assert_eq!(decoded.offsets, block.offsets);
    }

    // 只有一条记录的块也能还原：偏移数组只有一项。
    #[test]
    fn decode_handles_a_single_record() {
        let block = block_of(&[("only", "one")]);

        let decoded = Block::decode(&block.encode()).unwrap();

        assert_eq!(decoded.record_count(), 1);
        assert_eq!(decoded.data, block.data);
        assert_eq!(decoded.offsets, vec![0]);
    }

    // 偏移大于 255 时高位字节不为 0，解码要把两个字节都用上。
    #[test]
    fn decode_reads_offsets_larger_than_one_byte() {
        let long_value = "v".repeat(300);
        let block = block_of(&[("a", long_value.as_str()), ("b", "2"), ("c", "3")]);
        // 第一条记录 2 + 2 + 1 + 2 + 300 = 307 字节，第二条 8 字节。
        assert_eq!(block.offsets, vec![0, 307, 315]);

        let decoded = Block::decode(&block.encode()).unwrap();

        assert_eq!(decoded.offsets, vec![0, 307, 315]);
        assert_eq!(decoded.data, block.data);
    }

    // 解码后的块不依赖传入的缓冲区：缓冲区之后被改动，块里的内容不变。
    #[test]
    fn decode_copies_the_input() {
        let block = block_of(&[("a", "1"), ("b", "2")]);
        let mut encoded = block.encode();

        let decoded = Block::decode(&encoded).unwrap();
        encoded.fill(0xff);

        assert_eq!(decoded.data, block.data);
        assert_eq!(decoded.offsets, block.offsets);
    }

    // ---------- 解码坏数据 ----------

    /// 解码应当失败，并且失败的原因是"内容损坏"。
    fn assert_corruption(encoded: &[u8]) {
        match Block::decode(encoded) {
            Err(Error::Corruption(_)) => {}
            Err(other) => panic!("应当是 Corruption，实际是 {other:?}"),
            Ok(_) => panic!("这段字节不应该解码成功：{encoded:?}"),
        }
    }

    // 样本本身是合法的，后面的测试都是在它上面改一处。第二条的 key 由共有前缀还原。
    #[test]
    fn decode_accepts_the_valid_sample() {
        let block = Block::decode(&valid_bytes()).unwrap();

        assert_eq!(block.record_count(), 2);
        assert_eq!(record(&block, 0), kv(b"ab", b"1"));
        assert_eq!(record(&block, 1), kv(b"ac", b""));
    }

    // 空的输入、只有 1 个字节的输入：连条数都读不出来。
    #[test]
    fn decode_rejects_input_too_short_for_the_count() {
        assert_corruption(&[]);
        assert_corruption(&[0]);
    }

    // 条数为 0：块里至少要有一条记录。
    #[test]
    fn decode_rejects_zero_count() {
        assert_corruption(&[0, 0]);
    }

    // 条数比实际的大：偏移数组会超出块的开头。
    #[test]
    fn decode_rejects_count_larger_than_the_block() {
        let mut bytes = valid_bytes();
        let len = bytes.len();
        bytes[len - 2] = 0xff;
        bytes[len - 1] = 0xff;

        assert_corruption(&bytes);
    }

    // 文件被截断：末尾少了几个字节，读到的"条数"其实是别的内容。
    #[test]
    fn decode_rejects_truncated_block() {
        let bytes = valid_bytes();

        for cut in 1..bytes.len() {
            let truncated = &bytes[..bytes.len() - cut];
            assert!(
                Block::decode(truncated).is_err(),
                "截掉末尾 {cut} 个字节之后不应该解码成功"
            );
        }
    }

    // 第一条记录的起点不是 0。
    #[test]
    fn decode_rejects_first_offset_not_zero() {
        let mut bytes = valid_bytes();
        bytes[17] = 1;

        assert_corruption(&bytes);
    }

    // 第二条记录的起点指到了数据区外面，或者没有比第一条大。
    #[test]
    fn decode_rejects_offset_out_of_order_or_out_of_range() {
        let mut beyond_data = valid_bytes();
        beyond_data[19] = 200;
        assert_corruption(&beyond_data);

        let mut not_increasing = valid_bytes();
        not_increasing[19] = 0;
        assert_corruption(&not_increasing);
    }

    // 记录里剩余部分的长度或 value 的长度被改大、改小，都与记录的范围对不上。
    #[test]
    fn decode_rejects_lengths_that_do_not_fill_the_record() {
        let mut rest_too_long = valid_bytes();
        rest_too_long[3] = 9;
        assert_corruption(&rest_too_long);

        let mut rest_too_short = valid_bytes();
        rest_too_short[3] = 1;
        assert_corruption(&rest_too_short);

        let mut value_too_long = valid_bytes();
        value_too_long[7] = 2;
        assert_corruption(&value_too_long);

        let mut value_too_short = valid_bytes();
        value_too_short[7] = 0;
        assert_corruption(&value_too_short);
    }

    // 第一条记录的共有前缀不是 0：它没有可以参照的 key。
    #[test]
    fn decode_rejects_first_record_with_overlap() {
        let mut bytes = valid_bytes();
        bytes[1] = 1;

        assert_corruption(&bytes);
    }

    // 共有前缀比第一个 key 还长：还原 key 时会越界，必须在解码时拒绝。
    #[test]
    fn decode_rejects_overlap_longer_than_first_key() {
        let mut bytes = valid_bytes();
        // 第一个 key 是 "ab"，长度 2；共有前缀 2 还可以，3 就越界了。
        bytes[10] = 2;
        assert!(Block::decode(&bytes).is_ok());
        bytes[10] = 3;

        assert_corruption(&bytes);
    }

    // 共有前缀和剩余部分都是 0：key 为空。
    #[test]
    fn decode_rejects_empty_key() {
        #[rustfmt::skip]
        let bytes: Vec<u8> = vec![
            0, 0,   0, 1, b'a',   0, 1, b'1',
            0, 0,   0, 0,         0, 1, b'2',
            0, 0, 0, 8,
            0, 2,
        ];

        assert_corruption(&bytes);
    }

    // 把合法的字节里任意一个字节改成任意值，解码要么成功、要么返回错误，不会 panic；
    // 解码成功的块，每条记录都能还原出 key、取出 value 而不越界。
    #[test]
    fn decode_never_panics_on_single_byte_changes() {
        let original = valid_bytes();

        for position in 0..original.len() {
            for new_value in [0u8, 1, 2, 3, 7, 0x7f, 0xff] {
                let mut bytes = original.clone();
                bytes[position] = new_value;

                if let Ok(block) = Block::decode(&bytes) {
                    for index in 0..block.record_count() {
                        let _ = record(&block, index);
                    }
                }
            }
        }
    }

    // ---------- 按下标取记录 ----------

    // 每个下标还原出的是对应那条记录的完整 key 和 value。
    #[test]
    fn records_are_restored_at_each_index() {
        let block = block_of(&[("a", "1"), ("bb", "22"), ("ccc", "333")]);

        assert_eq!(record(&block, 0), kv(b"a", b"1"));
        assert_eq!(record(&block, 1), kv(b"bb", b"22"));
        assert_eq!(record(&block, 2), kv(b"ccc", b"333"));
    }

    // 共有前缀长短不一、有的 key 是第一个 key 的延长：都能还原成原来的 key。
    #[test]
    fn keys_with_shared_prefixes_are_restored() {
        let keys = ["a/10/01", "a/10/02", "a/10/021", "a/11/07", "a/11/08", "b"];
        let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (*key, "v")).collect();
        let block = block_of(&pairs);

        for (index, key) in keys.iter().enumerate() {
            assert_eq!(record(&block, index), kv(key.as_bytes(), b"v"));
        }
    }

    // 墓碑（空 value）取出来是一段长度为 0 的字节，后面的记录不受影响。
    #[test]
    fn tombstone_value_is_empty() {
        let block = block_of(&[("a", "1"), ("b", ""), ("c", "3")]);

        assert_eq!(record(&block, 1), kv(b"b", b""));
        assert_eq!(record(&block, 2), kv(b"c", b"3"));
    }

    // key 和 value 的长度不同、且长度超过 255 时，各自的边界仍然算对。
    #[test]
    fn long_keys_and_values_are_restored() {
        let long_key = "k".repeat(300);
        let longer_key = "k".repeat(301);
        let long_value = "v".repeat(1000);
        let block = block_of(&[
            ("a", "1"),
            (long_key.as_str(), long_value.as_str()),
            (longer_key.as_str(), "2"),
        ]);

        assert_eq!(
            record(&block, 1),
            kv(long_key.as_bytes(), long_value.as_bytes())
        );
        assert_eq!(record(&block, 2), kv(longer_key.as_bytes(), b"2"));
    }

    // 记录里可以有任意字节（0x00、0xff），不会被当成分隔符。
    #[test]
    fn arbitrary_bytes_are_restored() {
        let mut builder = BlockBuilder::new(4096);
        assert!(builder.add(&[0x00, 0x01], &[0xff, 0x00, 0xff]));
        assert!(builder.add(&[0x00, 0xff], &[0x00]));
        let block = builder.build();

        assert_eq!(record(&block, 0), kv(&[0x00, 0x01], &[0xff, 0x00, 0xff]));
        assert_eq!(record(&block, 1), kv(&[0x00, 0xff], &[0x00]));
    }

    // 还原 key 时先清掉缓冲区里原来的内容，不会接在后面。
    #[test]
    fn key_at_replaces_buffer_contents() {
        let block = block_of(&[("abc", "1"), ("abd", "2")]);
        let mut buf = b"leftover".to_vec();

        block.key_at(1, &mut buf);

        assert_eq!(buf, b"abd");
    }

    // 内存里的大小是数据区的字节数，加上每条记录 2 字节的偏移。
    #[test]
    fn memory_size_counts_data_and_offsets() {
        let block = block_of(&[("a", "1"), ("bb", "22"), ("ccc", "333")]);

        assert_eq!(block.memory_size(), block.data.len() + 3 * 2);
    }

    // 解码出来的块与原来的块一样可以按下标还原记录。
    #[test]
    fn records_are_restored_from_a_decoded_block() {
        let block = block_of(&[("a", "1"), ("ab", ""), ("cc", "333")]);

        let decoded = Block::decode(&block.encode()).unwrap();

        assert_eq!(record(&decoded, 0), kv(b"a", b"1"));
        assert_eq!(record(&decoded, 1), kv(b"ab", b""));
        assert_eq!(record(&decoded, 2), kv(b"cc", b"333"));
    }
}
