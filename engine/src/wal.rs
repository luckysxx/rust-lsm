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
//! 内容： 第 0 对 │ 第 1 对 │ …          （一批写入，至少一对）
//! 一对： key 的长度（2 字节）│ key │ value 的长度（2 字节）│ value
//! ```
//!
//! value 为空是墓碑。一批写入是一条记录：外面一层与 manifest 的帧相同，只管"这一段完整、
//! 没被改过"，残尾的处理也相同，所以崩溃之后一批要么全在、要么全不在。里面一层只管
//! key 与 value 怎么排；内容里不记"有几对"，外面一层的长度已经划出了一批的边界，
//! 一对一对解到正好用完为止。只有一对的批次与 L14 的单条记录字节完全相同，旧文件照样能读。

use bytes::{BufMut, Bytes};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crate::block::{MAX_KEY_LEN, MAX_VALUE_LEN};
use crate::error::{Error, Result};
use crate::manifest::sync_parent_dir;

/// 一条记录的头部：内容的长度，4 字节。
const LEN_SIZE: usize = 4;
/// 一条记录的尾部：内容的校验和，4 字节。
const CHECKSUM_SIZE: usize = 4;
/// 内容里 key 的长度与 value 的长度各占 2 字节。
const ENTRY_LEN_SIZE: usize = 2;

/// 从 WAL 里解出的一批写入：按写入顺序的 key 与 value，value 为空是墓碑。
pub type WalBatch = Vec<(Bytes, Bytes)>;

/// 打开着的 WAL 文件，只往末尾追加。
///
/// 追加由调用方排队（[`crate::mem_table::MemTable::put_batch`] 的锁，它同时保证 WAL 里的
/// 顺序与插入表的顺序一致）。同步自己排队，可以与追加同时进行。
///
/// 并发的同步共用一次 fsync（group commit）：fsync 刷的是整个文件，一次 fsync 开始之前
/// 已经追加的记录都会被它带下去。每个追加的人记下自己那条记录的末尾（[`Wal::append`] 的
/// 返回值），同步时先看已经落盘的位置有没有越过它，越过了就不必再 fsync。
///
/// fsync 在锁外做：锁只保护 [`SyncState`] 的两个数，拿着它的时间只有一瞬间。正在等的线程
/// 睡在 `sync_done` 上，fsync 做完叫醒全部，各自回去看自己的记录落盘了没有。
pub struct Wal {
    file: File,
    /// 已经追加了多少字节：每次追加完加上这条记录的长度。只有追加的人改它，
    /// 追加由调用方排队，所以它增长的先后就是记录在文件里的先后。
    appended: AtomicU64,
    /// 同步的进度。只在读写这两个数时拿锁，fsync 期间不拿。
    sync_state: Mutex<SyncState>,
    /// 一次 fsync 做完（成功或失败）时叫醒等着的线程。配对的锁是 `sync_state`。
    sync_done: Condvar,
    /// 只在测试里用：为真时每次追加之后停一小段长短不一的时间，把"追加完、还没插入表"
    /// 这段间隙拉长，让并发写入的交错容易出现。
    #[cfg(test)]
    slow_appends: std::sync::atomic::AtomicBool,
    /// 只在测试里用：为真时每次 fsync 之前停 [`SLOW_SYNC`]，让别的线程在这段时间里追加、排队。
    #[cfg(test)]
    slow_syncs: std::sync::atomic::AtomicBool,
    /// 只在测试里用：开始过几次 fsync。
    #[cfg(test)]
    fsyncs_started: std::sync::atomic::AtomicUsize,
    /// 只在测试里用：`Some(n)` 时 fsync 要拿到许可才做，还剩 n 个许可，用完就停在那里等；
    /// `None` 时不拦。测试用它把一次 fsync 停在半路，确定地造出"有人正在 fsync"的时刻。
    #[cfg(test)]
    fsync_permits: (Mutex<Option<usize>>, Condvar),
}

/// 同步的进度，由 [`Wal`] 的 `sync_state` 锁保护。
struct SyncState {
    /// 已经确认落盘到第几个字节：`[0, synced)` 之内的记录断电也不会丢。只增不减。
    synced: u64,
    /// 有没有线程正在 fsync。同一时刻最多一个：看到它为真的线程去 `sync_done` 上等。
    syncing: bool,
}

