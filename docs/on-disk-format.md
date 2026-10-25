# 磁盘格式

本文是引擎写在磁盘上的每一种文件的格式规范，精确到字节：数据库目录、SST（数据块、索引、Bloom filter、末尾）、WAL、MANIFEST。另外说明读的时候检查什么、遇到损坏怎样处理，以及改格式时要遵守的兼容规则。

本文描述的是 2026-10-24 的代码，此时 SST 的格式版本是 2。文中的字节示例来自用 `lsm-cli` 实际生成的文件。改动下文的任何格式时，要一起改本文。

格式为什么这样设计，见设计笔记 [L04 数据块](../notes/04-block-format.md)、[L05 SST](../notes/05-sst-format.md)、[L08 Bloom filter 与前缀压缩](../notes/08-bloom-filter-and-prefix-compression.md)、[L13 manifest](../notes/13-manifest.md)、[L14 WAL](../notes/14-wal.md)、[L15 写批次与校验和](../notes/15-write-batch-and-checksums.md)。这些文件在什么时候写、什么时候删，见[架构](architecture.md#换状态的协议)。

## 通用约定

- **整数都是大端、无符号**：`u8`、`u16`、`u32`、`u64` 分别占 1、2、4、8 字节。
- **校验和**：crc32 一律用 CRC-32/ISO-HDLC（IEEE 802.3 的多项式，与 zlib 的 `crc32` 相同），由 `crc32fast` 计算。存为 `u32`。
- **key 与 value**：都是任意字节，按字节序比较。key 不能为空，最长 65535 字节；value 最长 65535 字节。**空的 value 表示墓碑**（这个 key 被删除了），所以用户写入的 value 不能为空。
- **长度上限来自格式**：key 与 value 的长度字段在块、索引、WAL 里都是 `u16`。写入的入口 `LsmStorage::write` 先检查长度，超过时返回 `Error::InvalidArgument`，不会写出读不回来的文件。

## 数据库目录

一个数据库是一个目录：

```text
db/
├── MANIFEST       变更日志：各层有哪些 SST
├── 00015.sst      SST
├── 00016.sst
├── 00017.sst
├── 00019.sst
└── 00018.wal      一张 memtable 的 WAL
```

| 文件 | 名字 | 由谁写 |
| --- | --- | --- |
| manifest | 固定为 `MANIFEST`（`manifest::MANIFEST_FILE_NAME`） | 只追加，每条记录追加后立刻 fsync |
| SST | `{编号:05}.sst`，例如 `00007.sst` | 一次写完：`File::create_new`，`write_all`，fsync，之后只读 |
| WAL | `{编号:05}.wal`，例如 `00018.wal` | 只追加；是否 fsync 由写入的选项决定 |

文件名的规则：

- **编号**：SST 与 memtable 共用一个计数器，从 1 开始，只增不减，不重复使用，所以 `.sst` 与 `.wal` 不会同名。编号用 5 位十进制、左边补 0；超过 99999 时位数随之变多（`{:05}` 只规定最少 5 位）。读目录时把扩展名前面的部分按十进制整数解析。
- **文件名不表示层**：一个 SST 在哪一层只由 manifest 决定。
- **没有临时文件**：SST 直接以最终的名字创建，不经过"写临时文件再改名"。写了一半的 SST 不在 manifest 里，下次打开时被当作残留删掉。
- **其他文件**：扩展名不是 `.sst` 或 `.wal`、或者扩展名前面不是整数的文件，引擎不读也不删。
- **正常关闭后也有 WAL**：`close` 冻结可写的表时，会为新换上的空表建一个 WAL。所以正常关闭的数据库目录里也会有一个空的 `.wal`，下次打开时照常重放（里面没有记录）。

每次在目录里新建文件之后，都要同步一次目录，让"目录里多了这个文件"也落到磁盘上：WAL 与 manifest 在创建时同步，SST 在一次 flush 或合并写完全部输出之后同步。

## SST

一个 SST 是一个有序、不可变的文件。打开时只读进索引和 Bloom filter，数据块用到哪个读哪个。

### 整体布局（版本 2）

```text
┌──────────┬───────┬──────────┬───────┬───┬──────┬───────┬────────┬───────┬────────┐
│ 数据块 0 │ crc32 │ 数据块 1 │ crc32 │ … │ 索引 │ crc32 │ filter │ crc32 │ 末尾   │
└──────────┴───────┴──────────┴───────┴───┴──────┴───────┴────────┴───────┴────────┘
 ▲                                          ▲                ▲                16 字节
 0                                          block_meta_start bloom_start
```

**末尾**（footer）固定 16 字节：

| 偏移（从末尾往前数） | 类型 | 字段 | 说明 |
| --- | --- | --- | --- |
| −16 | `u32` | `block_meta_start` | 索引所在段的起点 |
| −12 | `u32` | `bloom_start` | filter 所在段的起点 |
| −8 | `u32` | `version` | 格式版本，现在是 2（`SST_FORMAT_VERSION`） |
| −4 | `u32` | `magic` | `0x4C534D31`，也就是 ASCII 的 `LSM1`（`SST_MAGIC`） |

各段的范围都由起点推出，不另外记长度：

| 段 | 范围 |
| --- | --- |
| 第 i 个数据块 | `[索引第 i 项的 offset, 第 i+1 项的 offset)`；最后一个块到 `block_meta_start` 为止 |
| 索引 | `[block_meta_start, bloom_start)` |
| filter | `[bloom_start, 文件长度 − 16)` |

**校验和**：版本 2 里，每一段的最后 4 字节是这一段前面全部内容的 crc32，只覆盖这一段。所以读一个数据块只需要读这一段，不用读别的。末尾的 16 字节自己没有校验和：魔数与版本号各自核对；两个起点先检查范围，指错了位置的话，切出来那一段的校验和就对不上。

**大小上限**：偏移都是 `u32`，所以一个 SST 最多 `MAX_SST_SIZE`（4 GiB − 1）字节。`SsTableBuilder::build` 在创建文件之前检查整个文件的大小，超过时返回 `Error::InvalidArgument`，不留下文件。flush 把一整张 memtable 写成一个文件，不按 `target_sst_size` 拆分，所以 memtable 的上限必须远小于 4 GiB：否则这张表的 flush 每一轮都失败，它一直留在内存里。

### 数据块

```text
┌───────┬───────┬───┬─────────┬───────┬───────┬───┬─────────┬───────┐
│ 记录 0 │ 记录 1 │ … │ 记录 n-1 │ 偏移 0 │ 偏移 1 │ … │ 偏移 n-1 │ n     │
└───────┴───────┴───┴─────────┴───────┴───────┴───┴─────────┴───────┘
  数据区                          偏移数组（n 个 u16）          u16
```

- **偏移 i**（`u16`）：记录 i 在数据区里的起点。偏移 0 总是 0。
- **n**（`u16`）：记录的条数，至少 1。

一条记录：

| 类型 | 字段 | 说明 |
| --- | --- | --- |
| `u16` | `overlap_len` | 这个 key 与本块**第一个 key** 共有前缀的长度 |
| `u16` | `rest_len` | key 剩余部分的长度 |
| `rest_len` 字节 | `rest` | key 去掉共有前缀之后的部分 |
| `u16` | `value_len` | value 的长度，0 是墓碑 |
| `value_len` 字节 | `value` | |

完整的 key 是"第一个 key 的前 `overlap_len` 个字节"接上 `rest`。第一条记录的 `overlap_len` 是 0，`rest` 就是完整的 key。每个 key 只和第一个 key 比较前缀，不和前一个 key 比较，所以任意一条记录都能单独还原出 key，偏移数组上照样可以二分查找。

块里的记录按 key 严格递增，同一个 key 在一个 SST 里只出现一次。

**块的大小**：`LsmStorageOptions::block_size` 是目标大小，不能超过 65535（偏移是 `u16`），不含后面的 crc32。加入一条记录会让块超过目标大小时，就封口、另开一个块；但空块总是收下第一条记录，所以只有一条很大的记录的块可以超过 65535 字节（它的偏移是 0，仍然记得下）。

**例子**：下面是一个块的前 48 字节，第一个 key 是 `key-000374`，第二个是 `key-000375`：

```text
00000000: 0000 000a 6b65 792d 3030 3033 3734 000c  ....key-000374..
00000010: 7661 6c75 652d 3030 3033 3734 0009 0001  value-000374....
00000020: 3500 0c76 616c 7565 2d30 3030 3337 3500  5..value-000375.
```

第一条：`overlap_len = 0`、`rest_len = 10`、`key-000374`、`value_len = 12`、`value-000374`。第二条：`overlap_len = 9`（共有 `key-00037`）、`rest_len = 1`、`5`、`value_len = 12`、`value-000375`。

### 索引

```text
┌────────────┬────────┬────────┬───┐
│ 块数 (u32) │ 第 0 项 │ 第 1 项 │ … │
└────────────┴────────┴────────┴───┘
```

每个数据块一项，按块在文件里的顺序排列：

| 类型 | 字段 | 说明 |
| --- | --- | --- |
| `u32` | `offset` | 这个块所在段的起点 |
| `u16` | `first_key_len` | |
| `first_key_len` 字节 | `first_key` | 块里最小的 key，完整存放 |
| `u16` | `last_key_len` | |
| `last_key_len` 字节 | `last_key` | 块里最大的 key，完整存放 |

整个 SST 的 key 范围是第 0 项的 `first_key` 到最后一项的 `last_key`。

### Bloom filter

```text
┌────────────────────────┬────┐
│ 位数组（m / 8 字节）    │ k  │
└────────────────────────┴────┘
                          u8
```

- **哈希**：`key_hash(key) = XXH3-64(key)` 的低 32 位（`xxhash-rust` 的 `xxh3_64`）。
- **位置**：一个 key 在 m 位的数组里置 k 位。第 i 个位置（i 从 0 到 k−1）是 `(h + i × delta) mod m`，其中 `h = key_hash(key)`，`delta` 是 `h` 循环左移 15 位，乘法与加法都按 32 位回绕。
- **位的编号**：第 j 位在第 `j / 8` 个字节里，是这个字节从低往高数的第 `j % 8` 位。
- **大小**：每个 key 分到 `bits_per_key` 位，由目标误判率 1%（`BLOOM_FALSE_POSITIVE_RATE`）算出，是 10 位。m 是 `key 数 × bits_per_key`，至少 64，再向上补齐到 8 的倍数。k 是 `bits_per_key × 0.69` 向下取整，限制在 1 到 30 之间，误判率 1% 时是 6。

**查询时用的 k 来自文件**，m 来自位数组的长度。所以改变 `bits_per_key` 或者 k 的算法，旧文件照样能查。但 `key_hash` 与求位置的算法不能改：改了之后，旧文件里存在的 key 会算出别的位置，被误判成"一定没有"。

### 版本 1

版本 1 与版本 2 只差校验和：每个数据块、索引、filter 后面都没有那 4 字节的 crc32，末尾的 `version` 是 1，其余相同。引擎仍然能打开版本 1 的文件，只是读出的内容不做校验。新写出的文件总是版本 2。

### 打开与读取时的检查

打开一个 SST（`SsTable::open`）时依次检查，任何一项不通过都返回 `Error::Corruption`：

1. 文件至少 16 字节。
2. 魔数是 `LSM1`。魔数不对，说明这根本不是本引擎写的 SST。
3. 版本号在 1 到 2 之间。
4. `block_meta_start ≤ bloom_start ≤ 文件长度 − 16`。
5. 版本 2：索引与 filter 两段的 crc32 都对得上。
6. 索引的字节恰好解完，既没有不够，也没有多出来的。
7. 索引至少有一项；第 0 项的 `offset` 是 0；`offset` 严格递增，并且都小于 `block_meta_start`；每项的 `first_key` 非空且不大于 `last_key`；相邻两项的范围不重叠（前一项的 `last_key` 小于后一项的 `first_key`）。
8. filter 的位数组非空，k 在 1 到 30 之间。

读一个数据块（`SsTable::read_block`）时：版本 2 先核对这一段的 crc32，再按块的格式检查结构（`Block::decode`）：条数不为 0；偏移从 0 开始、严格递增、落在数据区里；每条记录的三个长度字段恰好占满这条记录；第一条的 `overlap_len` 是 0；`overlap_len` 不超过第一个 key 的长度；key 不为空。

结构检查发现不了"格式合法、内容被改过"的情况，那要靠校验和。块里的 key 是否递增，读的时候也不检查。

## WAL

一个 WAL 文件属于一张 memtable，按写入的先后记下这张表收到的每一批写入。

```text
┌────────┬────────┬────────┬───┐
│ 记录 0 │ 记录 1 │ 记录 2 │ … │
└────────┴────────┴────────┴───┘
```

一条记录就是一次写入（`put`、`delete`，或者一整个 `write_batch`）：

| 类型 | 字段 | 说明 |
| --- | --- | --- |
| `u32` | `len` | 内容的字节数 |
| `len` 字节 | `body` | 一对接一对，至少一对 |
| `u32` | `crc32` | `body` 的 crc32 |

`body` 里的一对：

| 类型 | 字段 |
| --- | --- |
| `u16` | `key_len` |
| `key_len` 字节 | `key` |
| `u16` | `value_len`（0 是墓碑） |
| `value_len` 字节 | `value` |

`body` 里不记有几对：外层的 `len` 已经划出了一批的边界，一对一对往下解，正好用完为止。一批里同一个 key 出现几次时，重放时按顺序插入，后面的覆盖前面的。

**例子**：依次执行 `put apple 1`、`put apricot 22`、`delete banana`，得到的 57 字节：

```text
00000000: 0000 000a 0005 6170 706c 6500 0131 4ae9  ......apple..1J.
00000010: fcf0 0000 000d 0007 6170 7269 636f 7400  ........apricot.
00000020: 0232 3264 834f c900 0000 0a00 0662 616e  .22d.O.......ban
00000030: 616e 6100 002b dfe0 9b                   ana..+...
```

第一条：`len = 10`，内容是 `0005 apple 0001 1`，crc32 是 `4ae9fcf0`。第三条的 `value_len` 是 0，是一条墓碑。

**一条记录用一次 `write_all` 写出**，不在进程里先缓冲。因此进程崩溃时，至多最后一条记录只写了一半。

## MANIFEST

manifest 是一列记录，记下每次改变文件列表的操作。第一条记下数据库的形状，之后每条记下一次变化。

```text
┌────────┬────────┬────────┬───┐
│ 记录 0 │ 记录 1 │ 记录 2 │ … │
└────────┴────────┴────────┴───┘
```

一条记录：

| 类型 | 字段 | 说明 |
| --- | --- | --- |
| `u64` | `len` | JSON 的字节数 |
| `len` 字节 | `json` | 一条 `ManifestRecord`，用 `serde_json` 编码成 UTF-8 |
| `u32` | `crc32` | `json` 的 crc32 |

外层的帧与 WAL 相同，只是长度字段是 `u64`。

### 记录的内容

JSON 是 serde 对枚举的默认表示（externally tagged）：枚举值是只有一个键的对象，键是变体的名字；没有数据的变体直接写成字符串。

**第一条：`Init`**，数据库的形状：

```json
{"Init":{"Levels":{"max_levels":4}}}
{"Init":"Tiers"}
```

`Levels` 表示 L0 加 `max_levels` 层（不合并、简单分层、leveled 都是这种，可以互相换着打开）；`Tiers` 表示一列 tier，没有 L0（tiered）。打开时形状与选项对不上，返回 `Error::IncompatibleOptions`。

**之后每条：`Edit`**，一次变化：

```json
{"Edit":{"removed":[],"added":[3],"placement":"L0","next_sst_id":5,"flushed_memtable_id":1}}
{"Edit":{"removed":[5,3],"added":[6,7,8],"placement":{"Level":1},"next_sst_id":9}}
{"Edit":{"removed":[],"added":[3],"placement":"Tier","next_sst_id":4,"flushed_memtable_id":1}}
```

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `removed` | 编号的数组 | 去掉的 SST。编号在整个数据库里唯一，所以不写在哪一层 |
| `added` | 编号的数组 | 加上的 SST |
| `placement` | `"L0"`、`{"Level":n}` 或 `"Tier"` | `added` 放在哪里，见下表 |
| `next_sst_id` | 整数 | 追加这条记录时，下一个要分配的编号 |
| `flushed_memtable_id` | 整数，可以没有 | 这次 flush 写成 SST 的那张 memtable 的编号。只有 flush 才有；没有时整个键都不写 |

`placement` 的含义：

| 值 | 用于 | 怎样放 |
| --- | --- | --- |
| `"L0"` | 分层时的 flush | 放到 L0 的最前面，保持 `added` 里的顺序 |
| `{"Level":n}` | 分层时的合并、trivial move | 放进第 n 层（1 是 L1），n 在 1 到 `max_levels` 之间。层内的顺序不记，打开 SST 后按首 key 排序 |
| `"Tier"` | tiered 时的 flush 与合并 | `added` 作为一个新 tier，放在 `removed` 的第一个文件原来所在的 tier 的位置；`removed` 为空时放在最前面；`added` 为空时不放新 tier |

`Levels` 只能配 `"L0"` 和 `{"Level":n}`，`Tiers` 只能配 `"Tier"`，对不上就是损坏。trivial move 记成"去掉这几个文件，再把同样的编号放进下层"，所以同一个编号会同时出现在 `removed` 和 `added` 里。

### 重放

打开时从头重放（`manifest::replay`），得到崩溃或关闭之前的文件列表：

1. 第一条必须是 `Init`，之后都必须是 `Edit`。
2. 对每条 `Edit`：先从文件列表里去掉 `removed`，每个编号都必须找得到；再按 `placement` 放上 `added`。tiered 时，去掉文件后变空的 tier 也一起拿掉。
3. `next_sst_id` 取最后一条的值。
4. `flushed_memtable_id` 取最后一次出现的值，叫做**高水位**；它必须一次比一次大。编号不超过高水位的 memtable 都已经写成 SST，它们的 WAL 不再重放。

重放完成后还要检查：文件列表里的编号都小于 `next_sst_id`。否则这些编号以后还会被分配出去，与已有的文件重名。

## 末尾残缺与损坏

WAL 与 manifest 用同一套规则读（`wal::decode_batches`、`manifest` 的 `decode_records`），从头一条一条往下解：

| 情况 | 处理 |
| --- | --- |
| 剩下的字节放不下长度字段，或者放不下长度所说的整条记录 | 残尾：从这里截掉，不算错误 |
| 长度加上头尾溢出了 `usize` | 同上，当作残尾 |
| 一条完整记录的 crc32 不对，并且它是文件的最后一条 | 当作残尾截掉：写到一半时断电，文件先变长了、内容没写完 |
| 一条完整记录的 crc32 不对，后面还有记录 | `Error::Corruption`：这条记录当初写完了，是后来被改坏的 |
| crc32 对得上，内容却解不出来（WAL：最后一对不完整，或者一对都没有；manifest：JSON 不是 `ManifestRecord`） | `Error::Corruption`，在文件末尾也一样 |

截掉残尾时，用 `set_len` 把文件截到最后一条完整记录的末尾，并做 fsync，之后的追加接在那里。

WAL 只用 crc32 判断一条记录完不完整，不检查 key 是否为空：写入的入口保证不会写出空 key。

目录层面的检查（`LsmStorage::open`）：

| 情况 | 处理 |
| --- | --- |
| 没有 `MANIFEST`，目录里也没有 `.sst` 与 `.wal` | 新建数据库，写下 `Init` |
| 没有 `MANIFEST`，目录里却有 `.sst` 或 `.wal` | `Error::Corruption`：这些文件不是由 manifest 管理的，不能当作残留删掉 |
| `MANIFEST` 里一条完整的记录都没有 | 当作新建，补写 `Init` |
| manifest 记下的 SST 不在目录里 | `Error::Corruption` |
| 目录里有 manifest 没记下的 SST | 删掉：写了一半的新文件，或者已经换掉、还没来得及删的旧文件 |
| 编号不超过高水位的 WAL | 删掉 |
| 其余的 WAL | 按编号从小到大重放，都成为冻结的 memtable |

## 版本与兼容

| 文件 | 怎样标识版本 | 已有的变化 |
| --- | --- | --- |
| SST | 末尾的 `version` | 版本 1：没有校验和。版本 2：每段带 crc32 |
| WAL | 没有版本号 | 早期每条记录只有一对 key 与 value。改成批次之后，只有一对的批次与旧记录逐字节相同，所以旧文件不用转换 |
| MANIFEST | 没有版本号 | 早期的 `Edit` 没有 `flushed_memtable_id`，读出来当作没有 |

改格式时要遵守：

- **SST**：格式每改一次，`SST_FORMAT_VERSION` 加一，`SsTable::open` 要能读之前的每一个版本（`OLDEST_SST_FORMAT_VERSION` 到当前版本）。魔数永远不变：它只回答"这是不是本引擎的 SST"。
- **Bloom filter**：`key_hash` 与求位置的算法不能改。真要改，就得升 SST 的版本，并按版本选择算法。
- **WAL**：新格式要能读旧文件，或者在文件开头加版本号。WAL 的寿命只到对应的 memtable flush 为止，所以也可以要求升级前先正常关闭，但这要写进升级说明。
- **MANIFEST**：新加的字段必须可以省略（`#[serde(default)]`），旧的 manifest 才能读。serde 默认忽略不认识的字段，所以旧代码读新 manifest 时会静默丢掉新字段。如果新字段影响恢复的正确性，就要让旧代码拒绝打开，例如换一个新的记录变体。

## 查看文件

没有专门的转储工具。可以这样看：

- `lsm-cli` 的 `status` 命令列出各层的 SST，以及目录里的 SST 与 WAL 文件。
- 用 `xxd` 或 `hexdump -C` 看原始字节，对照本文的布局。SST 先看最后 16 字节，WAL 与 manifest 从头看。
- manifest 的 JSON 是明文，每条记录的头 8 字节是长度。可以用几行脚本按"`u64` 长度、JSON、`u32` 校验和"逐条切开打印。
