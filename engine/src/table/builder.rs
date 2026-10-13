//! SST 的构建器：记录按 key 从小到大一条一条加进来，最后写成一个文件。

use std::path::Path;

use bytes::{BufMut, Bytes};

use super::{BlockMeta, FileObject, SST_MAGIC, SsTable, encode_block_meta};
use crate::block::BlockBuilder;
use crate::error::Result;

/// 构建一个 SST。
///
/// 手里有一个正在装的块；它装满了就封口、编码，接到已经攒下的字节后面，
/// 在索引里记一项，再换一个新的空块。全部内容攒在内存里，`build` 时一次写出。
/// 调用方保证记录按 key 从小到大加入，同一个 key 不出现两次。
pub struct SsTableBuilder {
    /// 正在装的块。
    builder: BlockBuilder,
    /// 正在装的块里的第一个 key；块还是空的时候为空。
    first_key: Vec<u8>,
    /// 正在装的块里最后加入的 key；块还是空的时候为空。
    last_key: Vec<u8>,
    /// 已经封口的块，编码之后首尾相接。
    data: Vec<u8>,
    /// 已经封口的块的索引，每个块一项。
    block_meta: Vec<BlockMeta>,
    /// 每个块的目标大小。
    block_size: usize,
}

impl SsTableBuilder {
    /// 创建一个空的构建器。`block_size` 是每个数据块的目标大小。
    pub fn new(block_size: usize) -> Self {
        Self {
            builder: BlockBuilder::new(block_size),
            first_key: Vec::new(),
            last_key: Vec::new(),
            data: Vec::new(),
            block_meta: Vec::new(),
            block_size,
        }
    }

    /// 加入一条记录。value 为空表示墓碑，照常加入。
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        // 调用方保证 key 严格递增。调试编译下检查一下：喂数据的循环忘了让游标前进时，
        // 这里会立刻报错，而不是把同一条记录无休止地加进来。
        debug_assert!(
            self.is_after_last_key(key),
            "加入 SST 的 key 没有比上一条大：记录没有按顺序加入，或者同一条加了两次"
        );

        // 先让这条记录进到某个块里：当前的块放不下，就封口，换一个新的块再加。
        if !self.builder.add(key, value) {
            self.finish_block();
            let added = self.builder.add(key, value);
            assert!(added, "空的块应当收下任何一条记录");
        }

        // 再更新这条记录所在的块的首末 key。
        // 首 key 为空说明它是块里的第一条，不管这个块是刚换上的还是一开始就有的。
        if self.first_key.is_empty() {
            self.first_key.extend_from_slice(key);
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
    }

    /// `key` 是否比已经加入的最后一条记录的 key 大。还没有加入过记录时为真。
    fn is_after_last_key(&self, key: &[u8]) -> bool {
        // 正在装的块里有记录：和它的末 key 比。
        if !self.last_key.is_empty() {
            return key > &self.last_key[..];
        }
        // 正在装的块是空的：和上一个封口的块的末 key 比。
        match self.block_meta.last() {
            Some(meta) => key > &meta.last_key[..],
            None => true,
        }
    }

    /// 把正在装的块封口：编码后接到 `data` 后面，在索引里记一项，换上一个新的空块。
    ///
    /// 调用时正在装的块里至少要有一条记录。
    fn finish_block(&mut self) {
        // 把正在装的构建器拿出来，原地换上一个新的。`build` 要拿走整个构建器，
        // 而它是 `self` 的一个字段，不能直接拿走不还，所以用"换一个进去"的办法。
        let new_builder = BlockBuilder::new(self.block_size);
        let full_builder = std::mem::replace(&mut self.builder, new_builder);
        let encoded_block = full_builder.build().encode();

        // 先记索引，再接数据：块的起点就是接上去之前 `data` 的长度。
        let new_block_meta = BlockMeta {
            offset: self.data.len(),
            first_key: Bytes::copy_from_slice(&self.first_key),
            last_key: Bytes::copy_from_slice(&self.last_key),
        };
        self.block_meta.push(new_block_meta);
        self.data.put_slice(&encoded_block);

        // 新的块还是空的。
        self.first_key.clear();
        self.last_key.clear();
    }

