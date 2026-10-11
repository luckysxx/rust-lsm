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
    /// 这里信任输入：传入的字节不是 `encode` 产生的（被截断、被改动）时，
    /// 可能 panic，也可能得到内容不对的块。对坏数据的检查等引擎有了错误类型
    /// 和校验和之后再加。
    pub fn decode(encoded: &[u8]) -> Self {
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
        Block { data, offsets }
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

        let block = Block::decode(&encoded);

        assert_eq!(block.record_count(), 2);
        assert_eq!(block.offsets, vec![0, 6]);
        assert_eq!(block.data, encoded[..12].to_vec());
    }

    // 编码再解码，得到的块与原来的逐字段相同。
    #[test]
    fn decode_is_the_inverse_of_encode() {
        let block = block_of(&[("a", "1"), ("b", ""), ("cc", "333"), ("dddd", "4")]);

        let decoded = Block::decode(&block.encode());

        assert_eq!(decoded.data, block.data);
        assert_eq!(decoded.offsets, block.offsets);
    }

    // 只有一条记录的块也能还原：偏移数组只有一项。
    #[test]
    fn decode_handles_a_single_record() {
        let block = block_of(&[("only", "one")]);

        let decoded = Block::decode(&block.encode());

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

        let decoded = Block::decode(&block.encode());

        assert_eq!(decoded.offsets, vec![0, 305, 311]);
        assert_eq!(decoded.data, block.data);
    }

    // 解码后的块不依赖传入的缓冲区：缓冲区之后被改动，块里的内容不变。
    #[test]
    fn decode_copies_the_input() {
        let block = block_of(&[("a", "1"), ("b", "2")]);
        let mut encoded = block.encode();

        let decoded = Block::decode(&encoded);
        encoded.fill(0xff);

        assert_eq!(decoded.data, block.data);
        assert_eq!(decoded.offsets, block.offsets);
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

        let decoded = Block::decode(&block.encode());

        assert_eq!(decoded.entry(0), (&b"a"[..], &b"1"[..]));
        assert_eq!(decoded.entry(1), (&b"b"[..], &b""[..]));
        assert_eq!(decoded.entry(2), (&b"cc"[..], &b"333"[..]));
    }
}
