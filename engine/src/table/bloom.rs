//! Bloom filter：每个 SST 一份，回答"某个 key 是不是一定不在这个文件里"。
//!
//! 一个位数组，开始时全是 0。插入一个 key 时算出它的 k 个位置并置 1；查询时看同样的
//! k 个位置，有一位是 0 就一定没插入过，全是 1 就"可能插入过"。位只会被置 1，
//! 所以插入过的 key 一定回答"可能有"：**不会漏报，只会误报**。
//!
//! k 个位置只用一次哈希得到：第 0 个位置是 `h`，之后每次加上步长 `delta`
//! （`h` 循环左移 15 位），都对总位数取余。加法按 32 位回绕。
//!
//! 第 i 位在第 `i / 8` 个字节里，是这个字节从低往高数的第 `i % 8` 位。
//!
//! 编码后的样子：
//!
//! ```text
//! ┌──────────────────────────┬──────┐
//! │ 位数组                    │  k   │
//! └──────────────────────────┴──────┘
//!   总位数 / 8 个字节           1 字节
//! ```
//!
//! 位数组和查询方法都存在文件里，所以 [`key_hash`] 的算法不能改：改了之后，
//! 旧文件里存在的 key 会算出不同的位置，被错答成"一定没有"。

use bytes::{BufMut, Bytes};

use crate::error::{Error, Result};

/// k 的上限。每个 key 分到的位数再多，也最多置这么多位。
const MAX_K: u8 = 30;

/// 位数组至少这么多位，避免 key 很少时数组太小、几乎全是 1。
const MIN_BITS: usize = 64;

/// 一个做好的 Bloom filter。创建之后不再修改。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom {
    /// 位数组，总位数是字节数乘 8。
    bits: Bytes,
    /// 每个 key 置几位，查询时就看几位。
    k: u8,
}

/// 一个 key 的哈希值。写 SST 和查询都必须用它，算法固定为 XXH3 的低 32 位。
pub fn key_hash(key: &[u8]) -> u32 {
    xxhash_rust::xxh3::xxh3_64(key) as u32
}

impl Bloom {
    /// 要达到误判率 `false_positive_rate`，每个 key 大约要分到几位。
    ///
    /// 公式是 `-ln(p) / (ln 2)²`，向上取整。误判率 0.01 时是 10 位。
    pub fn bits_per_key(false_positive_rate: f64) -> usize {
        let ln2 = std::f64::consts::LN_2;
        let bits = -false_positive_rate.ln() / (ln2 * ln2);
        bits.ceil() as usize
    }

    /// 用一个文件里全部 key 的哈希值做出 filter，每个 key 分到 `bits_per_key` 位。
    ///
    /// - 总位数是 `key 个数 × bits_per_key`，至少 64 位，再向上补齐到 8 的倍数。
    /// - k 是 `bits_per_key × 0.69` 取整（0.69 约等于 ln 2），限制在 1 到 30 之间。
    /// - 每个哈希值置 k 位，位置的算法见模块的说明。
    pub fn build(key_hashes: &[u32], bits_per_key: usize) -> Self {
        // k：浮点数转整数时直接截掉小数，再限制在 1 到 30 之间。
        let k = (bits_per_key as f64 * 0.69) as u8;
        let k = k.clamp(1, MAX_K);

        // 总位数：先按 key 的个数算，不足 64 位按 64 位，再补齐到整字节。
        let wanted_bits = (key_hashes.len() * bits_per_key).max(MIN_BITS);
        let byte_count = wanted_bits.div_ceil(8);
        let bit_count = byte_count * 8;

        let mut bits = vec![0u8; byte_count];
        for &key_hash in key_hashes {
            for position in bit_positions(key_hash, k, bit_count) {
                let byte_index = position / 8;
                let bit_in_byte = position % 8;
                bits[byte_index] |= 1 << bit_in_byte;
            }
        }

        Self {
            bits: Bytes::from(bits),
            k,
        }
    }

    /// 哈希值为 `key_hash` 的 key 可能在不在里面。
    ///
    /// 返回 `false` 时一定没插入过；返回 `true` 时可能插入过，也可能是误报。
    pub fn may_contain(&self, key_hash: u32) -> bool {
        let bit_count = self.bit_count();
        for position in bit_positions(key_hash, self.k, bit_count) {
            let byte_index = position / 8;
            let bit_in_byte = position % 8;
            if self.bits[byte_index] & (1 << bit_in_byte) == 0 {
                return false;
            }
        }
        true
    }

