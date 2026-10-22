# 引擎层接入基准，与第一份范围扫描的数字

2026-10-10。要回答的问题：把多张 memtable 组织起来的引擎层，相对单张 memtable 和内存基线，写入、点查、范围扫描各是什么水平；冻结的表不落盘、越积越多时，读的代价怎样变化。

- 机器：Apple M5，10 个逻辑 CPU，32 GB 内存，macOS 26.6.2，使用电池。
- 代码状态：提交 84c081f 之上未提交的工作区。引擎层支持写入、删除、点查、范围扫描、写满冻结；冻结的 memtable 不会被写成 SST，只增不减。memtable 的游标每前进一步重新定位一次。
- 单线程。每种组合运行 3 次，下表取中位数；同一组合三次之间的浮动在一成以内。
- 负载依次执行：`fillrandom`、`overwrite`、`readrandom`、`readmissing`、`seekrandom`，共用同一个库。`seekrandom` 做 1 万次扫描，每次从一个随机的已有 key 起读 100 条。

重跑方式（在仓库根目录下）：

```sh
cargo run --release -p lsm-bench -- --target btreemap
cargo run --release -p lsm-bench -- --target memtable
cargo run --release -p lsm-bench -- --target engine --memtable-mb 64
cargo run --release -p lsm-bench -- --target engine --memtable-mb 4
```

## 吞吐

写入与点查的单位是万次操作每秒；范围扫描的单位是万次扫描每秒，每次扫描读 100 条。

| 被测对象 | 结束时的表数 | fillrandom | overwrite | readrandom | readmissing | seekrandom |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| btreemap（互斥锁加 BTreeMap） | 1 | 302 | 203 | 220 | 302 | 71.2 |
| memtable（单张跳表） | 1 | 136 | 70 | 93 | 100 | 4.97 |
| engine，memtable 上限 64 MB | 4 | 181 | 163 | 38 | 23 | 1.81 |
| engine，memtable 上限 4 MB | 56 | 386 | 376 | 4.2 | 1.6 | 0.62 |

"表数"是一张可写的加上冻结的。

## p99 延迟（微秒）

| 被测对象 | fillrandom | overwrite | readrandom | readmissing | seekrandom |
| --- | ---: | ---: | ---: | ---: | ---: |
| btreemap | 1.08 | 0.71 | 0.67 | 0.50 | 1.88 |
| memtable | 1.96 | 3.12 | 2.08 | 2.08 | 29.5 |
| engine，64 MB | 1.54 | 1.71 | 6.63 | 7.08 | 73.3 |
| engine，4 MB | 1.00 | 1.00 | 69.6 | 80.8 | 202 |

## 看到了什么

- **表越小，写得越快。** memtable 上限 4 MB 时写入约 380 万次每秒，是单张大表的近三倍，也超过了 BTreeMap。每张跳表只有三万多条记录，层数少，节点都在缓存里。
- **表越多，读得越慢。** 点查要从新到旧逐张去看：4 张表时查存在的 key 降到单张表的四成，56 张表时只剩百分之四到五。查不存在的 key 更慢，因为每一张都要看完才能确定没有：56 张表时约 61 微秒一次，平均每张表约 1.1 微秒。
- **两者是同一个旋钮的两头。** 上限调小，写入变快、读取变慢；调大则相反。这就是读放大：数据分散在越多的来源里，一次读取要看的地方越多。目前没有 flush 和 compaction，表只会越积越多，这组数字是"不整理"的代价。
- **范围扫描比点查受影响小，但绝对值低。** 单张跳表上每条记录约 200 纳秒（每步重新定位），BTreeMap 约 14 纳秒：跳表的扫描比平衡树慢一个数量级以上，其中重新定位只占两到三倍，其余是跳表逐节点追指针与每步克隆的代价。4 张表时每条约 550 纳秒，56 张表时约 1.6 微秒。

## 没有验证的地方

- 写入随表变小而变快的原因按"跳表更小、更贴近缓存"解释，没有用剖析工具确认。
- 范围扫描的代价里，重新定位、堆的调整、每步复制 key 各占多少没有拆分。
- 使用电池运行；只有单线程。

## 其中一次的原始输出：engine，memtable 上限 64 MB

## 运行条件

- 被测对象：engine
- 被测对象说明：memtable 上限 64 MB；结束时有 3 张冻结的 memtable（不落盘，只增不减）
- 参数：`--target engine --threads 1 --num 1000000 --value-size 100 --seed 1 --scan-length 100 --memtable-mb 64 --workloads fillrandom,overwrite,readrandom,readmissing,seekrandom`
- key 16 字节，value 100 字节，1 个线程；范围扫描每次读 100 条
- 编译：release，工具链 1.98.1-aarch64-apple-darwin
- 系统：macos aarch64，逻辑 CPU 10 个

## 吞吐与延迟

| 负载 | 操作数 | 耗时 (s) | 吞吐 (ops/s) | 写入 (MB/s) | p50 (µs) | p99 (µs) | p99.9 (µs) | 最大 (µs) | 查到或读到的记录 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| fillrandom | 1000000 | 0.487 | 2052362 | 227.0 | 0.42 | 1.33 | 2.46 | 53.76 | 0 |
| overwrite | 1000000 | 0.597 | 1675101 | 185.3 | 0.50 | 1.58 | 6.88 | 52.19 | 0 |
| readrandom | 1000000 | 2.485 | 402450 | — | 2.08 | 6.33 | 9.84 | 65.38 | 1000000 |
| readmissing | 1000000 | 4.389 | 227838 | — | 4.29 | 7.04 | 12.71 | 43.23 | 0 |
| seekrandom | 10000 | 0.553 | 18067 | — | 54.72 | 73.28 | 99.39 | 141.18 | 999996 |

## 放大

不适用：engine 不写磁盘，没有引擎计数器。

## 其中一次的原始输出：engine，memtable 上限 4 MB

## 运行条件

- 被测对象：engine
- 被测对象说明：memtable 上限 4 MB；结束时有 55 张冻结的 memtable（不落盘，只增不减）
- 参数：`--target engine --threads 1 --num 1000000 --value-size 100 --seed 1 --scan-length 100 --memtable-mb 4 --workloads fillrandom,overwrite,readrandom,readmissing,seekrandom`
- key 16 字节，value 100 字节，1 个线程；范围扫描每次读 100 条
- 编译：release，工具链 1.98.1-aarch64-apple-darwin
- 系统：macos aarch64，逻辑 CPU 10 个

## 吞吐与延迟

| 负载 | 操作数 | 耗时 (s) | 吞吐 (ops/s) | 写入 (MB/s) | p50 (µs) | p99 (µs) | p99.9 (µs) | 最大 (µs) | 查到或读到的记录 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| fillrandom | 1000000 | 0.259 | 3864590 | 427.5 | 0.21 | 1.00 | 1.42 | 37.09 | 0 |
| overwrite | 1000000 | 0.266 | 3755135 | 415.4 | 0.21 | 0.96 | 1.50 | 47.30 | 0 |
| readrandom | 1000000 | 23.318 | 42886 | — | 17.68 | 68.35 | 78.85 | 626.17 | 1000000 |
| readmissing | 1000000 | 60.191 | 16614 | — | 60.54 | 79.30 | 88.00 | 296.45 | 0 |
| seekrandom | 10000 | 1.598 | 6257 | — | 159.49 | 198.66 | 217.98 | 241.92 | 999996 |

## 放大

不适用：engine 不写磁盘，没有引擎计数器。
