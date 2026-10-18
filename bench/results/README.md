# 基准结果记录

每个文件是一次运行的完整记录：驱动程序的输出，加上程序自己拿不到的环境信息。方法与指标的含义见 [基准方案与三种放大的测量](../../notes/01-benchmark-plan-and-amplification.md)。

## 怎样记一次结果

在 `lsm/` 目录下运行，输出保存为 `日期-说明.md`：

```sh
cargo run --release -p lsm-bench > bench/results/2026-10-07-btreemap-baseline.md
```

然后在文件开头手工补上：

- 标题与一句话说明：这次运行要回答什么问题，对应哪一次改动的之前或之后。
- 机器：机型、内存、磁盘类型与文件系统、是否接电源。
- 代码状态：对应的改动说明；有版本库时写提交号。
- 重复运行几次、数字浮动多大。

## 比较两次结果的条件

- 同一台机器、同样的电源状态，参数行（`--target`、`--threads`、`--num`、`--value-size`、`--seed`、`--scan-length`、`--memtable-mb`、`--block-cache-mb`、`--max-levels`、`--compaction` 与所选策略的参数、`--workloads`）除了要比较的那一项之外完全相同。
- 都是 release 编译，工具链相同。
- 差距小于同一版本重复运行的浮动（目前约一成）时，不能当作结论。

## 记录

| 文件 | 内容 |
| --- | --- |
| [2026-10-07-btreemap-baseline.md](2026-10-07-btreemap-baseline.md) | 内存基线：互斥锁保护的 `BTreeMap`，引擎出现之前的参照 |
| [2026-10-08-memtable-vs-btreemap.md](2026-10-08-memtable-vs-btreemap.md) | memtable（无锁跳表）与内存基线在 1、2、4、8 个线程下的对比 |
| [2026-10-10-engine-and-scan.md](2026-10-10-engine-and-scan.md) | 引擎层接入基准；第一份范围扫描的数字；memtable 上限 64 MB 与 4 MB 下表的张数对读写的影响 |
| [2026-10-14-disk-cache-and-range.md](2026-10-14-disk-cache-and-range.md) | SST 落盘之后：块缓存 0 / 16 / 64 MB 的对比；顺序与乱序写入下按 key 范围跳过文件的效果；与 2026-10-10 相同参数的对照；第一组写放大与读放大的计数 |
| [2026-10-14-bloom-filter.md](2026-10-14-bloom-filter.md) | Bloom filter 的前后对比：每次点查进入的 SST 与预测一致，查不存在的 key 快 19～43 倍；文件约大 1% |
| [2026-10-15-prefix-compression.md](2026-10-15-prefix-compression.md) | 块内 key 前缀压缩的前后对比：SST 小了 8%，与估算一致；吞吐的变化在浮动之内 |
| [2026-10-15-full-compaction.md](2026-10-15-full-compaction.md) | 全量 compaction 的前后对比：扫描快 7～19 倍、查不存在的 key 快 4～9 倍；整次运行的写放大 1.46～1.95；覆盖写之后的空间放大从 1.95 降到 0.98 |
| [2026-10-17-simple-leveled-compaction.md](2026-10-17-simple-leveled-compaction.md) | 简单分层的自动合并：读回到全量合并一次的水平，写放大 4～6，其中约 2 是穿过空层的复制；合并跟不上写入；扫描因多一层归并慢约三成，归并游标改成当前来源放在堆外之后恢复 |
| [2026-10-18-tiered-compaction.md](2026-10-18-tiered-compaction.md) | tiered 与简单分层的对比：整次运行的写放大从 5.4 降到 2～2.6，合并跟得上写入；读与空间取决于停下时剩几个 tier，7 个 tier 时查不存在的 key 慢一半、扫描慢到三分之一 |