/// 测试里一次"慢 fsync"停多久。
#[cfg(test)]
const SLOW_SYNC: std::time::Duration = std::time::Duration::from_millis(20);

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
        Ok(Self::from_file(file, 0))
    }

    /// 打开已有的 WAL，按写入的顺序读出全部批次，每批里的 key 与 value 也按写入的顺序。
    ///
    /// 末尾不完整的一条（崩溃时只写了一半）从文件里截掉并同步，之后的追加接在最后一条
    /// 完整的记录后面。中间某一条损坏时返回 [`Error::Corruption`]。
    ///
    /// 读出的记录可能只在页缓存里（上次进程崩溃时还没同步），所以不算已落盘：
    /// 之后第一次同步照样 fsync。
    pub fn recover(path: &Path) -> Result<(Self, Vec<WalBatch>)> {
        let mut file = OpenOptions::new().read(true).append(true).open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;

        let (batches, valid_len) = decode_batches(&buf)?;
        if valid_len < buf.len() {
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }
        Ok((Self::from_file(file, valid_len as u64), batches))
    }

    /// 把一批写入作为一条记录追加在末尾，返回这条记录的末尾在第几个字节，交给
    /// [`Wal::sync_to`]。只写进操作系统的页缓存、不同步：返回之后进程崩溃不会丢，
    /// 断电可能丢。
    ///
    /// 整条记录用一次 `write_all` 写出，不先放进进程里的缓冲区：缓冲区里的数据进程一崩溃就没了。
    /// 批次不能为空，key 与 value 的长度上限见 [`encode_batch`]。同一时刻只能有一个线程追加。
    ///
    /// 写到一半失败时，文件末尾可能留下半条记录，之后再追加，它就成了中间的损坏。
    pub fn append(&self, batch: &[(&[u8], &[u8])]) -> Result<u64> {
        let frame = encode_batch(batch);
        // `&File` 也实现了 `Write`：写文件不需要 `&mut`，由操作系统维护文件的位置。
        let mut file = &self.file;
        file.write_all(&frame)?;
        // write 返回之后才加：别的线程读到新的长度时，这条记录一定已经在页缓存里。
        let start = self
            .appended
            .fetch_add(frame.len() as u64, Ordering::SeqCst);
        let end = start + frame.len() as u64;
        #[cfg(test)]
        if self.slow_appends.load(Ordering::Relaxed) {
            let pause_micros = crc32fast::hash(&frame) % 50;
            std::thread::sleep(std::time::Duration::from_micros(pause_micros.into()));
        }
        Ok(end)
    }

    /// 让末尾在 `end` 之前（含）的记录都落到磁盘上。返回这次有没有真的做 fsync。
    ///
    /// 已经落盘的位置越过了 `end`（别人那次 fsync 已经把它带下去了）就直接返回；有别人正在
    /// fsync 就睡下等它做完，醒来再看；没人在 fsync 就自己做一次，它会把开始之前已经追加的
    /// 记录（包括别人的）一起带下去。
    ///
    /// fsync 失败时返回错误，已落盘的位置不变；等着的线程被叫醒后会有一个再试一次。
    pub fn sync_to(&self, end: u64) -> Result<bool> {
        let mut state = self.sync_state.lock().unwrap();
        loop {
            if state.synced >= end {
                return Ok(false);
            }
            if !state.syncing {
                break;
            }
            // 放开锁并睡下，被叫醒后重新拿到锁才返回；醒来时状态可能又变了，所以回到开头再看。
            state = self.sync_done.wait(state).unwrap();
        }
        state.syncing = true;
        // 必须在 fsync 之前读：fsync 进行期间才追加的记录不一定被它带下去。
        let target = self.appended.load(Ordering::SeqCst);
        // fsync 期间不拿锁：别人要能随时进来看"自己的记录落盘了没有"。
        drop(state);

        let result = self.fsync();

        let mut state = self.sync_state.lock().unwrap();
        state.syncing = false;
        if result.is_ok() {
            state.synced = target;
        }
        drop(state);
        // 叫醒全部：这一次可能让好几个人同时落盘，而只有他们自己知道各自的末尾。
        self.sync_done.notify_all();
        result.map(|()| true)
    }

    /// 让到目前为止追加的记录都落到磁盘上。返回这次有没有真的做 fsync：
    /// 上次同步之后没有新的追加时不做。
    pub fn sync(&self) -> Result<bool> {
        let end = self.appended.load(Ordering::SeqCst);
        self.sync_to(end)
    }

    /// 真正的 fsync（macOS 上是 `F_FULLFSYNC`）。测试里可以让它变慢、停在半路、数它的次数。
    fn fsync(&self) -> Result<()> {
        #[cfg(test)]
        {
            self.fsyncs_started.fetch_add(1, Ordering::SeqCst);
            if self.slow_syncs.load(Ordering::Relaxed) {
                std::thread::sleep(SLOW_SYNC);
            }
            self.take_fsync_permit();
        }
        self.file.sync_all()?;
        Ok(())
    }

    fn from_file(file: File, len: u64) -> Self {
        Self {
            file,
            appended: AtomicU64::new(len),
            sync_state: Mutex::new(SyncState {
                synced: 0,
                syncing: false,
            }),
            sync_done: Condvar::new(),
            #[cfg(test)]
            slow_appends: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            slow_syncs: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fsyncs_started: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fsync_permits: (Mutex::new(None), Condvar::new()),
        }
    }

    /// 只在测试里用：之后每次追加都停一小段时间，见 `slow_appends`。
    #[cfg(test)]
    pub(crate) fn make_appends_slow(&self) {
        self.slow_appends.store(true, Ordering::Relaxed);
    }

    /// 只在测试里用：之后每次 fsync 之前都停 [`SLOW_SYNC`]。
    #[cfg(test)]
    pub(crate) fn make_syncs_slow(&self) {
        self.slow_syncs.store(true, Ordering::Relaxed);
    }

    /// 只在测试里用：到现在为止开始过几次 fsync。
    #[cfg(test)]
    pub(crate) fn fsyncs_started(&self) -> usize {
        self.fsyncs_started.load(Ordering::SeqCst)
    }

    /// 只在测试里用：已经确认落盘到第几个字节。
    #[cfg(test)]
    fn synced_len(&self) -> u64 {
        self.sync_state.lock().unwrap().synced
    }

    /// 只在测试里用：从现在起每次 fsync 都要先拿到许可，见 `fsync_permits`。一开始没有许可。
    #[cfg(test)]
    fn hold_fsyncs(&self) {
        *self.fsync_permits.0.lock().unwrap() = Some(0);
    }

    /// 只在测试里用：再放行 `count` 次 fsync。
    #[cfg(test)]
    fn allow_fsyncs(&self, count: usize) {
        let (permits, changed) = &self.fsync_permits;
        if let Some(left) = permits.lock().unwrap().as_mut() {
            *left += count;
        }
        changed.notify_all();
    }

    /// 只在测试里用：需要许可时等到有许可，再用掉一个。
    #[cfg(test)]
    fn take_fsync_permit(&self) {
        let (permits, changed) = &self.fsync_permits;
        let mut permits = permits.lock().unwrap();
        while *permits == Some(0) {
            permits = changed.wait(permits).unwrap();
        }
        if let Some(left) = permits.as_mut() {
            *left -= 1;
        }
    }
}

