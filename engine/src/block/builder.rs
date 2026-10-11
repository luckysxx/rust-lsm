//! 块的构建器：记录按 key 从小到大一条一条加进来，放不下时告诉调用方。

use super::{Block, MAX_KEY_LEN, MAX_VALUE_LEN, SIZEOF_U16};
use bytes::BufMut;

/// 构建一个块。
///
/// 构建器只认识自己这一个块：放不下时 `add` 返回 `false`，由调用方把当前的块
/// 封口、另开一个构建器再加。调用方保证记录按 key 从小到大加入，这里不检查。
pub struct BlockBuilder {
    /// 数据区：已经加入的记录，首尾相接。
    data: Vec<u8>,
    /// 每条记录在数据区里的起点。
    offsets: Vec<u16>,
    /// 目标大小：编码后的块不超过这么多字节，除非块里只有一条记录。
    block_size: usize,
}

impl BlockBuilder {
    /// 创建一个空的构建器。
    ///
    /// 目标大小不能超过 65535：偏移只有 2 字节，块再大，靠后的记录的起点就记不下了。
    pub fn new(block_size: usize) -> Self {
        assert!(
            block_size <= u16::MAX as usize,
            "块的目标大小不能超过 65535 字节"
        );
        Self {
            data: Vec::new(),
            offsets: Vec::new(),
            block_size,
        }
    }

    /// 还没有加入任何记录。
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// 如果现在封口，编码出来有多少字节：数据区、偏移数组、条数。
    pub fn encoded_size(&self) -> usize {
        let data_size = self.data.len();
        let offsets_size = self.offsets.len() * SIZEOF_U16;
        let count_size = SIZEOF_U16;
        data_size + offsets_size + count_size
    }

    /// 加入一条记录。value 为空表示墓碑，照常加入。
    ///
    /// 返回 `true` 表示已经加入。返回 `false` 表示加入之后编码的大小会超过目标
    /// 大小，这时构建器保持原样，这条记录没有加入。
    ///
    /// 块是空的时候总是加入，不管这条记录有多大：它独占一个块。
    #[must_use]
    pub fn add(&mut self, key: &[u8], value: &[u8]) -> bool {
        assert!(!key.is_empty(), "key 不能为空");
        assert!(key.len() <= MAX_KEY_LEN, "key 超过了 65535 字节");
        assert!(value.len() <= MAX_VALUE_LEN, "value 超过了 65535 字节");

        // 这条记录要多占的字节：记录本身（两个长度字段、key、value），
        // 加上偏移数组里新增的一项。末尾的条数每个块只有一份，已经算在
        // `encoded_size` 里，不随记录增加。
        let record_size = SIZEOF_U16 + key.len() + SIZEOF_U16 + value.len();
        let size_after_add = self.encoded_size() + record_size + SIZEOF_U16;
        if size_after_add > self.block_size && !self.is_empty() {
            return false;
        }

        // 先记下起点，再追加记录：起点就是追加之前数据区的长度。
        self.offsets.push(self.data.len() as u16);
        self.data.put_u16(key.len() as u16);
        self.data.put_slice(key);
        self.data.put_u16(value.len() as u16);
        self.data.put_slice(value);
        true
    }

