//! WAL（预写日志）：一张 memtable 收到的全部写入，按发生的顺序追加在一个文件里。
//!
//! 写入先追加到 WAL，再插入 memtable；崩溃之后重放 WAL，重建那张 memtable。一个 WAL 文件
//! 只属于一张 memtable：那张表写成 SST 并记进 manifest 之后，这个文件就没用了。
//!
//! 文件的样子（所有整数都是大端）：
//!
//! ```text
//! ┌──────────┬──────────┬──────────┬───┐
//! │ 第 0 条   │ 第 1 条   │ 第 2 条  │ … │
//! └──────────┴──────────┴──────────┴───┘
//!
//! 一条： 长度（4 字节）│ 内容（"长度"个字节）│ 内容的 crc32（4 字节）
//! 内容： key 的长度（2 字节）│ key │ value 的长度（2 字节）│ value
//! ```
//!
//! value 为空是墓碑。外面一层与 manifest 的帧相同，只管"这一段完整、没被改过"，残尾的
//! 处理也相同；里面一层只管 key 与 value 怎么排。以后一条记录要放多对 key 与 value
//! （写批次）时，只改里面一层。

use bytes::{BufMut, Bytes};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::block::{MAX_KEY_LEN, MAX_VALUE_LEN};
use crate::error::{Error, Result};
use crate::manifest::sync_parent_dir;

/// 一条记录的头部：内容的长度，4 字节。
const LEN_SIZE: usize = 4;
/// 一条记录的尾部：内容的校验和，4 字节。
const CHECKSUM_SIZE: usize = 4;
/// 内容里 key 的长度与 value 的长度各占 2 字节。
const ENTRY_LEN_SIZE: usize = 2;

/// 打开着的 WAL 文件，只往末尾追加。
///
/// 自己不带锁：追加由调用方排队（[`crate::mem_table::MemTable::put`] 的锁，它同时保证
/// WAL 里的顺序与插入表的顺序一致）。同步可以与追加同时进行。
pub struct Wal {
    file: File,
    /// 只在测试里用：为真时每次追加之后停一小段长短不一的时间，把"追加完、还没插入表"
    /// 这段间隙拉长，让并发写入的交错容易出现。
    #[cfg(test)]
    slow_appends: std::sync::atomic::AtomicBool,
}