/// 一批写入编码之后在文件里占多少字节，与 [`encode_batch`] 的结果一样长。
pub fn encoded_len(batch: &[(&[u8], &[u8])]) -> usize {
    let body_len: usize = batch
        .iter()
        .map(|(key, value)| 2 * ENTRY_LEN_SIZE + key.len() + value.len())
        .sum();
    LEN_SIZE + body_len + CHECKSUM_SIZE
}

/// 把一批写入编码成文件里的一条记录：长度、内容（一对接一对）、校验和。
///
/// 批次不能为空；每个 key 不能超过 [`MAX_KEY_LEN`]、每个 value 不能超过 [`MAX_VALUE_LEN`]
/// 字节（长度字段只有 2 字节）。违反时 panic：写入的入口先检查，这样的批次到不了这里。
pub fn encode_batch(batch: &[(&[u8], &[u8])]) -> Vec<u8> {
    assert!(!batch.is_empty(), "一批写入至少要有一对 key 与 value");
    let mut body = Vec::new();
    for &(key, value) in batch {
        assert!(key.len() <= MAX_KEY_LEN, "key 超过了 65535 字节");
        assert!(value.len() <= MAX_VALUE_LEN, "value 超过了 65535 字节");
        body.put_u16(key.len() as u16);
        body.put_slice(key);
        body.put_u16(value.len() as u16);
        body.put_slice(value);
    }
    let crc32 = crc32fast::hash(&body);
    let mut frame = Vec::with_capacity(LEN_SIZE + body.len() + CHECKSUM_SIZE);
    frame.put_u32(body.len() as u32);
    frame.extend_from_slice(&body);
    frame.put_u32(crc32);
    frame
}