    /// 封口，得到做好的块。空的构建器不能封口。
    pub fn build(self) -> Block {
        assert!(!self.is_empty(), "块里至少要有一条记录");
        Block {
            data: self.data,
            offsets: self.offsets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 新建的构建器是空的，加入一条之后不再是空的。
    #[test]
    fn builder_is_empty_until_first_add() {
        let mut builder = BlockBuilder::new(4096);
        assert!(builder.is_empty());

        assert!(builder.add(b"a", b"1"));

        assert!(!builder.is_empty());
    }

    // 一条记录在数据区里是：key 长度、key、value 长度、value，长度是 2 字节大端。
    #[test]
    fn add_encodes_one_record() {
        let mut builder = BlockBuilder::new(4096);

        assert!(builder.add(b"cat", b"meow"));
        let block = builder.build();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0, 3, b'c', b'a', b't',
            0, 4, b'm', b'e', b'o', b'w',
        ];
        assert_eq!(block.data, expected);
        assert_eq!(block.offsets, vec![0]);
    }

    // 记录首尾相接；每条记录的起点记在偏移数组里。
    #[test]
    fn add_appends_records_and_records_their_starts() {
        let mut builder = BlockBuilder::new(4096);

        assert!(builder.add(b"cat", b"meow"));
        assert!(builder.add(b"dog", b"woof"));
        assert!(builder.add(b"eel", b"z"));
        let block = builder.build();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0, 3, b'c', b'a', b't', 0, 4, b'm', b'e', b'o', b'w',
            0, 3, b'd', b'o', b'g', 0, 4, b'w', b'o', b'o', b'f',
            0, 3, b'e', b'e', b'l', 0, 1, b'z',
        ];
        assert_eq!(block.data, expected);
        assert_eq!(block.offsets, vec![0, 11, 22]);
    }