impl Wal {
    /// 新建一个空的 WAL。文件已经存在时返回错误，不覆盖。
    ///
    /// 新建之后同步它所在的目录，让"目录里有了这个文件"落到磁盘上：否则之后即使同步了
    /// 文件的内容，断电后也可能连文件名都找不到。
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)?;
        sync_parent_dir(path)?;
        Ok(Self::from_file(file))
    }

    /// 打开已有的 WAL，按写入的顺序读出全部 key 与 value。
    ///
    /// 末尾不完整的一条（崩溃时只写了一半）从文件里截掉并同步，之后的追加接在最后一条
    /// 完整的记录后面。中间某一条损坏时返回 [`Error::Corruption`]。
    pub fn recover(path: &Path) -> Result<(Self, Vec<(Bytes, Bytes)>)> {
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        let (entries, valid_len) = decode_entries(&buf)?;
        if valid_len < buf.len() {
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }
        Ok((Self::from_file(file), entries))
    }

    /// 在末尾追加一条记录。只写进操作系统的页缓存、不同步：返回之后进程崩溃不会丢，
    /// 断电可能丢。要落到磁盘上，再调用 [`Wal::sync`]。
    ///
    /// 整条记录用一次 `write_all` 写出，不先放进进程里的缓冲区：缓冲区里的数据进程一崩溃就没了。
    /// key 与 value 的长度上限见 [`encode_entry`]。同一时刻只能有一个线程追加。
    ///
    /// 写到一半失败时，文件末尾可能留下半条记录，之后再追加，它就成了中间的损坏。
    pub fn append(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let frame = encode_entry(key, value);
        // `&File` 也实现了 `Write`：写文件不需要 `&mut`，由操作系统维护文件的位置。
        let mut file = &self.file;
        file.write_all(&frame)?;
        #[cfg(test)]
        if self.slow_appends.load(std::sync::atomic::Ordering::Relaxed) {
            let pause_micros = crc32fast::hash(&frame) % 50;
            std::thread::sleep(std::time::Duration::from_micros(pause_micros.into()));
        }
        Ok(())
    }

    /// 把已经追加的记录同步到磁盘。返回之后，之前追加的记录断电也不会丢。
    pub fn sync(&self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn from_file(file: File) -> Self {
        Self {
            file,
            #[cfg(test)]
            slow_appends: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 只在测试里用：之后每次追加都停一小段时间，见 `slow_appends`。
    #[cfg(test)]
    pub(crate) fn make_appends_slow(&self) {
        self.slow_appends
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 一对 key 与 value 编码之后在文件里占多少字节，与 [`encode_entry`] 的结果一样长。
pub fn encoded_len(key_len: usize, value_len: usize) -> usize {
    LEN_SIZE + ENTRY_LEN_SIZE + key_len + ENTRY_LEN_SIZE + value_len + CHECKSUM_SIZE
}

/// 把一对 key 与 value 编码成文件里的一条记录：长度、内容、校验和。
///
/// key 不能超过 [`MAX_KEY_LEN`]、value 不能超过 [`MAX_VALUE_LEN`] 字节（长度字段只有 2 字节），
/// 超过时 panic：写入的入口先检查长度，超长的写入到不了这里。
pub fn encode_entry(key: &[u8], value: &[u8]) -> Vec<u8> {
    assert!(key.len() <= MAX_KEY_LEN, "key 超过了 65535 字节");
    assert!(value.len() <= MAX_VALUE_LEN, "value 超过了 65535 字节");
    let body_len = 2 * ENTRY_LEN_SIZE + key.len() + value.len();
    let mut body = Vec::with_capacity(body_len);
    body.put_u16(key.len() as u16);
    body.put_slice(key);
    body.put_u16(value.len() as u16);
    body.put_slice(value);
    let crc32 = crc32fast::hash(&body);
    let mut frame = Vec::with_capacity(LEN_SIZE + body_len + CHECKSUM_SIZE);
    frame.put_u32(body_len as u32);
    frame.extend_from_slice(&body);
    frame.put_u32(crc32);
    frame
}

/// 从文件的全部内容里逐条解出 key 与 value，返回解出的结果，以及它们一共占了多少字节
/// （最后一条完整记录的末尾）。
///
/// 外面一层的规则与 manifest 相同：
///
/// - 剩下的字节不够一条完整的记录（长度不够 4 字节，或者长度说的字节数超出了剩下的）：
///   是只写了一半的残尾，停下，不算错误。
/// - 一条完整记录的校验和对不上：它是文件的最后一条时也当作残尾；后面还有记录时说明文件坏了，
///   返回 [`Error::Corruption`]。
/// - 校验和对得上，内容却解不出一对 key 与 value（见 [`decode_body`]）：返回
///   [`Error::Corruption`]。
pub fn decode_entries(buf: &[u8]) -> Result<(Vec<(Bytes, Bytes)>, usize)> {
    let mut offset = 0;
    let mut wal_records = Vec::new();
    while offset < buf.len() {
        let remaining = &buf[offset..];
        if remaining.len() < LEN_SIZE {
            break;
        }
        let body_len = u32::from_be_bytes(remaining[..LEN_SIZE].try_into().unwrap());
        // 这一条的总长：长度、内容、校验和。长度来自文件，可能是任意值，溢出时当作残尾。
        let frame_len = usize::try_from(body_len)
            .ok()
            .and_then(|len| len.checked_add(LEN_SIZE + CHECKSUM_SIZE));
        let Some(frame_len) = frame_len else { break };
        if remaining.len() < frame_len {
            break;
        }
        // 上面确认过总长没有溢出，这里的 `as usize` 不会截断。
        let body_end = LEN_SIZE + body_len as usize;
        let body = &remaining[LEN_SIZE..body_end];
        let crc = u32::from_be_bytes(remaining[body_end..frame_len].try_into().unwrap());
        if crc != crc32fast::hash(body) {
            // 坏的是文件的最后一条：写到一半断电留下的残尾，停下。
            if offset + frame_len == buf.len() {
                break;
            }
            // 后面还有记录：这一条当初写完了，是之后被改坏的。
            return Err(Error::Corruption(format!(
                "WAL 第 {} 条记录（第 {offset} 字节起）的校验和不对",
                wal_records.len()
            )));
        }
        match decode_body(body) {
            Some(record) => {
                wal_records.push(record);
            }
            None => {
                return Err(Error::Corruption(format!(
                    "WAL 第 {} 条记录（第 {offset} 字节起）的内容解不出 key 与 value",
                    wal_records.len()
                )));
            }
        }
        offset += frame_len;
    }
    Ok((wal_records, offset))
}

/// 解出一条记录的内容：key 的长度、key、value 的长度、value。
///
/// 两个长度与内容的总长对不上时返回 `None`：字节不够，或者解完还剩下多余的字节。
pub fn decode_body(body: &[u8]) -> Option<(Bytes, Bytes)> {
    let body_len = body.len();
    let mut offset = 0;
    if body_len < offset + ENTRY_LEN_SIZE {
        return None;
    }
    let key_len =
        u16::from_be_bytes(body[offset..offset + ENTRY_LEN_SIZE].try_into().unwrap()) as usize;
    offset += ENTRY_LEN_SIZE;
    if body_len < offset + key_len {
        return None;
    }
    let key = &body[offset..offset + key_len];
    offset += key_len;
    if body_len < offset + ENTRY_LEN_SIZE {
        return None;
    }
    let value_len =
        u16::from_be_bytes(body[offset..offset + ENTRY_LEN_SIZE].try_into().unwrap()) as usize;
    offset += ENTRY_LEN_SIZE;
    if body_len < offset + value_len {
        return None;
    }
    let value = &body[offset..offset + value_len];
    offset += value_len;
    if body_len > offset {
        return None;
    }
    Some((Bytes::copy_from_slice(key), Bytes::copy_from_slice(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把解出的结果变成字符串，方便比较。
    fn as_strings(entries: &[(Bytes, Bytes)]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|(key, value)| {
                let key = String::from_utf8(key.to_vec()).unwrap();
                let value = String::from_utf8(value.to_vec()).unwrap();
                (key, value)
            })
            .collect()
    }

    /// 三条记录首尾相接：一条普通的、一条墓碑、再一条普通的。返回全部字节与每条的末尾位置。
    fn three_frames() -> (Vec<u8>, Vec<usize>) {
        let mut buf = Vec::new();
        let mut ends = Vec::new();
        for (key, value) in [("apple", "red"), ("banana", ""), ("cherry", "dark red")] {
            buf.extend_from_slice(&encode_entry(key.as_bytes(), value.as_bytes()));
            ends.push(buf.len());
        }
        (buf, ends)
    }

    /// 用给定的内容拼一条校验和正确的记录：内容可以是任意字节，用来造"校验和对、内容不对"的记录。
    fn frame_with_body(body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.put_u32(body.len() as u32);
        frame.put_slice(body);
        frame.put_u32(crc32fast::hash(body));
        frame
    }

    // 一条记录的字节逐个符合文件格式：长度、key 的长度、key、value 的长度、value、校验和。
    #[test]
    fn encode_entry_follows_the_format() {
        let frame = encode_entry(b"ab", b"xyz");

        let body = [0, 2, b'a', b'b', 0, 3, b'x', b'y', b'z'];
        let mut expected = vec![0, 0, 0, 9];
        expected.extend_from_slice(&body);
        expected.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
        assert_eq!(frame, expected);
    }

    // 算出的长度与真正编码出来的一样。
    #[test]
    fn encoded_len_matches_encode_entry() {
        for (key, value) in [(&b"k"[..], &b""[..]), (b"apple", b"red"), (b"ab", b"xyz")] {
            assert_eq!(
                encoded_len(key.len(), value.len()),
                encode_entry(key, value).len()
            );
        }
    }

    // 墓碑的 value 长度是 0，后面没有 value 的字节。
    #[test]
    fn encode_entry_writes_tombstone_with_zero_value_len() {
        let frame = encode_entry(b"k", b"");

        assert_eq!(&frame[..LEN_SIZE], &[0, 0, 0, 5]);
        assert_eq!(&frame[LEN_SIZE..LEN_SIZE + 5], &[0, 1, b'k', 0, 0]);
        assert_eq!(frame.len(), LEN_SIZE + 5 + CHECKSUM_SIZE);
    }

    // 长度到上限的 key 与 value 能写也能读回来。
    #[test]
    fn encode_and_decode_longest_key_and_value() {
        let key = vec![b'k'; MAX_KEY_LEN];
        let value = vec![b'v'; MAX_VALUE_LEN];
        let frame = encode_entry(&key, &value);

        let (entries, valid_len) = decode_entries(&frame).unwrap();

        assert_eq!(valid_len, frame.len());
        assert_eq!(entries, vec![(Bytes::from(key), Bytes::from(value))]);
    }

    // 超长的 key 在编码时 panic，不会写出长度字段装不下的记录。
    #[test]
    #[should_panic(expected = "key 超过了 65535 字节")]
    fn encode_entry_panics_on_too_long_key() {
        let key = vec![b'k'; MAX_KEY_LEN + 1];
        encode_entry(&key, b"v");
    }

    // 几条记录按写入的顺序全部解出来，墓碑解出空的 value，占用的字节是全部。
    #[test]
    fn decode_entries_returns_all_in_order() {
        let (buf, _) = three_frames();

        let (entries, valid_len) = decode_entries(&buf).unwrap();

        assert_eq!(valid_len, buf.len());
        let expected = vec![
            ("apple".to_string(), "red".to_string()),
            ("banana".to_string(), String::new()),
            ("cherry".to_string(), "dark red".to_string()),
        ];
        assert_eq!(as_strings(&entries), expected);
    }

    // 空文件解出零条，占用 0 字节。
    #[test]
    fn decode_entries_on_empty_buffer() {
        let (entries, valid_len) = decode_entries(&[]).unwrap();

        assert!(entries.is_empty());
        assert_eq!(valid_len, 0);
    }

    // 从任何一个位置截断，都只解出截断之前完整的那几条，占用的字节停在最后一条完整记录的末尾。
    #[test]
    fn decode_entries_stops_at_incomplete_tail_at_every_cut() {
        let (buf, ends) = three_frames();

        for cut in 0..buf.len() {
            let (entries, valid_len) = decode_entries(&buf[..cut]).unwrap();

            let complete = ends.iter().filter(|&&end| end <= cut).count();
            let expected_len = if complete == 0 { 0 } else { ends[complete - 1] };
            assert_eq!(entries.len(), complete, "截在第 {cut} 字节");
            assert_eq!(valid_len, expected_len, "截在第 {cut} 字节");
        }
    }

    // 长度字段是一个很大的数：当作残尾停下，不越界、不 panic。
    #[test]
    fn decode_entries_treats_huge_length_as_tail() {
        let mut buf = encode_entry(b"k", b"v");
        let first_end = buf.len();
        buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 1, 2, 3]);

        let (entries, valid_len) = decode_entries(&buf).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(valid_len, first_end);
    }

    // 最后一条的校验和对不上：当作写到一半的残尾，前面的照常解出。
    #[test]
    fn decode_entries_treats_bad_last_checksum_as_tail() {
        let (mut buf, ends) = three_frames();
        // 改坏第 3 条内容里的一个字节。
        buf[ends[1] + LEN_SIZE + 3] ^= 0xff;

        let (entries, valid_len) = decode_entries(&buf).unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(valid_len, ends[1]);
    }

    // 中间一条的校验和对不上：文件坏了，返回 Corruption。
    #[test]
    fn decode_entries_rejects_bad_checksum_in_the_middle() {
        let (mut buf, ends) = three_frames();
        // 改坏第 2 条内容里的一个字节。
        buf[ends[0] + LEN_SIZE + 3] ^= 0xff;

        let result = decode_entries(&buf);

        assert!(matches!(result, Err(Error::Corruption(_))), "{result:?}");
    }

    // 校验和对得上、内容解不出来：即使是最后一条也是 Corruption，不当作残尾。
    #[test]
    fn decode_entries_rejects_malformed_body_even_at_the_end() {
        let mut buf = encode_entry(b"k", b"v");
        // key 的长度说有 5 字节，实际只跟了 1 个字节。
        buf.extend_from_slice(&frame_with_body(&[0, 5, b'a']));

        let result = decode_entries(&buf);

        assert!(matches!(result, Err(Error::Corruption(_))), "{result:?}");
    }

    // 正常的内容解出 key 与 value；value 长度为 0 时解出空的 value。
    #[test]
    fn decode_body_reads_key_and_value() {
        let body = [0, 2, b'a', b'b', 0, 3, b'x', b'y', b'z'];
        assert_eq!(
            decode_body(&body),
            Some((Bytes::from_static(b"ab"), Bytes::from_static(b"xyz")))
        );

        let tombstone = [0, 1, b'k', 0, 0];
        assert_eq!(
            decode_body(&tombstone),
            Some((Bytes::from_static(b"k"), Bytes::new()))
        );
    }

    // 内容的字节不够或者有多余：每一种都返回 None，不越界。
    #[test]
    fn decode_body_rejects_wrong_lengths() {
        let cases: [&[u8]; 6] = [
            // 连 key 的长度都不够。
            &[],
            &[0],
            // key 不够长。
            &[0, 3, b'a', b'b'],
            // key 完整，value 的长度缺 1 个字节。
            &[0, 1, b'a', 0],
            // value 不够长。
            &[0, 1, b'a', 0, 2, b'x'],
            // 解完还多出 1 个字节。
            &[0, 1, b'a', 0, 1, b'x', b'!'],
        ];
        for body in cases {
            assert_eq!(decode_body(body), None, "{body:?}");
        }
    }

    // 新建、追加几条、同步，之后打开能按顺序读回来。
    #[test]
    fn wal_recover_reads_back_appended_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(b"a", b"1").unwrap();
        wal.append(b"b", b"").unwrap();
        wal.append(b"a", b"2").unwrap();
        wal.sync().unwrap();
        drop(wal);

        let (_wal, entries) = Wal::recover(&path).unwrap();

        let expected = vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), String::new()),
            ("a".to_string(), "2".to_string()),
        ];
        assert_eq!(as_strings(&entries), expected);
    }

    // 文件已经存在时 create 返回错误，不覆盖原有的内容。
    #[test]
    fn wal_create_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(b"a", b"1").unwrap();
        drop(wal);

        assert!(Wal::create(&path).is_err());

        let (_wal, entries) = Wal::recover(&path).unwrap();
        assert_eq!(entries.len(), 1);
    }

    // 打开时截掉写了一半的残尾；之后的追加接在最后一条完整记录后面，再打开能读到。
    #[test]
    fn wal_recover_truncates_tail_and_appends_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(b"a", b"1").unwrap();
        wal.append(b"b", b"2").unwrap();
        drop(wal);
        let complete_len = std::fs::metadata(&path).unwrap().len();
        // 模拟崩溃：第三条只写进去一半。
        let half = encode_entry(b"c", b"3");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&half[..half.len() / 2]).unwrap();
        drop(file);

        let (wal, entries) = Wal::recover(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), complete_len);
        wal.append(b"d", b"4").unwrap();
        drop(wal);

        let (_wal, entries) = Wal::recover(&path).unwrap();
        let expected = vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
            ("d".to_string(), "4".to_string()),
        ];
        assert_eq!(as_strings(&entries), expected);
    }

    // 中间一条损坏时打开返回 Corruption，文件不被截短。
    #[test]
    fn wal_recover_rejects_corruption_in_the_middle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(b"a", b"1").unwrap();
        wal.append(b"b", b"2").unwrap();
        drop(wal);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[LEN_SIZE + 2] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let result = Wal::recover(&path);

        assert!(matches!(result, Err(Error::Corruption(_))));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