/// 从文件的全部内容里逐条解出批次，返回解出的结果，以及它们一共占了多少字节
/// （最后一条完整记录的末尾）。
///
/// 外面一层的规则与 manifest 相同：
///
/// - 剩下的字节不够一条完整的记录（长度不够 4 字节，或者长度说的字节数超出了剩下的）：
///   是只写了一半的残尾，停下，不算错误。
/// - 一条完整记录的校验和对不上：它是文件的最后一条时也当作残尾；后面还有记录时说明文件坏了，
///   返回 [`Error::Corruption`]。
/// - 校验和对得上，内容却解不出一批 key 与 value（见 [`decode_body`]）：返回
///   [`Error::Corruption`]。
pub fn decode_batches(buf: &[u8]) -> Result<(Vec<WalBatch>, usize)> {
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
            Some(batch) => {
                wal_records.push(batch);
            }
            None => {
                return Err(Error::Corruption(format!(
                    "WAL 第 {} 条记录（第 {offset} 字节起）的内容解不出一批 key 与 value",
                    wal_records.len()
                )));
            }
        }
        offset += frame_len;
    }
    Ok((wal_records, offset))
}

/// 解出一条记录的内容：一对接一对的 key 的长度、key、value 的长度、value，直到正好用完。
///
/// 内容为空，或者最后一对的字节不够（包括解完前面几对之后只剩下零星几个字节）时返回 `None`。
pub fn decode_body(body: &[u8]) -> Option<WalBatch> {
    let body_len = body.len();
    let mut batch = Vec::new();
    let mut offset = 0;
    while offset < body_len {
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
        batch.push((Bytes::copy_from_slice(key), Bytes::copy_from_slice(value)));
    }
    if batch.is_empty() {
        return None;
    }
    Some(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把解出的批次变成字符串，方便比较：每批一个列表。
    fn as_strings(batches: &[WalBatch]) -> Vec<Vec<(String, String)>> {
        batches
            .iter()
            .map(|batch| {
                batch
                    .iter()
                    .map(|(key, value)| {
                        let key = String::from_utf8(key.to_vec()).unwrap();
                        let value = String::from_utf8(value.to_vec()).unwrap();
                        (key, value)
                    })
                    .collect()
            })
            .collect()
    }

    /// 把字符串写的批次变成期望的结果。
    fn expected(batches: &[&[(&str, &str)]]) -> Vec<Vec<(String, String)>> {
        batches
            .iter()
            .map(|batch| {
                batch
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect()
            })
            .collect()
    }

    /// 三条记录首尾相接：一对的批次、两对的批次（其中一条墓碑）、再一对的批次。
    /// 返回全部字节与每条的末尾位置。
    fn three_frames() -> (Vec<u8>, Vec<usize>) {
        let mut buf = Vec::new();
        let mut ends = Vec::new();
        let batches: [&[(&[u8], &[u8])]; 3] = [
            &[(b"apple", b"red")],
            &[(b"banana", b""), (b"cherry", b"dark red")],
            &[(b"date", b"brown")],
        ];
        for batch in batches {
            buf.extend_from_slice(&encode_batch(batch));
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

    // 只有一对的批次与 L14 的单条记录字节完全相同：长度、key 的长度、key、value 的长度、value、校验和。
    #[test]
    fn single_pair_batch_matches_the_old_format() {
        let frame = encode_batch(&[(b"ab", b"xyz")]);

        let body = [0, 2, b'a', b'b', 0, 3, b'x', b'y', b'z'];
        let mut expected = vec![0, 0, 0, 9];
        expected.extend_from_slice(&body);
        expected.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
        assert_eq!(frame, expected);
    }

    // 几对的批次：内容里一对接一对，没有"有几对"，校验和覆盖整个内容。
    #[test]
    fn encode_batch_puts_pairs_one_after_another() {
        let frame = encode_batch(&[(b"ab", b"x"), (b"c", b"")]);

        let body = [0, 2, b'a', b'b', 0, 1, b'x', 0, 1, b'c', 0, 0];
        let mut expected = vec![0, 0, 0, 12];
        expected.extend_from_slice(&body);
        expected.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
        assert_eq!(frame, expected);
    }

    // 算出的长度与真正编码出来的一样。
    #[test]
    fn encoded_len_matches_encode_batch() {
        let batches: [&[(&[u8], &[u8])]; 3] = [
            &[(b"k", b"")],
            &[(b"apple", b"red")],
            &[(b"ab", b"xyz"), (b"k", b""), (b"cherry", b"dark red")],
        ];
        for batch in batches {
            assert_eq!(encoded_len(batch), encode_batch(batch).len());
        }
    }

    // 墓碑的 value 长度是 0，后面没有 value 的字节。
    #[test]
    fn encode_batch_writes_tombstone_with_zero_value_len() {
        let frame = encode_batch(&[(b"k", b"")]);

        assert_eq!(&frame[..LEN_SIZE], &[0, 0, 0, 5]);
        assert_eq!(&frame[LEN_SIZE..LEN_SIZE + 5], &[0, 1, b'k', 0, 0]);
        assert_eq!(frame.len(), LEN_SIZE + 5 + CHECKSUM_SIZE);
    }

    // 长度到上限的 key 与 value 能写也能读回来，放在批次的中间也一样。
    #[test]
    fn encode_and_decode_longest_key_and_value() {
        let key = vec![b'k'; MAX_KEY_LEN];
        let value = vec![b'v'; MAX_VALUE_LEN];
        let frame = encode_batch(&[(b"a", b"1"), (&key, &value), (b"z", b"")]);

        let (batches, valid_len) = decode_batches(&frame).unwrap();

        assert_eq!(valid_len, frame.len());
        let expected_batch = vec![
            (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
            (Bytes::from(key), Bytes::from(value)),
            (Bytes::from_static(b"z"), Bytes::new()),
        ];
        assert_eq!(batches, vec![expected_batch]);
    }

    // 超长的 key 在编码时 panic，即使它不是批次的第一对。
    #[test]
    #[should_panic(expected = "key 超过了 65535 字节")]
    fn encode_batch_panics_on_too_long_key() {
        let key = vec![b'k'; MAX_KEY_LEN + 1];
        encode_batch(&[(b"a", b"1"), (&key, b"v")]);
    }

    // 空批次在编码时 panic：写出来的空内容读回时会被当成损坏。
    #[test]
    #[should_panic(expected = "至少要有一对")]
    fn encode_batch_panics_on_empty_batch() {
        encode_batch(&[]);
    }

    // 几条记录按写入的顺序解出来，每条是一批，批次的边界不变；墓碑解出空的 value，占用的字节是全部。
    #[test]
    fn decode_batches_keeps_batch_boundaries() {
        let (buf, _) = three_frames();

        let (batches, valid_len) = decode_batches(&buf).unwrap();

        assert_eq!(valid_len, buf.len());
        let expected_batches: &[&[(&str, &str)]] = &[
            &[("apple", "red")],
            &[("banana", ""), ("cherry", "dark red")],
            &[("date", "brown")],
        ];
        assert_eq!(as_strings(&batches), expected(expected_batches));
    }

    // 空文件解出零批，占用 0 字节。
    #[test]
    fn decode_batches_on_empty_buffer() {
        let (batches, valid_len) = decode_batches(&[]).unwrap();

        assert!(batches.is_empty());
        assert_eq!(valid_len, 0);
    }

    // 从任何一个位置截断，都只解出截断之前完整的那几批：两对的批次要么两对都在，要么都不在。
    #[test]
    fn decode_batches_stops_at_incomplete_tail_at_every_cut() {
        let (buf, ends) = three_frames();
        let (all, _) = decode_batches(&buf).unwrap();

        for cut in 0..buf.len() {
            let (batches, valid_len) = decode_batches(&buf[..cut]).unwrap();

            let complete = ends.iter().filter(|&&end| end <= cut).count();
            let expected_len = if complete == 0 { 0 } else { ends[complete - 1] };
            assert_eq!(batches, all[..complete], "截在第 {cut} 字节");
            assert_eq!(valid_len, expected_len, "截在第 {cut} 字节");
        }
    }

    // 长度字段是一个很大的数：当作残尾停下，不越界、不 panic。
    #[test]
    fn decode_batches_treats_huge_length_as_tail() {
        let mut buf = encode_batch(&[(b"k", b"v")]);
        let first_end = buf.len();
        buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 1, 2, 3]);

        let (batches, valid_len) = decode_batches(&buf).unwrap();

        assert_eq!(batches.len(), 1);
        assert_eq!(valid_len, first_end);
    }

    // 最后一条的校验和对不上：当作写到一半的残尾，前面的照常解出。
    #[test]
    fn decode_batches_treats_bad_last_checksum_as_tail() {
        let (mut buf, ends) = three_frames();
        // 改坏第 3 条内容里的一个字节。
        buf[ends[1] + LEN_SIZE + 3] ^= 0xff;

        let (batches, valid_len) = decode_batches(&buf).unwrap();

        assert_eq!(batches.len(), 2);
        assert_eq!(valid_len, ends[1]);
    }

    // 中间一条的校验和对不上：文件坏了，返回 Corruption。
    #[test]
    fn decode_batches_rejects_bad_checksum_in_the_middle() {
        let (mut buf, ends) = three_frames();
        // 改坏第 2 条内容里的一个字节。
        buf[ends[0] + LEN_SIZE + 3] ^= 0xff;

        let result = decode_batches(&buf);

        assert!(matches!(result, Err(Error::Corruption(_))), "{result:?}");
    }

    // 校验和对得上、内容解不出来：即使是最后一条也是 Corruption，不当作残尾。
    #[test]
    fn decode_batches_rejects_malformed_body_even_at_the_end() {
        let mut buf = encode_batch(&[(b"k", b"v")]);
        // 第一对完整，第二对的 key 的长度说有 5 字节，实际只跟了 1 个字节。
        buf.extend_from_slice(&frame_with_body(&[0, 1, b'a', 0, 1, b'x', 0, 5, b'b']));

        let result = decode_batches(&buf);

        assert!(matches!(result, Err(Error::Corruption(_))), "{result:?}");
    }

    // 正常的内容解出一批：一对、墓碑、几对首尾相接都按顺序解出。
    #[test]
    fn decode_body_reads_pairs_in_order() {
        let body = [0, 2, b'a', b'b', 0, 3, b'x', b'y', b'z'];
        assert_eq!(
            decode_body(&body),
            Some(vec![(
                Bytes::from_static(b"ab"),
                Bytes::from_static(b"xyz")
            )])
        );

        let tombstone = [0, 1, b'k', 0, 0];
        assert_eq!(
            decode_body(&tombstone),
            Some(vec![(Bytes::from_static(b"k"), Bytes::new())])
        );

        let three = [
            0, 1, b'a', 0, 1, b'1', 0, 1, b'b', 0, 0, 0, 1, b'a', 0, 1, b'2',
        ];
        assert_eq!(
            decode_body(&three),
            Some(vec![
                (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
                (Bytes::from_static(b"b"), Bytes::new()),
                (Bytes::from_static(b"a"), Bytes::from_static(b"2")),
            ])
        );
    }

    // 内容为空、字节不够或者有多余：每一种都返回 None，不越界。
    #[test]
    fn decode_body_rejects_wrong_lengths() {
        let cases: [&[u8]; 9] = [
            // 空的内容不是一批。
            &[],
            // 连 key 的长度都不够。
            &[0],
            // key 不够长。
            &[0, 3, b'a', b'b'],
            // key 完整，value 的长度缺 1 个字节。
            &[0, 1, b'a', 0],
            // value 不够长。
            &[0, 1, b'a', 0, 2, b'x'],
            // 一对之后多出 1 个字节：凑不成第二对。
            &[0, 1, b'a', 0, 1, b'x', b'!'],
            // 第二对的 key 不够长。
            &[0, 1, b'a', 0, 1, b'x', 0, 2, b'b'],
            // 第二对的 value 的长度缺 1 个字节。
            &[0, 1, b'a', 0, 1, b'x', 0, 1, b'b', 0],
            // 第二对的 value 不够长。
            &[0, 1, b'a', 0, 1, b'x', 0, 1, b'b', 0, 3, b'y'],
        ];
        for body in cases {
            assert_eq!(decode_body(body), None, "{body:?}");
        }
    }

    // 新建、追加几批、同步，之后打开能按顺序读回来，批次的边界不变。
    #[test]
    fn wal_recover_reads_back_appended_batches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(&[(b"a", b"1")]).unwrap();
        wal.append(&[(b"b", b""), (b"c", b"3")]).unwrap();
        wal.append(&[(b"a", b"2")]).unwrap();
        wal.sync().unwrap();
        drop(wal);

        let (_wal, batches) = Wal::recover(&path).unwrap();

        let expected_batches: &[&[(&str, &str)]] =
            &[&[("a", "1")], &[("b", ""), ("c", "3")], &[("a", "2")]];
        assert_eq!(as_strings(&batches), expected(expected_batches));
    }

    // 文件已经存在时 create 返回错误，不覆盖原有的内容。
    #[test]
    fn wal_create_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(&[(b"a", b"1")]).unwrap();
        drop(wal);

        assert!(Wal::create(&path).is_err());

        let (_wal, batches) = Wal::recover(&path).unwrap();
        assert_eq!(batches.len(), 1);
    }

    // 打开时截掉写了一半的残尾（一整批都不要）；之后的追加接在最后一条完整记录后面，再打开能读到。
    #[test]
    fn wal_recover_truncates_tail_and_appends_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(&[(b"a", b"1")]).unwrap();
        wal.append(&[(b"b", b"2")]).unwrap();
        drop(wal);
        let complete_len = std::fs::metadata(&path).unwrap().len();
        // 模拟崩溃：两对的第三批只写进去一半（第一对已经完整写进去了）。
        let half = encode_batch(&[(b"c", b"3"), (b"cc", b"33")]);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&half[..half.len() - 4]).unwrap();
        drop(file);

        let (wal, batches) = Wal::recover(&path).unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), complete_len);
        wal.append(&[(b"d", b"4")]).unwrap();
        drop(wal);

        let (_wal, batches) = Wal::recover(&path).unwrap();
        let expected_batches: &[&[(&str, &str)]] = &[&[("a", "1")], &[("b", "2")], &[("d", "4")]];
        assert_eq!(as_strings(&batches), expected(expected_batches));
    }

    // 中间一条损坏时打开返回 Corruption，文件不被截短。
    #[test]
    fn wal_recover_rejects_corruption_in_the_middle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(&[(b"a", b"1")]).unwrap();
        wal.append(&[(b"b", b"2")]).unwrap();
        drop(wal);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[LEN_SIZE + 2] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let result = Wal::recover(&path);

        assert!(matches!(result, Err(Error::Corruption(_))));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    // ---------- 同步 ----------

    // 每次追加返回这条记录的末尾，就是追加之后文件的长度；打开已有的文件时接着它的长度往后数。
    #[test]
    fn append_returns_end_of_each_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();

        let first = wal.append(&[(b"a", b"1")]).unwrap();
        assert_eq!(first, std::fs::metadata(&path).unwrap().len());
        let second = wal.append(&[(b"b", b"2"), (b"c", b"")]).unwrap();
        assert_eq!(second, std::fs::metadata(&path).unwrap().len());
        drop(wal);

        let (wal, _) = Wal::recover(&path).unwrap();
        let third = wal.append(&[(b"d", b"4")]).unwrap();
        assert_eq!(third, std::fs::metadata(&path).unwrap().len());
        // 第三条与第一条一样长。
        assert_eq!(third - second, first);
    }

    // 一次 fsync 把它开始之前追加的记录全部带下去：之后同步到其中任何一条都不再 fsync。
    #[test]
    fn sync_to_skips_records_already_synced() {
        let dir = tempfile::tempdir().unwrap();
        let wal = Wal::create(&dir.path().join("00001.wal")).unwrap();
        let first = wal.append(&[(b"a", b"1")]).unwrap();
        let second = wal.append(&[(b"b", b"2")]).unwrap();
        let third = wal.append(&[(b"c", b"3")]).unwrap();

        assert!(wal.sync_to(first).unwrap());
        assert_eq!(wal.synced_len(), third);
        assert!(!wal.sync_to(second).unwrap());
        assert!(!wal.sync_to(third).unwrap());
        assert_eq!(wal.fsyncs_started(), 1);

        let fourth = wal.append(&[(b"d", b"4")]).unwrap();
        assert!(wal.sync_to(fourth).unwrap());
        assert_eq!(wal.fsyncs_started(), 2);
    }

    // 上次同步之后没有新的追加时 sync 不做 fsync；空文件也不做。
    #[test]
    fn sync_without_new_appends_does_not_fsync() {
        let dir = tempfile::tempdir().unwrap();
        let wal = Wal::create(&dir.path().join("00001.wal")).unwrap();

        assert!(!wal.sync().unwrap());
        wal.append(&[(b"a", b"1")]).unwrap();
        assert!(wal.sync().unwrap());
        assert!(!wal.sync().unwrap());
        assert_eq!(wal.fsyncs_started(), 1);
    }

    // 打开已有的文件：读出的记录可能还没落盘，第一次同步照样 fsync。
    #[test]
    fn recovered_wal_is_not_assumed_synced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("00001.wal");
        let wal = Wal::create(&path).unwrap();
        wal.append(&[(b"a", b"1")]).unwrap();
        drop(wal);

        let (wal, _) = Wal::recover(&path).unwrap();

        assert!(wal.sync().unwrap());
        assert_eq!(wal.synced_len(), std::fs::metadata(&path).unwrap().len());
    }

    // fsync 进行期间追加的记录不算已落盘：要落盘的位置在 fsync 之前读，
    // 这条记录之后还要再 fsync 一次。
    #[test]
    fn records_appended_during_fsync_need_another_fsync() {
        crate::test_util::run_with_slow_deadline("fsync 期间追加", || {
            let dir = tempfile::tempdir().unwrap();
            let wal = Wal::create(&dir.path().join("00001.wal")).unwrap();
            wal.make_syncs_slow();
            let first = wal.append(&[(b"a", b"1")]).unwrap();

            let second = std::thread::scope(|scope| {
                let syncer = scope.spawn(|| wal.sync_to(first).unwrap());
                // 等那次 fsync 开始（之后它还要停一会儿），再追加第二条。
                while wal.fsyncs_started() == 0 {
                    std::thread::yield_now();
                }
                let second = wal.append(&[(b"b", b"2")]).unwrap();
                assert!(syncer.join().unwrap());
                second
            });

            assert_eq!(wal.synced_len(), first);
            assert!(wal.sync_to(second).unwrap());
            assert_eq!(wal.fsyncs_started(), 2);
        });
    }

    // 已经被带下去的线程不会被下一次 fsync 挡住。A 的 fsync 带下去了 B 的记录；C 的记录是
    // A 开始 fsync 之后才追加的，A 做完之后 C 接着做下一次 fsync，停在半路不让它结束。
    // 这时 B 也要马上返回，而不是等到 C 做完。C 比 B 先开始等：拿着锁做 fsync 的写法里，
    // 先等的 C 先拿到锁，B 就被挡住了。
    #[test]
    fn synced_waiter_is_not_held_up_by_the_next_fsync() {
        crate::test_util::run_with_slow_deadline(
            "已落盘的不被下一次 fsync 挡住",
            || {
                for round in 0..3 {
                    let dir = tempfile::tempdir().unwrap();
                    let wal = Wal::create(&dir.path().join("00001.wal")).unwrap();
                    let wal = &wal;
                    wal.hold_fsyncs();
                    let first = wal.append(&[(b"a", b"1")]).unwrap();
                    let second = wal.append(&[(b"b", b"2")]).unwrap();

                    let b_returned_in_time = std::thread::scope(|scope| {
                        // A：同步到第一条，两条都会被它带下去；fsync 停在半路。
                        let a = scope.spawn(|| wal.sync_to(first).unwrap());
                        while wal.fsyncs_started() == 0 {
                            std::thread::yield_now();
                        }
                        // C：第三条在 A 的 fsync 开始之后才追加，要等下一次 fsync。
                        let third = wal.append(&[(b"c", b"3")]).unwrap();
                        let c = scope.spawn(move || wal.sync_to(third).unwrap());
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        // B：第二条已经在 A 的 fsync 里，等 A 做完就该返回。
                        let (b_done, b_done_receiver) = std::sync::mpsc::channel();
                        let b = scope.spawn(move || {
                            let did_fsync = wal.sync_to(second).unwrap();
                            b_done.send(()).unwrap();
                            did_fsync
                        });
                        std::thread::sleep(std::time::Duration::from_millis(20));

                        // 只放行 A 的 fsync；C 接着开始的那一次停在半路。
                        wal.allow_fsyncs(1);
                        let b_returned_in_time = b_done_receiver
                            .recv_timeout(std::time::Duration::from_secs(2))
                            .is_ok();
                        // 再放行 C 的 fsync，让每个线程都能结束。
                        wal.allow_fsyncs(1);
                        assert!(a.join().unwrap());
                        assert!(
                            !b.join().unwrap(),
                            "B 的记录已经被 A 带下去，不该自己再 fsync"
                        );
                        assert!(c.join().unwrap());
                        b_returned_in_time
                    });

                    assert!(
                        b_returned_in_time,
                        "第 {round} 轮：B 的记录已经落盘，却被下一次 fsync 挡住了"
                    );
                    assert_eq!(wal.fsyncs_started(), 2);
                    assert_eq!(
                        wal.synced_len(),
                        std::fs::metadata(dir.path().join("00001.wal"))
                            .unwrap()
                            .len()
                    );
                }
            },
        );
    }

    // 几个线程不停地追加再同步到自己的记录，fsync 很慢：每个人返回时自己的记录都已落盘，
    // fsync 的次数远少于同步的次数。
    #[test]
    fn concurrent_syncs_share_fsyncs() {
        crate::test_util::run_with_slow_deadline("并发同步", || {
            let dir = tempfile::tempdir().unwrap();
            let wal = Wal::create(&dir.path().join("00001.wal")).unwrap();
            wal.make_syncs_slow();
            let threads = 8;
            let rounds = 10;
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        for _ in 0..rounds {
                            let end = wal.append(&[(b"k", b"v")]).unwrap();
                            wal.sync_to(end).unwrap();
                            assert!(wal.synced_len() >= end);
                        }
                    });
                }
            });

            let syncs = threads * rounds;
            let fsyncs = wal.fsyncs_started();
            assert!(fsyncs * 2 < syncs, "{syncs} 次同步做了 {fsyncs} 次 fsync");
            assert!(!wal.sync().unwrap());
        });
    }
}
