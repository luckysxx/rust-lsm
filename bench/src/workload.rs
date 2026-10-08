//! 负载：决定每一步操作哪个 key。
//!
//! 同一组参数加同一个种子，产生完全相同的操作序列，改动前后才能比较。

/// key 固定 16 字节，与 RocksDB `db_bench` 的默认值相同。
pub const KEY_SIZE: usize = 16;

/// 编号最多 15 位十进制数，乘 2 加 1 之后仍能放进 16 位。
pub const MAX_KEYS: u64 = 1_000_000_000_000_000;

/// SplitMix64 伪随机数发生器。
///
/// 自己写而不用 `rand`：算法固定在这里，种子相同序列就相同，
/// 不会因为依赖升级而变化。只用于生成负载。
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[0, bound)` 中的一个数。取模带来的偏差在这里可以忽略。
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

/// 把数字写成 16 位十进制、前面补 0。这样 key 的字节序与数字大小一致。
fn encode_key(number: u64) -> [u8; KEY_SIZE] {
    let mut key = [b'0'; KEY_SIZE];
    let mut rest = number;
    for slot in key.iter_mut().rev() {
        *slot = b'0' + (rest % 10) as u8;
        rest /= 10;
    }
    assert!(rest == 0, "key 编号超出 16 位十进制");
    key
}

/// 第 `index` 个会被写入的 key，占用偶数。
pub fn existing_key(index: u64) -> [u8; KEY_SIZE] {
    encode_key(index * 2)
}

/// 第 `index` 个永远不会被写入的 key，占用奇数。
///
/// 它夹在两个存在的 key 之间，落在数据的 key 范围之内。如果把不存在的
/// key 放在范围之外，引擎只凭每个文件的首末 key 就能排除，测不出
/// Bloom filter 的作用。
pub fn missing_key(index: u64) -> [u8; KEY_SIZE] {
    encode_key(index * 2 + 1)
}

/// `0..count` 的一个随机排列（Fisher–Yates）。
pub fn shuffled_indexes(count: u64, rng: &mut Rng) -> Vec<u64> {
    let mut indexes: Vec<u64> = (0..count).collect();
    for i in (1..indexes.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        indexes.swap(i, j);
    }
    indexes
}

/// 长度为 `size` 的 value，内容由种子决定。
pub fn make_value(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut value = Vec::with_capacity(size);
    while value.len() < size {
        let word = rng.next_u64().to_le_bytes();
        let remaining = size - value.len();
        let take = remaining.min(word.len());
        value.extend_from_slice(&word[..take]);
    }
    value
}

/// 一种固定的操作序列。名字沿用 `db_bench`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// 乱序写入 `num` 个不同的 key，每个恰好一次。
    FillRandom,
    /// `num` 次写入，每次随机挑一个已有的 key 覆盖。
    Overwrite,
    /// `num` 次点查，每次随机挑一个已有的 key。
    ReadRandom,
    /// `num` 次点查，每次随机挑一个不存在、但落在 key 范围内的 key。
    ReadMissing,
}

impl Workload {
    pub const ALL: [Workload; 4] = [
        Workload::FillRandom,
        Workload::Overwrite,
        Workload::ReadRandom,
        Workload::ReadMissing,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Workload::FillRandom => "fillrandom",
            Workload::Overwrite => "overwrite",
            Workload::ReadRandom => "readrandom",
            Workload::ReadMissing => "readmissing",
        }
    }

    pub fn parse(name: &str) -> Option<Workload> {
        Workload::ALL
            .into_iter()
            .find(|workload| workload.name() == name)
    }

    /// 每种负载用自己的随机序列，结果不随负载的排列顺序变化。
    pub fn seed_offset(self) -> u64 {
        match self {
            Workload::FillRandom => 1,
            Workload::Overwrite => 2,
            Workload::ReadRandom => 3,
            Workload::ReadMissing => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_reproducible() {
        let mut first = Rng::new(42);
        let mut second = Rng::new(42);
        let mut other_seed = Rng::new(43);

        let from_first: Vec<u64> = (0..5).map(|_| first.next_u64()).collect();
        let from_second: Vec<u64> = (0..5).map(|_| second.next_u64()).collect();
        let from_other: Vec<u64> = (0..5).map(|_| other_seed.next_u64()).collect();

        assert_eq!(from_first, from_second);
        assert_ne!(from_first, from_other);
    }

    #[test]
    fn rng_matches_splitmix64_reference() {
        // SplitMix64 在种子 0 下的前三个输出，来自算法作者的参考实现。
        let mut rng = Rng::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn keys_are_fixed_width_and_ordered_like_numbers() {
        assert_eq!(&existing_key(0), b"0000000000000000");
        assert_eq!(&existing_key(5), b"0000000000000010");
        assert_eq!(&missing_key(5), b"0000000000000011");
        assert!(existing_key(9) < existing_key(10));
        assert!(existing_key(99) < existing_key(100));
    }

    #[test]
    fn missing_keys_sit_between_existing_keys() {
        for index in 0..100 {
            assert!(existing_key(index) < missing_key(index));
            assert!(missing_key(index) < existing_key(index + 1));
        }
    }

    #[test]
    fn largest_allowed_key_fits() {
        let last = MAX_KEYS - 1;
        assert_eq!(&missing_key(last), b"1999999999999999");
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut rng = Rng::new(7);
        let shuffled = shuffled_indexes(1000, &mut rng);

        let in_order: Vec<u64> = (0..1000).collect();
        assert_ne!(shuffled, in_order);

        let mut sorted = shuffled.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, in_order);
    }

    #[test]
    fn value_has_requested_size_and_depends_on_seed() {
        assert_eq!(make_value(100, 1).len(), 100);
        assert_eq!(make_value(0, 1).len(), 0);
        assert_eq!(make_value(100, 1), make_value(100, 1));
        assert_ne!(make_value(100, 1), make_value(100, 2));
    }

    #[test]
    fn workload_names_round_trip() {
        for workload in Workload::ALL {
            assert_eq!(Workload::parse(workload.name()), Some(workload));
        }
        assert_eq!(Workload::parse("nosuch"), None);
    }
}
