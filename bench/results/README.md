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

- 同一台机器、同样的电源状态，参数行（`--target`、`--threads`、`--num`、`--value-size`、`--seed`、`--scan-length`、`--memtable-mb`、`--block-cache-mb`、`--workloads`）除了要比较的那一项之外完全相同。
- 都是 release 编译，工具链相同。
- 差距小于同一版本重复运行的浮动（目前约一成）时，不能当作结论。

## 记录

| 文件 | 内容 |
| --- | --- |
| [2026-10-07-btreemap-baseline.md](2026-10-07-btreemap-baseline.md) | 内存基线：互斥锁保护的 `BTreeMap`，引擎出现之前的参照 |
| [2026-10-08-memtable-vs-btreemap.md](2026-10-08-memtable-vs-btreemap.md) | memtable（无锁跳表）与内存基线在 1、2、4、8 个线程下的对比 |
| [2026-10-10-engine-and-scan.md](2026-10-10-engine-and-scan.md) | 引擎层接入基准；第一份范围扫描的数字；memtable 上限 64 MB 与 4 MB 下表的张数对读写的影响 |
| [2026-10-14-disk-cache-and-range.md](2026-10-14-disk-cache-and-range.md) | SST 落盘之后：块缓存 0 / 16 / 64 MB 的对比；顺序与乱序写入下按 key 范围跳过文件的效果；与 2026-10-10 相同参数的对照；第一组写放大与读放大的计数 |