    /// 编码后有多少字节。
    pub fn encoded_len(&self) -> usize {
        self.bits.len() + 1
    }

    /// 把 filter 编码后接到 `buf` 的末尾：位数组，然后是 1 字节的 k。
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.put_slice(&self.bits);
        buf.put_u8(self.k);
    }

    /// 把 [`Bloom::encode`] 写出的字节还原成 filter。
    ///
    /// 位数组为空，或者 k 不在 1 到 30 之间时返回 [`Error::Corruption`]。
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let Some((&k, bits)) = buf.split_last() else {
            return Err(corrupt_bloom("没有任何字节"));
        };
        if bits.is_empty() {
            return Err(corrupt_bloom("位数组为空"));
        }
        if k == 0 || k > MAX_K {
            return Err(corrupt_bloom("k 不在 1 到 30 之间"));
        }
        Ok(Self {
            bits: Bytes::copy_from_slice(bits),
            k,
        })
    }

    /// 每个 key 置几位。
    pub fn k(&self) -> u8 {
        self.k
    }

    /// 位数组的总位数。
    pub fn bit_count(&self) -> usize {
        self.bits.len() * 8
    }
}

/// 哈希值为 `key_hash` 的 key 在 `bit_count` 位的数组里对应的 k 个位置，依次给出。
///
/// 第 i 个位置是 `key_hash + i × delta` 对总位数取余，`delta` 是 `key_hash` 循环左移
/// 15 位，乘法和加法都按 32 位回绕。构建与查询都只通过这个函数求位置，保证两边一致。
fn bit_positions(key_hash: u32, k: u8, bit_count: usize) -> impl Iterator<Item = usize> {
    let delta = key_hash.rotate_left(15);
    (0..k).map(move |i| {
        let step = delta.wrapping_mul(i as u32);
        let h = key_hash.wrapping_add(step);
        h as usize % bit_count
    })
}

