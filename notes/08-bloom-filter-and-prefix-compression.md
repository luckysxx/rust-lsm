# Bloom filter 与 key 前缀压缩

状态：已有正文 · 代码：[table/bloom.rs](../engine/src/table/bloom.rs) 的 `Bloom`、`key_hash`；[table.rs](../engine/src/table.rs) 的 `SsTable::bloom_may_contain`、`SST_FORMAT_VERSION`；[table/builder.rs](../engine/src/table/builder.rs) 的 `SsTableBuilder::build`；[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `get`；[block/builder.rs](../engine/src/block/builder.rs) 的 `common_prefix_len`、`BlockBuilder::add`；[block.rs](../engine/src/block.rs) 的 `Block::key_at`；[block/iterator.rs](../engine/src/block/iterator.rs) 的 `BlockIterator`

## 位置

[全景](README.md#全景)里"一次点查"在每个 SST 上的检查顺序是：首末 key 的范围 → Bloom filter → 索引找块 → 读块 → 块内查找。[读路径](06-read-path.md) 与 [写路径](07-write-path.md) 做通了这条路和第一道范围检查，这一篇加上第二道检查，并改动数据块的编码：

- **Bloom filter** 减少点查要读的文件数。
- **key 前缀压缩** 减少每个数据块占的字节。

两项都只改 SST 的格式与读法，不改变 `get`、`scan` 返回的任何结果。下一篇开始做 [compaction](09-compaction-execution.md)。

## 与 B+Tree 阶段的对应

| B+Tree | LSM |
| --- | --- |
| 一个 key 只可能在一个地方：从根走一条路径就能确定有没有，查不存在的 key 与查存在的 key 代价相同 | 一个 key 可能在每张 memtable、每个 L0 文件、每一层里；查不存在的 key 要问遍所有候选。Bloom filter 让每个候选能便宜地回答"这里一定没有" |
| 页内的长 key 与前缀压缩（存储笔记[长 key 与分隔键](https://github.com/luckysxx/go-btree-kv/blob/main/notes/06-storage-comparisons/02-large-keys-and-separators.md)） | 块内 key 的前缀压缩：页和块里的 key 都有序，相邻的 key 前缀相似 |
| meta 页里的魔数与版本 | SST 末尾的魔数与格式版本号，见 [SST 格式](05-sst-format.md#文件的组成) |

## Bloom filter

### 问题：范围检查答不了的情况

范围检查只能回答"key 在不在这个文件的 [首 key, 末 key] 之间"，回答不了"在范围里，但文件里没有"。乱序写入时，L0 每个文件的范围都几乎覆盖全部 key：

- 查一个存在的 key，平均要看一半的文件才找到它。
- 查一个不存在的 key，要把所有文件都看一遍。

每看一个文件就是一次找块、读块、块内二分。数据在页缓存里时一次约 1 微秒；数据大于内存时就是一次 SSD 读。

compaction 之后，每层最多一个文件的范围覆盖这个 key，但查不存在的 key 仍然每层都要读一个块。所以需要一种只占少量内存、常驻内存的摘要，能对每个候选文件回答"这里一定没有"。

### 候选

| 做法 | 每个 key 占的内存 | 回答 |
| --- | --- | --- |
| 文件里所有 key 放进内存的哈希集合 | key 本身的长度再加集合的开销 | 准确，但等于把文件的 key 搬进内存 |
| 每个 key 的 32 位哈希，排序后二分 | 4 字节 | 哈希冲突时把"没有"错答成"可能有"，概率极低 |
| Bloom filter | 约 10 位（1.25 字节） | 约 1% 的概率把"没有"错答成"可能有" |

LevelDB、RocksDB、Pebble 默认都用 Bloom filter。RocksDB 后来加了 Ribbon filter，相同误判率下再省约 30% 的内存，原理相近。

### 原理

从哈希表出发：哈希表查找时，先用哈希值选一个桶，再到桶里**比对存着的 key**。如果为了省内存不存 key、每个桶只留 1 位（有没有 key 落进来过），就得到一个位数组：

- 插入：`pos = hash(key) % m`，第 `pos` 位置 1。
- 查询：第 `pos` 位是 0，说明从没有 key 落到这里，这个 key 一定没插入过；是 1，可能是它，也可能是别的 key 落在了同一位，没有存 key 就无法比对，只能回答"可能有"。

这就是只有一个哈希函数的 Bloom filter。两条性质都来自"只留 1 位、不存 key"：

- **不会漏报**：插入过的 key，它的那一位一定被置过 1，位只会被置 1、不会被清 0。
- **会误报**：别的 key 碰巧把同一位置成了 1。

一般的 Bloom filter 让每个 key 置 **k** 位，查询时同样的 k 位全是 1 才回答"可能有"。一个不存在的 key 要同时撞上 k 个 1 才会误报。

### 位数、k 与误判率

所有 key 共用同一个位数组。**每个 key 分到的位数** b = m / n（总位数除以 key 的个数）只是一个比例，用来决定数组开多大；它与 k 是两个不同的数。用这个比例而不用数组的绝对大小来衡量，原因与哈希表用负载因子衡量拥挤程度相同：误判率只取决于两者的比值。

插入 n 个 key、每个置 k 位，一共置位 kn 次，某一位一次都没被置到的概率是

```
q = (1 − 1/m)^(kn) ≈ e^(−k/b)
```

q 是数组里 0 的比例。误报要 k 位都碰上 1：

```
p = (1 − q)^k
```

k 变大，1 变多（坏），但要同时撞上的位也变多（好），中间有最佳点。由 q = e^(−k/b) 得 k = −b·ln q，代入两边取对数：

```
ln p = −b · ln q · ln(1 − q)
```

右边把 q 与 1 − q 对调后不变，所以最大值在 q = 1/2：**最优时数组里恰好一半是 0、一半是 1**，此时

```
k = b · ln 2 ≈ 0.69 · b
p = (1/2)^k
b = −ln p / (ln 2)²
```

| 每个 key 的位数 | 最优的 k | 误判率 |
| ---: | ---: | ---: |
| 5 | 3 | 约 9% |
| 10 | 7 | 约 0.8% |
| 15 | 10 | 约 0.07% |
| 20 | 14 | 约 0.007% |

误判率每降一个数量级，每个 key 多给 ln 10 / (ln 2)² ≈ 4.8 位。最佳点附近曲线很平：每个 key 10 位时 k 取 6 的误判率 0.84%，取 7 是 0.82%，所以 LevelDB 与 mini-lsm 把 `b × 0.69` 直接截断取整，少检查一位。

直观的理解：1 太少，说明数组没用满，可以多置几位让检查更严；1 太多，每一位都几乎是 1，检查一位几乎筛不掉什么。一半一半时，每检查一位恰好筛掉一半。

### k 个位置从哪来

不必真的准备 k 个哈希函数。常见做法有两种：

- 同一个哈希函数换 k 个种子，每个 key 要哈希 k 遍。
- **只算一次哈希 h，推出 k 个位置**：第 i 个位置是 `(h + i·delta) % m`，`delta` 由 h 得到。LevelDB 与 mini-lsm 取 `delta = h 循环左移 15 位`，把高低位打乱，当作第二个哈希值用。Kirsch 与 Mitzenmacher（2006）证明，用两个哈希值这样线性组合出的 k 个位置，误判率与 k 个独立的哈希函数基本相同。

位置会重复：不同 key 落到同一位，这正是误报的来源，已经算在公式里；同一个 key 的几个位置互相撞上（例如 `delta` 恰好是 m 的倍数），也只是这个 key 实际置的位少了，插入与查询算出的是同一组位置，仍然不会漏报。工业实现都不特意处理。

### 不变量：不会漏报

错的方向决定了能不能用：

- **误报**（不存在的 key 回答"可能有"）只多读一个块，读完发现没有，接着查更旧的文件，结果仍然正确。
- **漏报**（存在的 key 回答"一定没有"）会出错：点查跳过了真正有这个 key 的新文件，去更旧的文件里读到被覆盖的旧值或被删除的值，或者直接返回不存在。

所以要守住：**filter 说没有，文件里就一定没有。** 由此得到三条要求：

1. **构建与查询算出完全相同的位置。** 同样的 key 字节、同样的哈希、同样的位置公式、同样的总位数。最稳妥的办法是两边调用同一个求位置的函数，而不是各写一份。总位数也要两边一致：构建时按"开出来的字节数 × 8"计算，查询时也用它，不能一边用补齐之前的位数。
2. **哈希算法永远不变。** filter 存在文件里，今天写的文件明天用新编译的程序查，必须算出同样的位置。所以不能用带随机种子的哈希：Rust 标准库 `HashMap` 默认的哈希每个进程种子不同，而且文档写明算法可能随版本改变（Go 的 map 也是每个进程随机种子）。要用算法固定的哈希函数，种子写成常量。
3. **k 跟着文件走。** k 存在 filter 自己的字节里，以后调整每个 key 的位数，旧文件仍按写入时的 k 查询。

### 存在哪里、何时读进内存

filter 放在索引之后，末尾的 footer 多记一个"filter 的起点"；打开文件时与索引一起读进内存，常驻。每个 key 约 1.25 字节：30 万个 key 一共约 375 KB，一个约 9000 条的文件约 11 KB。

数据到了上亿条，filter 也有上百 MB 到几 GB，常驻内存放不下。RocksDB 的办法是 partitioned filter：把 filter 也切成块，上面加一层索引，filter 块和数据块一样进块缓存、按需读取。

### 用在点查路径上

```
算一次 key 的哈希值                     ← 只和 key 有关，所有文件共用
对每个 L0 文件（新的在前）：
    key 不在 [首 key, 末 key] 之间 → 跳过
    filter 说一定没有              → 跳过
    找块、读块、块内查找；找到就返回
```

范围检查放在前面，因为它只是两次比较；filter 要检查 k 个随机位置。两道检查都只排除文件、不改变"从新到旧、遇到就停"的顺序。

### 扫描用不上

filter 只能回答**某一个** key 在不在，而一个范围里可能的 key 无穷多，没法一个个问。两种特殊情况：

- **前缀 Bloom filter**：插入的不是整个 key，而是它的前缀（RocksDB 的 `prefix_extractor`），之后"扫描以某个前缀开头的 key"就能先问 filter。
- **按列建 filter**：时序数据库的查询常是"某段时间内 `host = 'a'` 的数据"，本身是扫描，但带一个等值条件，按 `host` 这一列建的 filter 回答的正是这个条件。GreptimeDB 就是这样，见下面的导读。

### 效果

没有 compaction 时 L0 有 N 个范围都重叠的文件，误判率为 p，预计每次点查进入：

- 存在的 key：1 + (N − 1)·p 个文件（真正有它的那个，加上误报）。
- 不存在的 key：N·p 个。

[2026-10-14 的基准](../bench/results/2026-10-14-bloom-filter.md) 中 N = 33、p = 1%：readrandom 每次进入的文件从 16.9 降到 1.13，readmissing 从 33 降到 0.29，与预测一致；查不存在的 key 快了 19～43 倍。范围已经互不重叠时（顺序写入，或 compaction 之后的层），存在的 key 没有变化，因为范围检查已经把候选缩到真正有它的那一个文件；不存在的 key 从"读一个块才知道没有"变成"不读块"。代价是文件大约 1%、内存里常驻全部 filter。

## key 前缀压缩

### 问题

块里的记录按 key 排好序，相邻的 key 往往有很长的公共前缀。真实的 key 常由"表 ID + 主键 + 列名"或"指标名 + 标签 + 时间戳"拼成；基准里的 key 是 16 位十进制数、前面补 0。排好序之后，同一个块里的 key 数值挨得很近，高位也相同，就像字典同一页上的单词开头几个字母一样。

决定能省多少的不是 key 的长短，而是**排序后相邻的 key 开头像不像**。key 长短不一也能压缩，剩余部分有多长记多长；key 像随机字节（哈希值、随机生成的 UUID）时，相邻 key 几乎没有共有前缀，压缩反而让每条记录多出一个长度字段。

### 原理

选一个参照 key，每个 key 只记"与参照 key 相同的前缀有多长"和"剩下的部分"：

```
共有前缀的长度 │ 剩余部分的长度 │ 剩余部分 │ value 长度 │ value
```

还原时取参照 key 的前若干字节，接上剩余部分。参照 key 自己完整存放，共有前缀为 0：它不能再参照别人，否则还原它需要它自己。

### 参照第一个 key，还是上一个 key

块里依次是 `a/10/01`、`a/10/02`、`a/11/07`、`a/11/08`：

| key | 与第一个 key 比 | 与上一个 key 比 |
| --- | --- | --- |
| `a/10/01` | 完整存放 | 完整存放 |
| `a/10/02` | 共有 6，存 `2` | 共有 6，存 `2` |
| `a/11/07` | 共有 3，存 `1/07` | 共有 3，存 `1/07` |
| `a/11/08` | 共有 3，存 `1/08` | 共有 6，存 `8` |

块里越往后，key 与第一个 key 差得越远，与上一个 key 却仍然很像，所以参照上一个 key 压得更多。代价在随机访问：第 i 个 key 依赖第 i − 1 个，要还原中间某一个必须从头走过来，块内二分就做不了。LevelDB 用 **restart point** 解决：每 16 条强制完整存一次 key，块尾只记这些位置；查找时先在 restart point 上二分（它们的 key 是完整的，可以原地比较，不用拼），再从那里顺序走，最多 16 条。

| | 参照第一个 key（mini-lsm、本项目） | 参照上一个 key（LevelDB、RocksDB、Pebble） |
| --- | --- | --- |
| 压缩程度 | 较少 | 较多 |
| 随机还原第 i 个 key | 一步 | 从最近的 restart point 顺序走过来 |
| 块内二分 | 每条记录都可以二分 | 只在 restart point 上二分，然后线性走 |
| 偏移数组 | 每条一项 | 每 16 条一项，也省空间 |

参照第一个 key，相当于整个块只有一个 restart point，同时保留了每条一项的偏移数组。在基准的数据上，与第一个 key 平均共有 12.1 字节、与上一个 key 平均共有 13.5 字节；换成参照上一个 key 并只在 restart point 记偏移，每条记录大约再省 3 字节，不到 3%。

### 能省多少

基准里每个 key 平均省下约 12 字节，减去新增的 2 字节"共有前缀长度"，每条记录净省约 10 字节；原来一条记录连同偏移 122 字节，所以文件小约 8%。[2026-10-15 的基准](../bench/results/2026-10-15-prefix-compression.md) 实测小了 7.9%～8.0%。value 占了记录的大头，前缀压缩只动 key，key 越长、value 越短，比例越高。

数据都在页缓存里时，读写吞吐没有超出浮动的变化：块里多装 8% 的记录不会让读一个块明显变快。收益在数据大于内存时出现：同样的块缓存多装 8% 的记录，读同样多的记录少读 8% 的块。

### 还原 key 的代价

压缩之后，完整的 key 在块的字节里不再是连续的一段，游标的 `key()` 无法再直接借用块里的字节。做法是游标自带一个缓冲区，每次移动位置之后把"参照 key 的前缀"和"剩余部分"拼进去，`key()` 借出这个缓冲区。缓冲区反复使用，清空只把长度归零、不释放内存，之后不再分配，与 Go 里 `buf = buf[:0]` 再 `append` 是同一个手法。

块内二分每一步都要比较中间那条的 key。可以不拼：两个字节串的字典序，等于先比第一段、相同再比第二段，所以能拿目标先与"参照 key 的前缀"比、再与"剩余部分"比。要小心目标比前缀还短、恰好是前缀开头一段的情况，这时目标更小。对 16 字节左右的 key，拼一次只是一次很短的复制，块里二分五六步，合计几十纳秒，占一次点查不到 3%；key 很长或块很大时，分段比较才值得。Rust 里不分配内存、也不用自己处理边界的分段比较可以写成 `prefix.iter().chain(rest).cmp(target.iter())`。

### 为什么按字节而不按位

内存和文件按字节编址，切片只能从字节边界开始。共有前缀按位计时，剩余部分可能从一个字节的中间开始，写入和还原都要把后面每个字节移位，一次复制变成逐字节运算；而按位最多比按字节多省 7 位，不到 1 字节。key 的比较本身也是按字节的字典序（`memcmp`）。按位处理前缀的结构是 crit-bit 树、Patricia 树这类查找结构，它们按位是为了每层只做一次 0 或 1 的判断，并不按位切开存储 key。

### 为以后改格式留余地

先选简单的做法，以后可能要换成更省的做法（参照上一个 key、变长整数的长度字段、整块压缩）。让以后的修改容易，靠的是这几件事，而不是现在就为两种编码写一个抽象接口：

- **格式只让一个模块知道。** 记录的布局只有块的构建器、块、块上的游标三个文件知道；SST 的构建器只看 `add` 放没放下，游标的使用方只用 `key`、`value`、`next`、`seek`。换编码时改动不出这个模块。
- **接口按更难的那种做法设计。** 游标自带 key 缓冲区的接口，两种参照方式都能用；如果为"参照第一个 key"设计一个返回两段切片的接口，换成参照上一个 key 时接口和所有调用方都要改。
- **格式带版本号。** 块的编码变了就加版本号，见 [SST 格式](05-sst-format.md#文件的组成)。
- **测试分两类。** 大多数测试检查行为（编码后能解回、顺序遍历、定位、坏数据不 panic），换编码也成立；少数测试逐字节检查格式，标明是格式测试，换编码时只改这几个。
- **改之前先量。** 上面算过，参照上一个 key 只多省不到 3%；变长整数的长度字段与整块压缩不动记录之间的参照关系，收益更大。

## 本项目的做法

**Bloom filter**（[table/bloom.rs](../engine/src/table/bloom.rs)）：

- `key_hash(key)`：XXH3 的 64 位哈希取低 32 位（`xxhash-rust` crate 的 `xxh3` feature）。有一个测试把 `"hello"` 的哈希值写成常量，换了算法这个测试会失败。
- `Bloom::build(key_hashes, bits_per_key)`：k 取 `bits_per_key × 0.69` 截断，限制在 1 到 30；总位数是 key 个数乘每个 key 的位数，至少 64，补齐到 8 的倍数。`Bloom::bits_per_key(0.01)` 得 10。
- `Bloom::may_contain(hash)`：有一位是 0 就返回 `false`。
- `bit_positions(hash, k, bit_count)`：两边共用的求位置函数，第 i 个位置是 `hash + i × delta` 对总位数取余，乘法与加法都用 `wrapping_` 按 32 位回绕（Rust 在 debug 编译下整数溢出会 panic，不像 C 和 Go 那样悄悄回绕）。它返回一个迭代器，`for` 要下一个时才算，不分配内存。
- 编码：位数组之后接 1 字节的 k。第 i 位在第 `i / 8` 个字节，是这个字节从低往高的第 `i % 8` 位。解码时位数组为空或 k 不在 1 到 30 之间返回内容损坏。

**SST**（[table.rs](../engine/src/table.rs)、[table/builder.rs](../engine/src/table/builder.rs)）：构建器在 `add` 时记下每个 key 的哈希值，`build` 时在索引之后写 filter，最后写 16 字节的 footer（索引的起点、filter 的起点、版本号、魔数）。`SsTable::open` 一次读出索引与 filter，从 filter 的起点切开。目标误判率是常量 `BLOOM_FALSE_POSITIVE_RATE = 0.01`。

**点查**（[lsm_storage.rs](../engine/src/lsm_storage.rs) 的 `get`）：循环之前算一次哈希；范围检查之后问 filter，被排除时计入 `Stats` 的 `tables_skipped_by_bloom`。基准报告里有"每次点查被 Bloom filter 排除的 SST"一列。

**前缀压缩**（[block/builder.rs](../engine/src/block/builder.rs)、[block.rs](../engine/src/block.rs)、[block/iterator.rs](../engine/src/block/iterator.rs)）：构建器记住块的第一个 key，`add` 用 `common_prefix_len` 算共有前缀，按压缩之后的大小判断放不放得下。`Block::key_at(index, buf)` 把完整的 key 拼进调用方给的缓冲区；游标用自己的 `key` 字段作缓冲区。解码时检查第一条的共有前缀为 0、共有前缀不超过第一个 key 的长度、key 不为空，后一项保证还原时切片不越界。

## 工业对照

**mini-lsm**（[table/bloom.rs](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm/src/table/bloom.rs)）：位置公式、`delta` 的取法、k 的算法与本项目相同；哈希用 `farmhash::fingerprint32`。它的 filter 编码末尾多一个 crc32 校验和，本项目的校验和放到 [写批次与校验和](15-write-batch-and-checksums.md) 统一做。前缀压缩同样参照块的第一个 key。

**LevelDB 与 RocksDB**：

- LevelDB 的 filter 不是每个文件一个，而是按文件里每 2 KB 的数据各做一个，放在单独的 filter 块里；RocksDB 改为每个文件一个完整的 filter（full filter），文件很大时再切成 partitioned filter 放进块缓存。RocksDB 还提供 Ribbon filter。
- 两者的块内前缀压缩都参照上一个 key，restart point 默认每 16 条一个，长度字段用变长整数。
- RocksDB 的 footer 带 `format_version`，新的 filter 实现、校验方式都随它递增。

**GreptimeDB mito2**：filter 不按 key 建，而是按**列**建，存在 SST 旁边的索引文件（puffin 格式）里，查询时回答"某一列等于某个值的行可能在哪些 row group"。差异来自数据模型：时序查询多是"某段时间内、某些标签等于某值"的扫描，而不是按 key 的点查。mito2 的 SST 是 Parquet 文件，列的编码与压缩由 Parquet 负责，没有自己的块内 key 前缀压缩。

### 导读：一段行做一个 filter

代码在 `src/index/src/bloom_filter/creator.rs` 与 `creator/finalize_segment.rs`（`index` crate，mito2 通过 `sst/index/bloom_filter/creator.rs` 调用它）。先看写入时怎样把一列的值收集起来：

```rust
// creator.rs：BloomFilterCreator::push_row_elems（删去了内存统计）
pub async fn push_row_elems(&mut self, elems: impl IntoIterator<Item = Bytes>) -> Result<()> {
    self.accumulated_row_count += 1;
    for elem in elems.into_iter() {
        self.cur_seg_distinct_elems.insert(elem);   // HashSet：同一段里重复的值只算一次
    }
    if self.accumulated_row_count.is_multiple_of(self.rows_per_segment) {
        self.finalize_segment().await?;               // 攒满一段行，做一个 filter
        self.finalized_row_count = self.accumulated_row_count;
    }
    Ok(())
}
```

再看一段攒满之后怎样做成 filter：

```rust
// finalize_segment.rs：FinalizedBloomFilterStorage::add（删去了内存统计与写临时文件）
let mut bf = BloomFilter::with_false_pos(self.false_positive_rate)
    .seed(&SEED)                       // SEED 是常量 42
    .expected_items(element_count);    // 这一段里不同的值有几个
for elem in elems.into_iter() {
    bf.insert(&elem);
}
let fbf = FinalizedBloomFilterSegment::from(bf, element_count);

// 与上一段做出的 filter 完全相同时，不再存一份，只记"这一段用上一个"
if self.in_memory.last() == Some(&fbf) {
    self.segment_indices.push(self.flushed_seg_count + self.in_memory.len() - 1);
    return Ok(());
}
```

与本项目逐行对上：

| mito2 | 本项目 |
| --- | --- |
| `cur_seg_distinct_elems.insert(elem)` | 构建器的 `add` 里 `key_hashes.push(key_hash(key))`；SST 里的 key 本来就不重复，不需要去重 |
| 每 `rows_per_segment` 行做一个 filter | 每个 SST 文件做一个 filter；LevelDB 的"每 2 KB 数据一个"是同一类按段切分的做法 |
| `with_false_pos(rate).expected_items(n)` | `Bloom::bits_per_key(0.01)` 算出每个 key 的位数，`Bloom::build` 按 key 的个数开数组 |
| `.seed(&SEED)`，`SEED` 是常量 | `key_hash` 用固定算法、不带随机种子：filter 存在文件里，哈希不能随进程改变 |
| 相邻两段的 filter 相同就复用 | 没有对应：时序数据里一个标签值往往连续很多行不变，相邻两段的值集合常常完全相同 |

可以先跳过的部分：`global_memory_usage` 相关的内存统计，以及 filter 太多时先写到临时文件（`intermediate_provider`）的逻辑，它们是为了在构建大文件时控制内存。`BloomFilter` 本身来自 `fastbloom` crate。

查询的一侧在 `src/mito2/src/sst/index/bloom_filter/applier.rs`：`BloomFilterIndexApplier::apply` 接收每一列的等值或 `IN` 条件（`InListPredicate`）和要搜索的 row group，返回每个 row group 里可能有匹配的行范围。它对应本项目 `get` 里"filter 说一定没有就跳过"那一行，只是跳过的单位从整个文件变成了 row group 里的一段行。

## 常见错误

- **构建与查询各写一份求位置的代码。** 改了一边忘了另一边，存在的 key 被判成"一定没有"。
- **查询时用的总位数与构建时不同。** 例如构建时按补齐之前的位数取余，查询时按字节数乘 8 取余，同一个哈希值落到不同的位上。
- **用带随机种子的哈希。** 同一个进程里测试全部通过，重启之后旧文件里存在的 key 全部查不到。
- **求下一个位置时用普通加法。** debug 编译下哈希值接近 `u32::MAX` 时溢出 panic。
- **以为 filter 能证明 key 存在。** 回答"可能有"之后仍然要读块确认；把它当成"有"会返回错误的结果。
- **k 写死在代码里而不存进文件。** 以后调整每个 key 的位数，旧文件按新的 k 查询，漏报。
- **每查一个文件就算一次哈希。** 结果不变，只是浪费；哈希值只和 key 有关。
- **前缀压缩参照了上一个 key，偏移数组却仍然每条一项、直接二分。** 还原中间某一条时拿错了参照，得到错误的 key。
- **按未压缩的大小判断块放不放得下。** 块装不满，压缩省下的空间白白浪费；反过来按压缩之后判断、却按未压缩写入，块会超过目标大小。
- **解码时不检查共有前缀是否超过第一个 key 的长度。** 坏数据在还原 key 时切片越界 panic。
- **还原 key 前不清空缓冲区。** 新 key 接在旧 key 后面。
- **改了格式却没有改版本号。** 旧程序把新文件的字节读错位，而不是报告不认识的版本。

## 资料

- mini-lsm 教程第一周第 7 章：[week1-07-sst-optimizations.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week1-07-sst-optimizations.md)。
- Kirsch, Mitzenmacher. *Less Hashing, Same Performance: Building a Better Bloom Filter*（2006）：两个哈希值线性组合出 k 个位置。
- 已学内容中相关的篇：存储笔记[长 key 与分隔键](https://github.com/luckysxx/go-btree-kv/blob/main/notes/06-storage-comparisons/02-large-keys-and-separators.md)、[B+Tree 与 LSM](https://github.com/luckysxx/go-btree-kv/blob/main/notes/06-storage-comparisons/03-btree-and-lsm.md)。
- 基准记录：[Bloom filter 的前后对比](../bench/results/2026-10-14-bloom-filter.md)、[块内 key 前缀压缩的前后对比](../bench/results/2026-10-15-prefix-compression.md)。

返回 [本组目录](README.md)。
