//! manifest：记下"每一层现在有哪些 SST"的一份日志，重启时重放它恢复文件列表。
//!
//! 运行期间只往末尾追加、不读：读写用的是内存里的状态，manifest 是留给下次打开的。
//! 每次换状态（flush、合并、挪动）追加一条 [`VersionEdit`]，记下去掉了哪些文件、加上了
//! 哪些、放在哪里；文件的第一条是 [`ManifestRecord::Init`]，记下数据库的形状。
//!
//! 文件的样子（所有整数都是大端）：
//!
//! ```text
//! ┌──────────┬──────────┬──────────┬───┐
//! │ 第 0 条   │ 第 1 条   │ 第 2 条  │ … │
//! └──────────┴──────────┴──────────┴───┘
//!
//! 一条： 长度（8 字节）│ 记录的 JSON（"长度"个字节）│ JSON 的 crc32（4 字节）
//! ```
//!
//! 崩溃时最后一条可能只写了一半。打开时读到不完整的最后一条，当作没写过，从文件里截掉；
//! 中间的某一条对不上校验和，说明文件坏了，返回错误。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use bytes::BufMut;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// manifest 在数据库目录里的文件名。
pub const MANIFEST_FILE_NAME: &str = "MANIFEST";

/// 一条记录的头部：记录的长度，8 字节。
const LEN_SIZE: usize = 8;
/// 一条记录的尾部：校验和，4 字节。
const CHECKSUM_SIZE: usize = 4;

/// manifest 的一条记录。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ManifestRecord {
    /// 第一条：数据库的形状。之后每次打开都与选项核对，对不上就返回错误。
    Init(Shape),
    /// 一次换状态：flush、各种合并、trivial move、全量合并都用它。
    Edit(VersionEdit),
}

/// 数据库的形状：状态里的 `levels` 是"L1 往下各层"还是"一列 tier"。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// L0 加 `max_levels` 层：不合并、简单分层、leveled 都是这种，可以互换。
    Levels { max_levels: usize },
    /// 一列 tier，没有 L0。
    Tiers,
}

/// 一次换状态：去掉哪些文件，加上哪些、放在哪里。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VersionEdit {
    /// 去掉的文件编号。编号在整个数据库里唯一，所以不用写在哪一层。
    pub removed: Vec<usize>,
    /// 加上的文件编号，放到哪里由 `placement` 决定。
    pub added: Vec<usize>,
    /// `added` 放到哪里。
    pub placement: Placement,
    /// 追加这条记录时下一个要分配的编号。重放后取最后一条的。
    pub next_sst_id: usize,
    /// 这次换状态把编号为多少的 memtable 写成了 SST：flush 才有，合并、挪动是 `None`。
    /// flush 从最旧的表开始一张一张做，所以编号不超过它的表都已写成 SST，它们的 WAL 不用再重放。
    /// 是 `None` 时不写进 JSON；L14 之前写的记录也没有这一项，读出来都是 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flushed_memtable_id: Option<usize>,
}

/// 加上的文件放到哪里。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// 放到 L0 的最前面（分层时的 flush）。
    L0,
    /// 放进第 n 层（1 是 L1）。层内的顺序不记，重放完、打开文件之后按首 key 排。
    Level(usize),
    /// 作为一个新 tier，放在被去掉的那几个 tier 原来的位置；没有去掉任何文件时放在
    /// 最前面（tiered 时的 flush）。
    Tier,
}

/// 打开着的 manifest 文件，只往末尾追加。
pub struct Manifest {
    file: File,
}

impl Manifest {
    /// 新建一个空的 manifest。文件已经存在时返回错误，不覆盖。
    ///
    /// 新建之后同步文件与它所在的目录，让"目录里有了这个文件"落到磁盘上。
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)?;
        file.sync_all()?;
        sync_parent_dir(path)?;
        Ok(Self { file })
    }

    /// 打开已有的 manifest，读出全部记录。
    ///
    /// 末尾不完整的一条（崩溃时只写了一半）从文件里截掉并同步，之后的追加接在最后一条
    /// 完整的记录后面。中间某一条损坏时返回 [`Error::Corruption`]。
    pub fn recover(path: &Path) -> Result<(Self, Vec<ManifestRecord>)> {
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        let (records, valid_len) = decode_records(&buf)?;
        if valid_len < buf.len() {
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }
        Ok((Self { file }, records))
    }

    /// 在末尾追加一条记录并同步。返回时这条记录已经在磁盘上。
    pub fn append(&mut self, record: &ManifestRecord) -> Result<()> {
        let frame = encode_record(record);
        self.file.write_all(&frame)?;
        self.file.sync_all()?;
        Ok(())
    }
}