    // 墓碑（空 value）照常加入：value 长度为 0，后面没有 value 的字节。
    #[test]
    fn add_keeps_tombstone_as_zero_length_value() {
        let mut builder = BlockBuilder::new(4096);

        assert!(builder.add(b"a", b""));
        assert!(builder.add(b"b", b"2"));
        let block = builder.build();

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0, 1, b'a', 0, 0,
            0, 1, b'b', 0, 1, b'2',
        ];
        assert_eq!(block.data, expected);
        assert_eq!(block.offsets, vec![0, 5]);
    }

    // 长度超过 255 时高位字节不为 0：300 = 0x012c。
    #[test]
    fn add_writes_lengths_in_big_endian() {
        let mut builder = BlockBuilder::new(4096);
        let value = vec![b'v'; 300];

        assert!(builder.add(b"k", &value));
        let block = builder.build();

        assert_eq!(&block.data[..5], &[0, 1, b'k', 0x01, 0x2c]);
        assert_eq!(block.data.len(), 2 + 1 + 2 + 300);
    }

    // 每加入一条，估算的大小增加：记录本身，加上偏移数组里的一项（2 字节）。
    #[test]
    fn encoded_size_grows_by_record_and_offset() {
        let mut builder = BlockBuilder::new(4096);
        // 空的构建器只有末尾的条数。
        assert_eq!(builder.encoded_size(), 2);

        assert!(builder.add(b"cat", b"meow"));
        // 记录 2 + 3 + 2 + 4 = 11，偏移 2。
        assert_eq!(builder.encoded_size(), 2 + 11 + 2);

        assert!(builder.add(b"dog", b""));
        // 记录 2 + 3 + 2 + 0 = 7，偏移 2。
        assert_eq!(builder.encoded_size(), 2 + 11 + 2 + 7 + 2);
    }

    // 加入之后会超过目标大小时返回 false。
    #[test]
    fn add_returns_false_when_block_would_exceed_target() {
        // 每条记录占 2 + 1 + 2 + 1 = 6 字节，加偏移 2 字节，共 8 字节。
        // 两条之后是 2 + 8 + 8 = 18，第三条会到 26。
        let mut builder = BlockBuilder::new(20);

        assert!(builder.add(b"a", b"1"));
        assert!(builder.add(b"b", b"2"));
        assert!(!builder.add(b"c", b"3"));
    }

    // 加入之后恰好等于目标大小，算放得下。
    #[test]
    fn add_accepts_record_that_exactly_fits() {
        let mut builder = BlockBuilder::new(26);

        assert!(builder.add(b"a", b"1"));
        assert!(builder.add(b"b", b"2"));
        assert!(builder.add(b"c", b"3"));

        assert_eq!(builder.encoded_size(), 26);
    }

    // 判断时要把偏移数组里新增的那一项算进去，不能只看记录本身。
    #[test]
    fn add_counts_the_new_offset_entry() {
        // 两条之后是 18 字节；第三条的记录本身 6 字节，到 24，
        // 再加它的偏移 2 字节是 26，超过 25。
        let mut builder = BlockBuilder::new(25);
        assert!(builder.add(b"a", b"1"));
        assert!(builder.add(b"b", b"2"));

        let added = builder.add(b"c", b"3");

        assert!(!added);
    }

    // 返回 false 时构建器保持原样：被拒绝的记录没有留下任何痕迹。
    #[test]
    fn rejected_add_leaves_builder_unchanged() {
        let mut builder = BlockBuilder::new(25);
        assert!(builder.add(b"a", b"1"));
        assert!(builder.add(b"b", b"2"));
        let size_before = builder.encoded_size();

        assert!(!builder.add(b"c", b"3"));

        assert_eq!(builder.encoded_size(), size_before);
        let block = builder.build();
        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0, 1, b'a', 0, 1, b'1',
            0, 1, b'b', 0, 1, b'2',
        ];
        assert_eq!(block.data, expected);
        assert_eq!(block.offsets, vec![0, 6]);
    }

    // 被拒绝之后，更小的、放得下的记录仍然可以加入。
    #[test]
    fn smaller_record_can_be_added_after_a_rejection() {
        let mut builder = BlockBuilder::new(30);
        assert!(builder.add(b"a", b"1"));
        assert!(builder.add(b"b", b"2"));
        // 现在 18 字节。"c" 加一个 10 字节的 value 要 2 + 1 + 2 + 10 + 2 = 17，放不下。
        assert!(!builder.add(b"c", b"0123456789"));

        // "c" 加 1 字节的 value 要 8 字节，到 26，放得下。
        assert!(builder.add(b"c", b"3"));

        assert_eq!(builder.build().offsets, vec![0, 6, 12]);
    }

    // 空的块无条件收下第一条记录，哪怕它比目标大小还大。
    #[test]
    fn empty_block_accepts_oversized_record() {
        let mut builder = BlockBuilder::new(16);
        let value = vec![b'v'; 1000];

        assert!(builder.add(b"big", &value));

        assert!(builder.encoded_size() > 16);
        let block = builder.build();
        assert_eq!(block.offsets, vec![0]);
        assert_eq!(block.data.len(), 2 + 3 + 2 + 1000);
    }

    // 过大的记录独占一个块：它之后的记录不再加入这个块。
    #[test]
    fn oversized_record_stays_alone_in_its_block() {
        let mut builder = BlockBuilder::new(16);
        let value = vec![b'v'; 1000];
        assert!(builder.add(b"big", &value));

        assert!(!builder.add(b"next", b"1"));
    }

    // 目标大小小到连一条最短的记录都放不下时，每个块仍然能放一条。
    #[test]
    fn tiny_target_still_takes_one_record_per_block() {
        let mut builder = BlockBuilder::new(1);

        assert!(builder.add(b"a", b"1"));
        assert!(!builder.add(b"b", b"2"));
    }

    // 一直加到放不下为止：编码后的长度不超过目标大小，而且确实装了不止一条。
    #[test]
    fn filled_block_stays_within_target() {
        let block_size = 4096;
        let mut builder = BlockBuilder::new(block_size);
        let mut added = 0;
        loop {
            assert!(
                added < 10_000,
                "加了一万条还没有满，add 很可能从不返回 false"
            );
            let key = format!("key-{added:06}");
            let value = format!("value-{added:06}");
            if !builder.add(key.as_bytes(), value.as_bytes()) {
                break;
            }
            added += 1;
        }

        let encoded = builder.build().encode();

        // 每条记录 2 + 10 + 2 + 12 + 2 = 28 字节，(4096 - 2) / 28 = 146。
        assert_eq!(added, 146);
        assert!(encoded.len() <= block_size);
    }

    // 空的构建器不能封口。
    #[test]
    #[should_panic(expected = "至少要有一条记录")]
    fn build_panics_on_empty_builder() {
        let builder = BlockBuilder::new(4096);

        let _block = builder.build();
    }
}