    /// 封口并写成文件，返回这个 SST。`id` 是文件的编号，`path` 是文件的路径。
    ///
    /// 至少要加入过一条记录。写文件失败时返回错误。
    pub fn build(mut self, id: usize, path: &Path) -> Result<SsTable> {
        assert!(
            !self.builder.is_empty() || !self.block_meta.is_empty(),
            "SST 里至少要有一条记录"
        );

        // 最后一个块还没有封口。
        self.finish_block();

        // 数据块之后是索引，然后是索引的起点，最后是魔数。
        // 索引的起点要在接上索引之前取。
        let block_meta_start = self.data.len();
        encode_block_meta(&self.block_meta, &mut self.data);
        self.data.put_u32(block_meta_start as u32);
        self.data.put_u32(SST_MAGIC);

        let file = FileObject::create(path, &self.data)?;

        let first_key = self.block_meta[0].first_key.clone();
        let last_key = self.block_meta[self.block_meta.len() - 1].last_key.clone();
        Ok(SsTable {
            id,
            file,
            block_meta: self.block_meta,
            block_meta_start,
            first_key,
            last_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, BlockIterator};
    use crate::error::Error;
    use crate::iterators::vec_iterator::collect_all;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// 用字符串写出记录，做成一个 SST 文件，返回这个 SST 和文件的全部字节。
    fn build_sst(dir: &TempDir, block_size: usize, pairs: &[(&str, &str)]) -> (SsTable, Vec<u8>) {
        let path = dir.path().join("1.sst");
        let mut builder = SsTableBuilder::new(block_size);
        for (key, value) in pairs {
            builder.add(key.as_bytes(), value.as_bytes());
        }
        let sst = builder.build(1, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        (sst, bytes)
    }

    /// 按索引把文件里的每个块切出来解码，收集全部记录。
    fn records_in_file(sst: &SsTable, bytes: &[u8]) -> Vec<(String, String)> {
        let mut records = Vec::new();
        let block_meta = sst.block_meta();
        for (i, meta) in block_meta.iter().enumerate() {
            let block_end = match block_meta.get(i + 1) {
                Some(next) => next.offset,
                None => sst.block_meta_start(),
            };
            let block = Block::decode(&bytes[meta.offset..block_end]).unwrap();
            let mut iter = BlockIterator::new(Arc::new(block));
            records.extend(collect_all(&mut iter));
        }
        records
    }

    /// 生成 `count` 条形如 ("key-0000", "value-0000") 的记录，已经按 key 排好。
    fn numbered_pairs(count: usize) -> Vec<(String, String)> {
        (0..count)
            .map(|i| (format!("key-{i:04}"), format!("value-{i:04}")))
            .collect()
    }

    // ---------- 第 2 步之一：add 与 finish_block ----------

    // 文件的内容逐字节确定：两个数据块、索引、索引的起点、魔数。
    #[test]
    fn file_layout_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        // 一条记录编码成块是 10 字节；目标大小 16 放不下第二条，所以每块一条。
        let (_sst, bytes) = build_sst(&dir, 16, &[("a", "1"), ("b", "2")]);

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // 块 0，占 [0, 10)：记录 "a" → "1"，偏移数组 [0]，条数 1
            0, 1, b'a', 0, 1, b'1',   0, 0,   0, 1,
            // 块 1，占 [10, 20)：记录 "b" → "2"，偏移数组 [0]，条数 1
            0, 1, b'b', 0, 1, b'2',   0, 0,   0, 1,
            // 索引从 20 开始：块数 2
            0, 0, 0, 2,
            // 第 0 项：起点 0，首 key "a"，末 key "a"
            0, 0, 0, 0,   0, 1, b'a',   0, 1, b'a',
            // 第 1 项：起点 10，首 key "b"，末 key "b"
            0, 0, 0, 10,   0, 1, b'b',   0, 1, b'b',
            // 索引的起点 20
            0, 0, 0, 20,
            // 魔数 "LSM1"
            b'L', b'S', b'M', b'1',
        ];
        assert_eq!(bytes, expected);
    }

    // 记录放得下时都进同一个块：索引只有一项，首末 key 是第一条和最后一条。
    #[test]
    fn small_input_makes_one_block() {
        let dir = tempfile::tempdir().unwrap();
        let (sst, _bytes) = build_sst(&dir, 4096, &[("a", "1"), ("b", "2"), ("c", "3")]);

        assert_eq!(sst.block_count(), 1);
        let meta = &sst.block_meta()[0];
        assert_eq!(meta.offset, 0);
        assert_eq!(&meta.first_key[..], b"a");
        assert_eq!(&meta.last_key[..], b"c");
    }

    // 块装满后换新块：每一项的首末 key 正是那个块里的第一条和最后一条。
    #[test]
    fn block_meta_records_first_and_last_key_of_each_block() {
        let dir = tempfile::tempdir().unwrap();
        // 每条记录在块里占 8 字节（含偏移），块本身 2 字节；目标 26 恰好放三条。
        let (sst, _bytes) = build_sst(
            &dir,
            26,
            &[
                ("a", "1"),
                ("b", "2"),
                ("c", "3"),
                ("d", "4"),
                ("e", "5"),
                ("f", "6"),
                ("g", "7"),
            ],
        );

        let ranges: Vec<(&[u8], &[u8])> = sst
            .block_meta()
            .iter()
            .map(|meta| (&meta.first_key[..], &meta.last_key[..]))
            .collect();
        assert_eq!(
            ranges,
            vec![
                (&b"a"[..], &b"c"[..]),
                (&b"d"[..], &b"f"[..]),
                (&b"g"[..], &b"g"[..]),
            ]
        );
    }

    // 每个块的起点是它前面所有块的长度之和：第一个是 0，之后严格递增。
    #[test]
    fn block_offsets_follow_encoded_block_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let (sst, _bytes) = build_sst(
            &dir,
            26,
            &[
                ("a", "1"),
                ("b", "2"),
                ("c", "3"),
                ("d", "4"),
                ("e", "5"),
                ("f", "6"),
                ("g", "7"),
            ],
        );

        // 三条记录的块是 26 字节，一条记录的块是 10 字节。
        let offsets: Vec<usize> = sst.block_meta().iter().map(|meta| meta.offset).collect();
        assert_eq!(offsets, vec![0, 26, 52]);
        assert_eq!(sst.block_meta_start(), 62);
    }