/// 同步 `path` 所在的目录。
pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// 把一条记录编码成文件里的一段：长度、JSON、校验和。
pub fn encode_record(record: &ManifestRecord) -> Vec<u8> {
    let body = serde_json::to_vec(record).expect("记录里只有整数与枚举，编码不会失败");
    let checksum = crc32fast::hash(&body);
    let mut frame = Vec::with_capacity(LEN_SIZE + body.len() + CHECKSUM_SIZE);
    frame.put_u64(body.len() as u64);
    frame.put_slice(&body);
    frame.put_u32(checksum);
    frame
}

/// 从文件的全部内容里逐条解出记录，返回解出的记录，以及它们一共占了多少字节
/// （最后一条完整记录的末尾）。
///
/// - 剩下的字节不够一条完整的记录（长度不够 8 字节，或者长度说的字节数超出了剩下的）：
///   是只写了一半的残尾，停下，不算错误。
/// - 一条完整记录的校验和对不上：它是文件的最后一条时也当作残尾（写到一半断电，文件长度
///   先变长了、内容没写完）；后面还有记录时说明文件坏了，返回 [`Error::Corruption`]。
/// - 校验和对得上，JSON 却解不出 [`ManifestRecord`]：返回 [`Error::Corruption`]。
///
/// 长度来自文件，可能是任意值，计算位置时不能溢出。
fn decode_records(buf: &[u8]) -> Result<(Vec<ManifestRecord>, usize)> {
    let mut offset = 0;
    let mut manifest_records: Vec<ManifestRecord> = Vec::new();
    while offset < buf.len() {
        let remaining = &buf[offset..];
        if remaining.len() < LEN_SIZE {
            break;
        }
        let body_len = u64::from_be_bytes(remaining[..LEN_SIZE].try_into().unwrap());
        // 这一条的总长：长度、JSON、校验和。长度来自文件，可能是任意值，溢出时当作残尾。
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
                "manifest 第 {} 条记录（第 {offset} 字节起）的校验和不对",
                manifest_records.len()
            )));
        }
        let record = serde_json::from_slice::<ManifestRecord>(body).map_err(|error| {
            Error::Corruption(format!(
                "manifest 第 {} 条记录（第 {offset} 字节起）解不出来：{error}",
                manifest_records.len()
            ))
        })?;
        manifest_records.push(record);
        offset += frame_len;
    }
    Ok((manifest_records, offset))
}

/// 重放 manifest 得到的文件列表。只有编号，文件还没有打开。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredLayout {
    /// 数据库的形状，来自第一条记录。
    pub shape: Shape,
    /// L0 的文件编号，新的在前。tiered 时一直是空的。
    pub l0: Vec<usize>,
    /// 分层时是 L1 往下各层的编号，第 0 项是 L1，层内的顺序没有意义，打开文件之后按首 key 排；
    /// tiered 时是各个 tier，第 0 个最新，tier 内是合并输出的顺序。
    pub levels: Vec<Vec<usize>>,
    /// 下一个要分配的编号。
    pub next_sst_id: usize,
    /// 已经写成 SST 的 memtable 里编号最大的一张（高水位）：编号不超过它的 WAL 都没用了。
    /// `None` 表示还没有 flush 过。
    pub flushed_memtable_id: Option<usize>,
}

impl RecoveredLayout {
    /// 一个刚建好的数据库：没有文件，编号从 1 开始分配。分层时有 `max_levels` 个空层，
    /// tiered 时一个 tier 都没有。
    pub fn new(shape: Shape) -> Self {
        let levels = match shape {
            Shape::Levels { max_levels } => vec![Vec::new(); max_levels],
            Shape::Tiers => Vec::new(),
        };
        Self {
            shape,
            l0: Vec::new(),
            levels,
            next_sst_id: 1,
            flushed_memtable_id: None,
        }
    }
}