/// 造一个"filter 损坏"的错误。
fn corrupt_bloom(message: &str) -> Error {
    Error::Corruption(format!("Bloom filter：{message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 第 0 到 count-1 个 key 的哈希值，key 形如 `key-00000`。
    fn hashes_of_keys(prefix: &str, count: usize) -> Vec<u32> {
        (0..count)
            .map(|i| key_hash(format!("{prefix}-{i:05}").as_bytes()))
            .collect()
    }

    // 误判率 1% 要每个 key 10 位，0.1% 要 15 位。
    #[test]
    fn bits_per_key_follows_false_positive_rate() {
        assert_eq!(Bloom::bits_per_key(0.01), 10);
        assert_eq!(Bloom::bits_per_key(0.001), 15);
    }

    // k 是每个 key 的位数乘 0.69 取整，最少 1，最多 30。
    #[test]
    fn k_follows_bits_per_key_and_is_clamped() {
        let hashes = hashes_of_keys("key", 10);
        assert_eq!(Bloom::build(&hashes, 10).k(), 6);
        assert_eq!(Bloom::build(&hashes, 1).k(), 1);
        assert_eq!(Bloom::build(&hashes, 100).k(), 30);
    }

    // 总位数是 key 个数乘每个 key 的位数，至少 64，补齐到 8 的倍数。
    #[test]
    fn bit_count_follows_key_count() {
        assert_eq!(
            Bloom::build(&hashes_of_keys("key", 1000), 10).bit_count(),
            10000
        );
        assert_eq!(Bloom::build(&hashes_of_keys("key", 3), 10).bit_count(), 64);
        assert_eq!(Bloom::build(&hashes_of_keys("key", 7), 11).bit_count(), 80);
        assert_eq!(Bloom::build(&[], 10).bit_count(), 64);
    }

    // k 为 1 时只置 h 对总位数取余的那一位：第 i 位是第 i/8 个字节从低往高的第 i%8 位。
    #[test]
    fn single_hash_sets_exactly_one_bit() {
        // 每个 key 1 位：k 为 1，总位数 64。9 落在第 1 个字节的第 1 位。
        let bloom = Bloom::build(&[9], 1);
        let mut encoded = Vec::new();
        bloom.encode(&mut encoded);
        assert_eq!(encoded, vec![0, 0b10, 0, 0, 0, 0, 0, 0, 1]);
    }

    // 第二个位置是 h 加上 h 循环左移 15 位。
    #[test]
    fn second_position_adds_rotated_hash() {
        // 每个 key 3 位：k 为 2，总位数 64。h = 1 << 17，循环左移 15 位后是 1，
        // 两个位置是 h % 64 = 0 和 (h + 1) % 64 = 1。
        let bloom = Bloom::build(&[1 << 17], 3);
        assert_eq!(bloom.k(), 2);
        let mut encoded = Vec::new();
        bloom.encode(&mut encoded);
        assert_eq!(encoded, vec![0b11, 0, 0, 0, 0, 0, 0, 0, 2]);
    }

    // 查询要看全部 k 位：只有一位是 1 时回答一定没有。
    #[test]
    fn may_contain_needs_all_k_bits() {
        // 与上一个测试同一个 h，它的两个位置是第 0 位和第 1 位。
        let h = 1 << 17;
        let only_bit_0 = Bloom::decode(&[0b01, 0, 0, 0, 0, 0, 0, 0, 2]).unwrap();
        let only_bit_1 = Bloom::decode(&[0b10, 0, 0, 0, 0, 0, 0, 0, 2]).unwrap();
        let both_bits = Bloom::decode(&[0b11, 0, 0, 0, 0, 0, 0, 0, 2]).unwrap();
        assert!(!only_bit_0.may_contain(h));
        assert!(!only_bit_1.may_contain(h));
        assert!(both_bits.may_contain(h));
    }

    // 插入过的 key 全部回答可能有（不会漏报），各种位数都一样。
    // key 的个数取奇数，让"key 个数乘位数"不是 8 的倍数，要补齐。
    #[test]
    fn inserted_keys_are_never_rejected() {
        let hashes = hashes_of_keys("key", 4999);
        for bits_per_key in [1, 2, 5, 10, 20, 40] {
            let bloom = Bloom::build(&hashes, bits_per_key);
            for &hash in &hashes {
                assert!(bloom.may_contain(hash), "每个 key {bits_per_key} 位时漏报");
            }
        }
    }

    // 哈希值接近 u32 的上限时，求下一个位置要回绕，不能溢出。
    #[test]
    fn hashes_near_u32_max_wrap_around() {
        let hashes = [u32::MAX, u32::MAX - 1, 0x8000_0000];
        let bloom = Bloom::build(&hashes, 20);
        for hash in hashes {
            assert!(bloom.may_contain(hash));
        }
    }

    // 每个 key 10 位时，没插入过的 key 被误判为可能有的比例在 1% 左右。
    #[test]
    fn false_positive_rate_is_about_one_percent() {
        let bloom = Bloom::build(&hashes_of_keys("key", 10000), 10);
        let absent = hashes_of_keys("missing", 10000);
        let false_positives = absent
            .iter()
            .filter(|&&hash| bloom.may_contain(hash))
            .count();
        assert!(
            false_positives < 200,
            "误报了 {false_positives} 次，超过 2%"
        );
    }

    // 空的 filter 对任何 key 都回答一定没有。
    #[test]
    fn empty_filter_rejects_everything() {
        let bloom = Bloom::build(&[], 10);
        for hash in hashes_of_keys("key", 100) {
            assert!(!bloom.may_contain(hash));
        }
    }

    // 编码再解码得到同一个 filter，长度与 encoded_len 一致。
    #[test]
    fn encode_then_decode_round_trips() {
        let bloom = Bloom::build(&hashes_of_keys("key", 1000), 10);
        let mut encoded = Vec::new();
        bloom.encode(&mut encoded);
        assert_eq!(encoded.len(), bloom.encoded_len());
        assert_eq!(Bloom::decode(&encoded).unwrap(), bloom);
    }

    // 空的字节、只有 k、k 为 0 或超过 30，都是损坏。
    #[test]
    fn decode_rejects_malformed_bytes() {
        for bad in [&[][..], &[6], &[0xff, 0], &[0xff, 31]] {
            assert!(
                matches!(Bloom::decode(bad), Err(Error::Corruption(_))),
                "{bad:?}"
            );
        }
    }

    // key 的哈希算法固定：同一个 key 永远算出同一个值，换了算法这个测试会失败。
    #[test]
    fn key_hash_is_fixed() {
        assert_eq!(key_hash(b"hello"), key_hash(b"hello"));
        assert_ne!(key_hash(b"hello"), key_hash(b"hellp"));
        // XXH3-64 对 "hello" 的结果是 0x9555_E855_5C62_DCFD，取低 32 位。
        assert_eq!(key_hash(b"hello"), 0x5C62_DCFD);
    }
}
