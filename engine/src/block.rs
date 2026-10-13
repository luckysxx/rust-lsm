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
//! 一条记录：  key 长度 │ key │ value 长度 │ value
//! ```
//!
//! 偏移 i 是记录 i 在数据区里的起点。偏移数组和条数放在尾部，是因为构建时
//! 记录一条一条追加，封口之前不知道一共有几条；读的时候从最后 2 字节倒着找回来。
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

    /// 第 `index` 条记录的（key，value），从 0 数起。
    ///
    /// 返回的两段字节借自块的数据区，不拷贝。`index` 必须小于 `record_count()`。
    fn entry(&self, index: usize) -> (&[u8], &[u8]) {
        let record_start = self.offsets[index] as usize;

        // key 的长度在记录的最前面，key 紧跟其后。
        let key_len = read_u16(&self.data, record_start) as usize;
        let key_start = record_start + SIZEOF_U16;
        let key_end = key_start + key_len;

        // value 的长度紧跟在 key 后面，所以要先知道 key 在哪里结束。
        let value_len = read_u16(&self.data, key_end) as usize;
        let value_start = key_end + SIZEOF_U16;
        let value_end = value_start + value_len;

        let key = &self.data[key_start..key_end];
        let value = &self.data[value_start..value_end];
        (key, value)
    }
}