/// 从头重放全部记录，得到崩溃（或关闭）之前的文件列表。
///
/// 第一条必须是 [`ManifestRecord::Init`]，之后全是 [`ManifestRecord::Edit`]；不是这样，
/// 或者某一条与当时的文件列表对不上（见 [`apply_edit`]），返回 [`Error::Corruption`]。
pub fn replay(records: &[ManifestRecord]) -> Result<RecoveredLayout> {
    let Some((first, rest)) = records.split_first() else {
        return Err(Error::Corruption("manifest 里没有记录".to_string()));
    };
    let ManifestRecord::Init(shape) = first else {
        return Err(Error::Corruption(
            "manifest 的第一条记录不是 Init".to_string(),
        ));
    };
    let mut layout = RecoveredLayout::new(*shape);
    for (index, record) in rest.iter().enumerate() {
        let ManifestRecord::Edit(edit) = record else {
            return Err(Error::Corruption(format!(
                "manifest 第 {} 条记录又是 Init",
                index + 1
            )));
        };
        apply_edit(&mut layout, edit)?;
        layout.next_sst_id = edit.next_sst_id;
        if let Some(flushed) = edit.flushed_memtable_id {
            // flush 从最旧的表开始做，表的编号按冻结的先后递增：高水位只会往上走。
            if layout
                .flushed_memtable_id
                .is_some_and(|previous| flushed <= previous)
            {
                return Err(Error::Corruption(format!(
                    "manifest 第 {} 条记录 flush 的 memtable {flushed} 不比之前的大",
                    index + 1
                )));
            }
            layout.flushed_memtable_id = Some(flushed);
        }
    }
    Ok(layout)
}

/// 把一条记录用到文件列表上：先去掉 `edit.removed`，再按 `edit.placement` 放上 `edit.added`。
///
/// - `L0`：放到 L0 的最前面，保持 `added` 里的顺序。
/// - `Level(n)`：接在第 n 层（1 是 L1）的后面；层内的顺序之后按首 key 排，这里不管。
///   n 是 0 或大于层数时返回 [`Error::Corruption`]。
/// - `Tier`：`added` 作为一个新 tier，放在被去掉的那几个 tier 原来的位置，也就是
///   `removed` 的第一个文件所在的 tier 的位置（要在去掉之前找）；`removed` 是空的时候
///   放在最前面。去掉之后变空的 tier 要拿掉；`added` 是空的时候不放新 tier。
///
/// 分层的形状只能用 `L0`、`Level`，tiered 只能用 `Tier`，对不上时返回 [`Error::Corruption`]。
/// 要去掉的文件不在列表里时，`remove_ids` 返回 [`Error::Corruption`]。
fn apply_edit(layout: &mut RecoveredLayout, edit: &VersionEdit) -> Result<()> {
    match (layout.shape, edit.placement) {
        (Shape::Levels { .. }, Placement::L0) => {
            remove_ids(layout, &edit.removed)?;
            layout.l0.splice(0..0, edit.added.iter().copied());
        }
        (Shape::Levels { max_levels }, Placement::Level(level)) => {
            if level == 0 || level > max_levels {
                return Err(Error::Corruption(format!(
                    "manifest 里的层号 {level} 超出了 1 到 {max_levels}"
                )));
            }
            remove_ids(layout, &edit.removed)?;
            layout.levels[level - 1].extend(edit.added.iter().copied());
        }
        (Shape::Tiers, Placement::Tier) => {
            let start = match edit.removed.first() {
                Some(&first_removed) => {
                    find_list(&layout.levels, first_removed).ok_or_else(|| {
                        Error::Corruption(format!(
                            "manifest 要去掉的文件 {first_removed} 不在任何 tier 里"
                        ))
                    })?
                }
                None => 0,
            };
            remove_ids(layout, &edit.removed)?;
            layout.levels.retain(|tier| !tier.is_empty());
            if !edit.added.is_empty() {
                layout.levels.insert(start, edit.added.clone());
            }
        }
        (shape, placement) => {
            return Err(Error::Corruption(format!(
                "manifest 里的放法 {placement:?} 与形状 {shape:?} 不符"
            )));
        }
    }
    Ok(())
}

/// `lists` 里第几个列表含有编号 `id`；都没有时返回 `None`。
fn find_list(lists: &[Vec<usize>], id: usize) -> Option<usize> {
    lists.iter().position(|list| list.contains(&id))
}