    // 放不下而换块的那条记录没有丢：它是新块的第一条。
    #[test]
    fn record_that_does_not_fit_starts_the_next_block() {
        let dir = tempfile::tempdir().unwrap();
        let (sst, bytes) = build_sst(&dir, 16, &[("a", "1"), ("b", "2"), ("c", "3")]);

        assert_eq!(sst.block_count(), 3);
        let records = records_in_file(&sst, &bytes);
        let keys: Vec<&str> = records.iter().map(|(key, _value)| key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    // 很多条记录：按索引把每个块切出来解码，得到的记录与加入的完全相同、顺序不变。
    #[test]
    fn all_records_can_be_read_back_block_by_block() {
        let dir = tempfile::tempdir().unwrap();
        let pairs = numbered_pairs(1000);
        let borrowed: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();

        let (sst, bytes) = build_sst(&dir, 256, &borrowed);

        assert!(
            sst.block_count() > 50,
            "一千条记录、256 字节的块，应当有很多块"
        );
        assert_eq!(records_in_file(&sst, &bytes), pairs);
    }

    // 墓碑（空 value）照常写进文件。
    #[test]
    fn tombstones_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let (sst, bytes) = build_sst(&dir, 4096, &[("a", "1"), ("b", ""), ("c", "3")]);

        let records = records_in_file(&sst, &bytes);

        assert_eq!(records[1], ("b".to_string(), String::new()));
    }

    // 比目标大小还大的记录独占一个块，前后的记录各在别的块里。
    #[test]
    fn oversized_record_gets_its_own_block() {
        let dir = tempfile::tempdir().unwrap();
        let big_value = "v".repeat(1000);
        let (sst, bytes) = build_sst(
            &dir,
            64,
            &[("a", "1"), ("b", big_value.as_str()), ("c", "3")],
        );

        assert_eq!(sst.block_count(), 3);
        assert_eq!(&sst.block_meta()[1].first_key[..], b"b");
        assert_eq!(&sst.block_meta()[1].last_key[..], b"b");
        assert_eq!(records_in_file(&sst, &bytes).len(), 3);
    }

    // ---------- 第 2 步之二：build ----------

    // 文件的最后 4 字节是魔数，再往前 4 字节是索引的起点。
    #[test]
    fn file_ends_with_meta_offset_and_magic() {
        let dir = tempfile::tempdir().unwrap();
        let pairs = numbered_pairs(100);
        let borrowed: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let (sst, bytes) = build_sst(&dir, 256, &borrowed);

        let len = bytes.len();
        assert_eq!(&bytes[len - 4..], b"LSM1");
        let meta_offset = u32::from_be_bytes(bytes[len - 8..len - 4].try_into().unwrap());
        assert_eq!(meta_offset as usize, sst.block_meta_start());
        // 索引紧跟在数据块后面，开头是块数。
        let block_count = u32::from_be_bytes(
            bytes[sst.block_meta_start()..sst.block_meta_start() + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(block_count as usize, sst.block_count());
    }

    // 最后一个没装满的块也要封口写出去。
    #[test]
    fn last_partial_block_is_written() {
        let dir = tempfile::tempdir().unwrap();
        // 目标 26 放三条；四条记录，最后一条在第二个块里。
        let (sst, bytes) = build_sst(&dir, 26, &[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")]);

        assert_eq!(sst.block_count(), 2);
        assert_eq!(records_in_file(&sst, &bytes).len(), 4);
    }

    // 返回的 SST 记着整个文件的首末 key、文件的编号和长度。
    #[test]
    fn built_sst_knows_its_key_range_id_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("7.sst");
        let mut builder = SsTableBuilder::new(26);
        for key in ["a", "b", "c", "d", "e", "f", "g"] {
            builder.add(key.as_bytes(), b"1");
        }

        let sst = builder.build(7, &path).unwrap();

        assert_eq!(sst.id(), 7);
        assert_eq!(sst.first_key(), b"a");
        assert_eq!(sst.last_key(), b"g");
        assert_eq!(sst.file_size(), std::fs::metadata(&path).unwrap().len());
    }

    // 只有一条记录也能写成一个文件。
    #[test]
    fn single_record_sst() {
        let dir = tempfile::tempdir().unwrap();
        let (sst, bytes) = build_sst(&dir, 4096, &[("only", "one")]);

        assert_eq!(sst.block_count(), 1);
        assert_eq!(sst.first_key(), b"only");
        assert_eq!(sst.last_key(), b"only");
        assert_eq!(
            records_in_file(&sst, &bytes),
            vec![("only".to_string(), "one".to_string())]
        );
    }

    // 写文件失败（目录不存在）时返回读写错误，不 panic。
    #[test]
    fn build_reports_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("没有这个目录").join("1.sst");
        let mut builder = SsTableBuilder::new(4096);
        builder.add(b"a", b"1");

        let result = builder.build(1, &path);

        assert!(matches!(result, Err(Error::Io(_))));
    }

    // 一条记录都没有的构建器不能写成文件。
    #[test]
    #[should_panic(expected = "至少要有一条记录")]
    fn build_panics_without_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.sst");
        let builder = SsTableBuilder::new(4096);

        let _ = builder.build(1, &path);
    }
}