/// 检查一段字节是不是一个结构完整的块；不是的话说明哪里不对。
///
/// 检查的内容：读得出条数，条数不为 0；偏移数组没有超出块；每条记录的起点从 0
/// 开始、严格递增、落在数据区里；每条记录里的两个长度恰好把这条记录占满。
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

        // 记录里依次是：key 长度、key、value 长度、value，合起来要恰好等于记录的长度。
        let record_len = record_end - record_start;
        if record_len < SIZEOF_U16 * 2 {
            return Err(corrupt_block("记录太短，放不下两个长度字段"));
        }
        let key_len = read_u16(encoded, record_start) as usize;
        if key_len == 0 {
            return Err(corrupt_block("key 的长度为 0"));
        }
        let value_len_at = record_start + SIZEOF_U16 + key_len;
        if value_len_at + SIZEOF_U16 > record_end {
            return Err(corrupt_block("key 的长度超出了记录"));
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

    // ---------- 编码（encode 已经写好；这两项要靠 add 做出块，第 1 步完成后通过）----------

    // 编码的结果逐字节确定：数据区、每条记录的起点、条数，整数是大端。
    #[test]
    fn encode_lays_out_data_then_offsets_then_count() {
        let block = block_of(&[("cat", "meow"), ("dog", "woof")]);

        let encoded = block.encode();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // 记录 0：key 长度 3，"cat"，value 长度 4，"meow"
            0, 3, b'c', b'a', b't', 0, 4, b'm', b'e', b'o', b'w',
            // 记录 1：key 长度 3，"dog"，value 长度 4，"woof"
            0, 3, b'd', b'o', b'g', 0, 4, b'w', b'o', b'o', b'f',
            // 偏移数组：记录 0 从 0 开始，记录 1 从 11 开始
            0, 0, 0, 11,
            // 条数
            0, 2,
        ];
        assert_eq!(encoded, expected);
    }

    // 构建器估算的大小与真正编码出来的长度一致。
    #[test]
    fn encoded_length_matches_builder_estimate() {
        let mut builder = BlockBuilder::new(4096);
        assert!(builder.add(b"a", b"1"));
        assert!(builder.add(b"bb", b""));
        assert!(builder.add(b"ccc", b"333"));
        let estimate = builder.encoded_size();

        let encoded = builder.build().encode();

        assert_eq!(encoded.len(), estimate);
    }

    // ---------- 解码 ----------

    // 手写一段字节，解码后数据区、偏移数组、条数都对。
    #[test]
    fn decode_reads_count_and_offsets_from_the_tail() {
        #[rustfmt::skip]
        let encoded: Vec<u8> = vec![
            0, 1, b'a', 0, 1, b'1',
            0, 2, b'b', b'b', 0, 0,
            0, 0, 0, 6,
            0, 2,
        ];

        let block = Block::decode(&encoded).unwrap();

        assert_eq!(block.record_count(), 2);
        assert_eq!(block.offsets, vec![0, 6]);
        assert_eq!(block.data, encoded[..12].to_vec());
    }

    // 编码再解码，得到的块与原来的逐字段相同。
    #[test]
    fn decode_is_the_inverse_of_encode() {
        let block = block_of(&[("a", "1"), ("b", ""), ("cc", "333"), ("dddd", "4")]);

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
        assert_eq!(block.offsets, vec![0, 305, 311]);

        let decoded = Block::decode(&block.encode()).unwrap();

        assert_eq!(decoded.offsets, vec![0, 305, 311]);
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

    /// 一个合法的块编码后的字节：两条记录 "a" → "1"、"bb" → ""。
    fn valid_bytes() -> Vec<u8> {
        #[rustfmt::skip]
        let bytes: Vec<u8> = vec![
            0, 1, b'a', 0, 1, b'1',
            0, 2, b'b', b'b', 0, 0,
            0, 0, 0, 6,
            0, 2,
        ];
        bytes
    }

    /// 解码应当失败，并且失败的原因是"内容损坏"。
    fn assert_corruption(encoded: &[u8]) {
        match Block::decode(encoded) {
            Err(Error::Corruption(_)) => {}
            Err(other) => panic!("应当是 Corruption，实际是 {other:?}"),
            Ok(_) => panic!("这段字节不应该解码成功：{encoded:?}"),
        }
    }

    // 上面那段字节本身是合法的，后面的测试都是在它上面改一处。
    #[test]
    fn decode_accepts_the_valid_sample() {
        let block = Block::decode(&valid_bytes()).unwrap();

        assert_eq!(block.record_count(), 2);
        assert_eq!(block.entry(1), (&b"bb"[..], &b""[..]));
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
        bytes[13] = 1;

        assert_corruption(&bytes);
    }

    // 第二条记录的起点指到了数据区外面，或者没有比第一条大。
    #[test]
    fn decode_rejects_offset_out_of_order_or_out_of_range() {
        let mut beyond_data = valid_bytes();
        beyond_data[15] = 200;
        assert_corruption(&beyond_data);

        let mut not_increasing = valid_bytes();
        not_increasing[15] = 0;
        assert_corruption(&not_increasing);
    }

    // 记录里的 key 长度或 value 长度被改大、改小，都与记录的范围对不上。
    #[test]
    fn decode_rejects_lengths_that_do_not_fill_the_record() {
        let mut key_too_long = valid_bytes();
        key_too_long[1] = 9;
        assert_corruption(&key_too_long);

        let mut key_empty = valid_bytes();
        key_empty[1] = 0;
        assert_corruption(&key_empty);

        let mut value_too_long = valid_bytes();
        value_too_long[4] = 2;
        assert_corruption(&value_too_long);

        let mut value_too_short = valid_bytes();
        value_too_short[4] = 0;
        assert_corruption(&value_too_short);
    }

    // 把合法的字节里任意一个字节改成任意值，解码要么成功、要么返回错误，不会 panic；
    // 解码成功的块，每条记录都能取出来而不越界。
    #[test]
    fn decode_never_panics_on_single_byte_changes() {
        let original = valid_bytes();

        for position in 0..original.len() {
            for new_value in [0u8, 1, 2, 7, 0x7f, 0xff] {
                let mut bytes = original.clone();
                bytes[position] = new_value;

                if let Ok(block) = Block::decode(&bytes) {
                    for index in 0..block.record_count() {
                        let (key, value) = block.entry(index);
                        black_box_len(key, value);
                    }
                }
            }
        }
    }

    /// 用一下取出来的两段字节，确认它们确实可以访问。
    fn black_box_len(key: &[u8], value: &[u8]) -> usize {
        key.len() + value.len()
    }

    // ---------- 按下标取记录 ----------

    // 每个下标取到的是对应那条记录的 key 和 value。
    #[test]
    fn entry_returns_key_and_value_at_index() {
        let block = block_of(&[("a", "1"), ("bb", "22"), ("ccc", "333")]);

        assert_eq!(block.entry(0), (&b"a"[..], &b"1"[..]));
        assert_eq!(block.entry(1), (&b"bb"[..], &b"22"[..]));
        assert_eq!(block.entry(2), (&b"ccc"[..], &b"333"[..]));
    }

    // 墓碑（空 value）取出来是一段长度为 0 的字节，后面的记录不受影响。
    #[test]
    fn entry_returns_empty_value_for_tombstone() {
        let block = block_of(&[("a", "1"), ("b", ""), ("c", "3")]);

        assert_eq!(block.entry(1), (&b"b"[..], &b""[..]));
        assert_eq!(block.entry(2), (&b"c"[..], &b"3"[..]));
    }

    // key 和 value 的长度不同、且长度超过 255 时，各自的边界仍然算对。
    #[test]
    fn entry_handles_long_keys_and_values() {
        let long_key = "k".repeat(300);
        let long_value = "v".repeat(1000);
        let block = block_of(&[("a", "1"), (long_key.as_str(), long_value.as_str())]);

        let (key, value) = block.entry(1);

        assert_eq!(key, long_key.as_bytes());
        assert_eq!(value, long_value.as_bytes());
    }

    // 记录里可以有任意字节（0x00、0xff），不会被当成分隔符。
    #[test]
    fn entry_handles_arbitrary_bytes() {
        let mut builder = BlockBuilder::new(4096);
        assert!(builder.add(&[0x00, 0x01], &[0xff, 0x00, 0xff]));
        assert!(builder.add(&[0xff], &[0x00]));
        let block = builder.build();

        assert_eq!(block.entry(0), (&[0x00, 0x01][..], &[0xff, 0x00, 0xff][..]));
        assert_eq!(block.entry(1), (&[0xff][..], &[0x00][..]));
    }

    // 解码出来的块与原来的块一样可以按下标取记录。
    #[test]
    fn entry_works_on_a_decoded_block() {
        let block = block_of(&[("a", "1"), ("b", ""), ("cc", "333")]);

        let decoded = Block::decode(&block.encode()).unwrap();

        assert_eq!(decoded.entry(0), (&b"a"[..], &b"1"[..]));
        assert_eq!(decoded.entry(1), (&b"b"[..], &b""[..]));
        assert_eq!(decoded.entry(2), (&b"cc"[..], &b"333"[..]));
    }
}