/// 从 L0 与各层（或各个 tier）里去掉这些编号。某个编号哪里都找不到时返回
/// [`Error::Corruption`]：记录说要去掉一个不存在的文件，manifest 与它记下的历史对不上。
///
/// 去掉之后变空的 tier 不在这里拿掉，由调用方决定。
fn remove_ids(layout: &mut RecoveredLayout, ids: &[usize]) -> Result<()> {
    for &id in ids {
        if let Some(index) = layout.l0.iter().position(|&x| x == id) {
            layout.l0.remove(index);
            continue;
        }
        let Some(list_index) = find_list(&layout.levels, id) else {
            return Err(Error::Corruption(format!(
                "manifest 要去掉的文件 {id} 不在文件列表里"
            )));
        };
        let list = &mut layout.levels[list_index];
        let index = list.iter().position(|&x| x == id).unwrap();
        list.remove(index);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 几条不同种类的记录：形状、分层的 flush、合并、tiered 的 flush。
    fn sample_records() -> Vec<ManifestRecord> {
        vec![
            ManifestRecord::Init(Shape::Levels { max_levels: 3 }),
            ManifestRecord::Edit(VersionEdit {
                removed: vec![],
                added: vec![1],
                placement: Placement::L0,
                next_sst_id: 2,
                flushed_memtable_id: None,
            }),
            ManifestRecord::Edit(VersionEdit {
                removed: vec![2, 1],
                added: vec![3, 4],
                placement: Placement::Level(1),
                next_sst_id: 5,
                flushed_memtable_id: None,
            }),
            ManifestRecord::Edit(VersionEdit {
                removed: vec![],
                added: vec![5],
                placement: Placement::Tier,
                next_sst_id: 6,
                flushed_memtable_id: None,
            }),
        ]
    }

    /// 把一串记录依次编码、接在一起，返回整段字节和每条记录的结束位置。
    fn encode_all(records: &[ManifestRecord]) -> (Vec<u8>, Vec<usize>) {
        let mut buf = Vec::new();
        let mut ends = Vec::new();
        for record in records {
            buf.extend_from_slice(&encode_record(record));
            ends.push(buf.len());
        }
        (buf, ends)
    }

    /// 手工拼一条记录：给定的内容，配上它自己的长度与校验和。
    fn frame_of(body: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.put_u64(body.len() as u64);
        frame.put_slice(body);
        frame.put_u32(crc32fast::hash(body));
        frame
    }

    // ---------- 编码与解码 ----------

    // 一条记录编码后是"8 字节长度 + JSON + 4 字节校验和"，JSON 是人能读的样子。
    #[test]
    fn encode_writes_length_json_and_checksum() {
        let record = ManifestRecord::Edit(VersionEdit {
            removed: vec![2],
            added: vec![3],
            placement: Placement::Level(1),
            next_sst_id: 4,
            flushed_memtable_id: None,
        });

        let frame = encode_record(&record);

        let json =
            r#"{"Edit":{"removed":[2],"added":[3],"placement":{"Level":1},"next_sst_id":4}}"#;
        assert_eq!(&frame[..8], &(json.len() as u64).to_be_bytes());
        assert_eq!(&frame[8..8 + json.len()], json.as_bytes());
        assert_eq!(
            &frame[8 + json.len()..],
            &crc32fast::hash(json.as_bytes()).to_be_bytes()
        );
    }

    // 空的内容：没有记录，占 0 字节。
    #[test]
    fn decode_empty_buffer() {
        let (records, valid_len) = decode_records(&[]).unwrap();
        assert!(records.is_empty());
        assert_eq!(valid_len, 0);
    }

    // 几条完整的记录：原样解出来，占满全部字节。
    #[test]
    fn decode_returns_every_complete_record() {
        let records = sample_records();
        let (buf, _) = encode_all(&records);

        let (decoded, valid_len) = decode_records(&buf).unwrap();

        assert_eq!(decoded, records);
        assert_eq!(valid_len, buf.len());
    }

    // 在任意一个字节处截断：只解出完整落在截断点之前的记录，占用的字节停在最后一条
    // 完整记录的末尾；不报错。
    #[test]
    fn decode_stops_at_a_truncated_tail_anywhere() {
        let records = sample_records();
        let (buf, ends) = encode_all(&records);

        for cut in 0..=buf.len() {
            let (decoded, valid_len) = decode_records(&buf[..cut]).unwrap();
            let complete = ends.iter().filter(|&&end| end <= cut).count();
            let expected_len = if complete == 0 { 0 } else { ends[complete - 1] };
            assert_eq!(decoded, records[..complete], "截在第 {cut} 字节");
            assert_eq!(valid_len, expected_len, "截在第 {cut} 字节");
        }
    }

    // 最后一条长度完整、内容被改了一个字节：当作残尾，前面的照常解出。
    #[test]
    fn decode_treats_a_bad_last_record_as_the_tail() {
        let records = sample_records();
        let (mut buf, ends) = encode_all(&records);
        let last_start = ends[ends.len() - 2];
        buf[last_start + 10] ^= 0xff;

        let (decoded, valid_len) = decode_records(&buf).unwrap();

        assert_eq!(decoded, records[..records.len() - 1]);
        assert_eq!(valid_len, last_start);
    }

    // 中间一条的内容被改了：后面还有记录，说明文件坏了，返回 Corruption。
    #[test]
    fn decode_reports_a_bad_record_in_the_middle() {
        let records = sample_records();
        let (mut buf, ends) = encode_all(&records);
        buf[ends[0] + 10] ^= 0xff;

        match decode_records(&buf) {
            Err(Error::Corruption(_)) => {}
            other => panic!("应当是 Corruption，实际是 {other:?}"),
        }
    }

    // 校验和对得上，内容却不是一条记录：返回 Corruption。
    #[test]
    fn decode_reports_a_record_that_is_not_json_of_a_record() {
        let mut buf = encode_record(&sample_records()[0]);
        buf.extend_from_slice(&frame_of(b"not a record"));

        match decode_records(&buf) {
            Err(Error::Corruption(_)) => {}
            other => panic!("应当是 Corruption，实际是 {other:?}"),
        }
    }

    // 长度字段是一个大得离谱的数（例如全是 0xff）：算位置时不能溢出，当作残尾。
    #[test]
    fn decode_survives_a_huge_length() {
        let first = encode_record(&sample_records()[0]);
        let mut buf = first.clone();
        buf.extend_from_slice(&u64::MAX.to_be_bytes());
        buf.extend_from_slice(b"garbage");

        let (decoded, valid_len) = decode_records(&buf).unwrap();

        assert_eq!(decoded, sample_records()[..1]);
        assert_eq!(valid_len, first.len());
    }

    // ---------- 文件：create、append、recover ----------

    // 新建、追加几条，再打开：读出的记录与写入的相同。
    #[test]
    fn recover_reads_back_appended_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE_NAME);
        let records = sample_records();

        let mut manifest = Manifest::create(&path).unwrap();
        for record in &records {
            manifest.append(record).unwrap();
        }
        drop(manifest);

        let (_, recovered) = Manifest::recover(&path).unwrap();
        assert_eq!(recovered, records);
    }

    // 文件已经存在时新建失败，原来的内容不动。
    #[test]
    fn create_refuses_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE_NAME);
        let mut manifest = Manifest::create(&path).unwrap();
        manifest.append(&sample_records()[0]).unwrap();
        drop(manifest);

        assert!(Manifest::create(&path).is_err());
        let (_, recovered) = Manifest::recover(&path).unwrap();
        assert_eq!(recovered, sample_records()[..1]);
    }

    // 文件不存在时打开失败，是 Io 错误。
    #[test]
    fn recover_reports_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE_NAME);

        match Manifest::recover(&path) {
            Err(Error::Io(error)) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
            Err(other) => panic!("应当是 Io，实际是 {other:?}"),
            Ok(_) => panic!("不存在的文件不应该打得开"),
        }
    }

    // 末尾有半条记录：打开时截掉；之后追加的记录接在完整记录后面，再打开能全部读出。
    #[test]
    fn recover_cuts_the_tail_before_new_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE_NAME);
        let records = sample_records();
        let mut manifest = Manifest::create(&path).unwrap();
        manifest.append(&records[0]).unwrap();
        manifest.append(&records[1]).unwrap();
        drop(manifest);
        let complete_len = std::fs::metadata(&path).unwrap().len();
        // 模拟崩溃：第三条只写了前 5 个字节。
        let half = &encode_record(&records[2])[..5];
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(half)
            .unwrap();

        let (mut manifest, recovered) = Manifest::recover(&path).unwrap();
        assert_eq!(recovered, records[..2]);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), complete_len);

        manifest.append(&records[3]).unwrap();
        drop(manifest);
        let (_, recovered) = Manifest::recover(&path).unwrap();
        assert_eq!(
            recovered,
            vec![records[0].clone(), records[1].clone(), records[3].clone()]
        );
    }

    // 中间损坏的文件打开失败，文件不被截断（不能把后面完好的记录一起丢掉）。
    #[test]
    fn recover_keeps_a_corrupted_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE_NAME);
        let (mut buf, ends) = encode_all(&sample_records());
        buf[ends[0] + 10] ^= 0xff;
        std::fs::write(&path, &buf).unwrap();

        match Manifest::recover(&path) {
            Err(Error::Corruption(_)) => {}
            Err(other) => panic!("应当是 Corruption，实际是 {other:?}"),
            Ok(_) => panic!("中间损坏的文件不应该打得开"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), buf);
    }

    // ---------- 重放：replay、apply_edit ----------

    /// 一条 Edit 记录。
    fn edit(
        removed: &[usize],
        added: &[usize],
        placement: Placement,
        next: usize,
    ) -> ManifestRecord {
        ManifestRecord::Edit(VersionEdit {
            removed: removed.to_vec(),
            added: added.to_vec(),
            placement,
            next_sst_id: next,
            flushed_memtable_id: None,
        })
    }

    /// 以分层的形状（L0 之下 3 层）开头，接上这些记录，重放。
    fn replay_levels(edits: Vec<ManifestRecord>) -> Result<RecoveredLayout> {
        let mut records = vec![ManifestRecord::Init(Shape::Levels { max_levels: 3 })];
        records.extend(edits);
        replay(&records)
    }

    /// 以 tiered 的形状开头，接上这些记录，重放。
    fn replay_tiers(edits: Vec<ManifestRecord>) -> Result<RecoveredLayout> {
        let mut records = vec![ManifestRecord::Init(Shape::Tiers)];
        records.extend(edits);
        replay(&records)
    }

    /// 期望的结果应当是 Corruption。
    fn assert_corruption(result: Result<RecoveredLayout>) {
        match result {
            Err(Error::Corruption(_)) => {}
            other => panic!("应当是 Corruption，实际是 {other:?}"),
        }
    }

    // 只有 Init：分层时是 L0 与 3 个空层，tiered 时一个 tier 都没有；编号从 1 开始。
    #[test]
    fn replay_init_only() {
        let levels = replay_levels(vec![]).unwrap();
        assert_eq!(levels.shape, Shape::Levels { max_levels: 3 });
        assert!(levels.l0.is_empty());
        assert_eq!(levels.levels, vec![Vec::<usize>::new(); 3]);
        assert_eq!(levels.next_sst_id, 1);

        let tiers = replay_tiers(vec![]).unwrap();
        assert!(tiers.l0.is_empty());
        assert!(tiers.levels.is_empty());
        assert_eq!(tiers.next_sst_id, 1);
    }

    // 没有记录、第一条不是 Init、中间又出现 Init：都是 Corruption。
    #[test]
    fn replay_needs_exactly_one_init_at_the_start() {
        assert_corruption(replay(&[]));
        assert_corruption(replay(&[edit(&[], &[1], Placement::L0, 2)]));
        assert_corruption(replay_levels(vec![ManifestRecord::Init(Shape::Tiers)]));
    }

    // 分层的 flush：新文件放到 L0 的最前面；下一个编号取最后一条记录的。
    #[test]
    fn replay_flushes_into_l0_newest_first() {
        let layout = replay_levels(vec![
            edit(&[], &[1], Placement::L0, 2),
            edit(&[], &[2], Placement::L0, 3),
            edit(&[], &[3], Placement::L0, 4),
        ])
        .unwrap();
        assert_eq!(layout.l0, vec![3, 2, 1]);
        assert_eq!(layout.levels, vec![Vec::<usize>::new(); 3]);
        assert_eq!(layout.next_sst_id, 4);
    }

    // L0 并进 L1：L0 拿掉输入，合并期间 flush 出的 3 留下；L1 换成输出。
    // 之后 L1 并进 L2 的一部分：两层的输入都拿掉，输出接在 L2 后面。
    #[test]
    fn replay_compactions_between_levels() {
        let layout = replay_levels(vec![
            edit(&[], &[1], Placement::L0, 2),
            edit(&[], &[2], Placement::L0, 3),
            edit(&[], &[3], Placement::L0, 4),
            edit(&[2, 1], &[4, 5], Placement::Level(1), 6),
            edit(&[], &[6], Placement::L0, 7),
            edit(&[5], &[7], Placement::Level(2), 8),
            edit(&[4, 7], &[8, 9], Placement::Level(2), 10),
        ])
        .unwrap();
        assert_eq!(layout.l0, vec![6, 3]);
        assert_eq!(layout.levels, vec![vec![], vec![8, 9], vec![]]);
        assert_eq!(layout.next_sst_id, 10);
    }

    // 一次合并同时去掉 L0 和 L1 的文件（简单分层的 L0 并进 L1）。
    #[test]
    fn replay_removes_from_l0_and_a_level_in_one_edit() {
        let layout = replay_levels(vec![
            edit(&[], &[1], Placement::L0, 2),
            edit(&[1], &[2], Placement::Level(1), 3),
            edit(&[], &[3], Placement::L0, 4),
            edit(&[3, 2], &[4], Placement::Level(1), 5),
        ])
        .unwrap();
        assert!(layout.l0.is_empty());
        assert_eq!(layout.levels, vec![vec![4], vec![], vec![]]);
    }

    // trivial move：同一个文件从 L1 去掉、放进 L2，编号不变。
    #[test]
    fn replay_trivial_move() {
        let layout = replay_levels(vec![
            edit(&[], &[1], Placement::L0, 2),
            edit(&[1], &[1], Placement::Level(1), 2),
            edit(&[1], &[1], Placement::Level(2), 2),
        ])
        .unwrap();
        assert!(layout.l0.is_empty());
        assert_eq!(layout.levels, vec![vec![], vec![1], vec![]]);
    }

    // 全量合并：所有文件换成最底层的输出；输出为空（全是墓碑）时只是全部拿掉。
    #[test]
    fn replay_full_compaction() {
        let layout = replay_levels(vec![
            edit(&[], &[1], Placement::L0, 2),
            edit(&[1], &[2], Placement::Level(1), 3),
            edit(&[], &[3], Placement::L0, 4),
            edit(&[3, 2], &[4, 5], Placement::Level(3), 6),
            edit(&[4, 5], &[], Placement::Level(3), 6),
        ])
        .unwrap();
        assert!(layout.l0.is_empty());
        assert_eq!(layout.levels, vec![Vec::<usize>::new(); 3]);
        assert_eq!(layout.next_sst_id, 6);
    }

    // 要去掉的文件不存在：Corruption。
    #[test]
    fn replay_rejects_removing_a_missing_file() {
        assert_corruption(replay_levels(vec![
            edit(&[], &[1], Placement::L0, 2),
            edit(&[7], &[2], Placement::Level(1), 3),
        ]));
        assert_corruption(replay_tiers(vec![
            edit(&[], &[1], Placement::Tier, 2),
            edit(&[7], &[2], Placement::Tier, 3),
        ]));
    }

    // 层号是 0 或超出层数：Corruption。
    #[test]
    fn replay_rejects_a_level_out_of_range() {
        assert_corruption(replay_levels(vec![edit(&[], &[1], Placement::Level(0), 2)]));
        assert_corruption(replay_levels(vec![edit(&[], &[1], Placement::Level(4), 2)]));
    }

    // 放法与形状不符：分层用了 Tier，tiered 用了 L0 或 Level。
    #[test]
    fn replay_rejects_a_placement_of_the_other_shape() {
        assert_corruption(replay_levels(vec![edit(&[], &[1], Placement::Tier, 2)]));
        assert_corruption(replay_tiers(vec![edit(&[], &[1], Placement::L0, 2)]));
        assert_corruption(replay_tiers(vec![edit(&[], &[1], Placement::Level(1), 2)]));
    }

    // tiered 的 flush：每个新文件自己成为最前面的一个 tier。
    #[test]
    fn replay_tiered_flushes_newest_first() {
        let layout = replay_tiers(vec![
            edit(&[], &[1], Placement::Tier, 2),
            edit(&[], &[2], Placement::Tier, 3),
            edit(&[], &[3], Placement::Tier, 4),
        ])
        .unwrap();
        assert!(layout.l0.is_empty());
        assert_eq!(layout.levels, vec![vec![3], vec![2], vec![1]]);
        assert_eq!(layout.next_sst_id, 4);
    }

    // tiered 的合并：合并 [3]、[2] 期间 flush 出了 [4]；新 tier 放在被合并的那两个原来的
    // 位置，也就是 [4] 之后、[1] 之前，不是最前面。
    #[test]
    fn replay_tiered_compaction_keeps_the_position() {
        let layout = replay_tiers(vec![
            edit(&[], &[1], Placement::Tier, 2),
            edit(&[], &[2], Placement::Tier, 3),
            edit(&[], &[3], Placement::Tier, 4),
            edit(&[], &[4], Placement::Tier, 5),
            edit(&[3, 2], &[5, 6], Placement::Tier, 7),
        ])
        .unwrap();
        assert_eq!(layout.levels, vec![vec![4], vec![5, 6], vec![1]]);
        assert_eq!(layout.next_sst_id, 7);
    }

    // tiered 的合并输出为空：只拿掉那几个 tier，不留空 tier。
    #[test]
    fn replay_tiered_compaction_with_empty_output() {
        let layout = replay_tiers(vec![
            edit(&[], &[1], Placement::Tier, 2),
            edit(&[], &[2], Placement::Tier, 3),
            edit(&[], &[3], Placement::Tier, 4),
            edit(&[3, 2], &[], Placement::Tier, 4),
        ])
        .unwrap();
        assert_eq!(layout.levels, vec![vec![1]]);
    }

    // tiered 的全量合并：多文件的 tier 也整个拿掉，全部换成一个 tier。
    #[test]
    fn replay_tiered_full_compaction() {
        let layout = replay_tiers(vec![
            edit(&[], &[1], Placement::Tier, 2),
            edit(&[], &[2], Placement::Tier, 3),
            edit(&[2, 1], &[3, 4], Placement::Tier, 5),
            edit(&[], &[5], Placement::Tier, 6),
            edit(&[5, 3, 4], &[6, 7, 8], Placement::Tier, 9),
        ])
        .unwrap();
        assert_eq!(layout.levels, vec![vec![6, 7, 8]]);
        assert_eq!(layout.next_sst_id, 9);
    }

    // 记录写进文件、再读出来重放，与直接重放的结果相同。
    #[test]
    fn replay_after_recover() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_FILE_NAME);
        let records = vec![
            ManifestRecord::Init(Shape::Levels { max_levels: 3 }),
            edit(&[], &[1], Placement::L0, 2),
            edit(&[], &[2], Placement::L0, 3),
            edit(&[2, 1], &[3], Placement::Level(1), 4),
        ];
        let mut manifest = Manifest::create(&path).unwrap();
        for record in &records {
            manifest.append(record).unwrap();
        }
        drop(manifest);

        let (_, recovered) = Manifest::recover(&path).unwrap();
        assert_eq!(replay(&recovered).unwrap(), replay(&records).unwrap());
        assert_eq!(
            replay(&recovered).unwrap().levels,
            vec![vec![3], vec![], vec![]]
        );
    }

    /// 一条 flush 的 Edit 记录：把编号为 `flushed` 的 memtable 写成了 `added`。
    fn flush_edit(added: &[usize], flushed: usize, next: usize) -> ManifestRecord {
        ManifestRecord::Edit(VersionEdit {
            removed: vec![],
            added: added.to_vec(),
            placement: Placement::L0,
            next_sst_id: next,
            flushed_memtable_id: Some(flushed),
        })
    }

    // L14 之前写的记录没有 flushed_memtable_id：照样解得出来，这一项是 None。
    #[test]
    fn old_record_without_flushed_memtable_id_decodes() {
        let json = r#"{"Edit":{"removed":[],"added":[1],"placement":"L0","next_sst_id":2}}"#;

        let record: ManifestRecord = serde_json::from_str(json).unwrap();

        assert_eq!(record, edit(&[], &[1], Placement::L0, 2));
    }

    // 高水位取最后一条 flush 记录的；合并的记录（None）不改变它；没有 flush 过时是 None。
    #[test]
    fn replay_keeps_the_latest_flushed_memtable_id() {
        let none = replay_levels(vec![edit(&[], &[1], Placement::L0, 2)]).unwrap();
        assert_eq!(none.flushed_memtable_id, None);

        let layout = replay_levels(vec![
            flush_edit(&[2], 1, 4),
            flush_edit(&[4], 3, 6),
            edit(&[4, 2], &[6], Placement::Level(1), 7),
            flush_edit(&[], 5, 8),
        ])
        .unwrap();

        assert_eq!(layout.flushed_memtable_id, Some(5));
        assert_eq!(layout.l0, Vec::<usize>::new());
        assert_eq!(layout.levels[0], vec![6]);
    }

    // 空表的 flush：没有加上文件，只把高水位往上推；tiered 时也不留下空的 tier。
    #[test]
    fn replay_empty_flush_only_moves_the_high_water() {
        let levels = replay_levels(vec![flush_edit(&[], 1, 2)]).unwrap();
        assert!(levels.l0.is_empty());
        assert_eq!(levels.flushed_memtable_id, Some(1));

        let empty_tier_flush = ManifestRecord::Edit(VersionEdit {
            removed: vec![],
            added: vec![],
            placement: Placement::Tier,
            next_sst_id: 2,
            flushed_memtable_id: Some(1),
        });
        let tiers = replay_tiers(vec![empty_tier_flush]).unwrap();
        assert!(tiers.levels.is_empty());
        assert_eq!(tiers.flushed_memtable_id, Some(1));
    }

    // 高水位倒退或者不变：flush 的顺序被打乱，是 Corruption。
    #[test]
    fn replay_rejects_a_flushed_memtable_id_going_back() {
        assert_corruption(replay_levels(vec![
            flush_edit(&[2], 3, 4),
            flush_edit(&[4], 1, 5),
        ]));
        assert_corruption(replay_levels(vec![
            flush_edit(&[2], 3, 4),
            flush_edit(&[4], 3, 5),
        ]));
    }
}
