# memtable 与跳表

状态：未教学

## 主要内容大纲

- 并发有序表的选择：跳表与其他结构。
- put、get、delete，以及墓碑在内存中的表示。
- 近似大小与冻结的触发条件。
- 可写 memtable 与只读 memtable 的切换，切换时的加锁。
- 点查从新到旧依次查找各个 memtable。

## 资料

- mini-lsm 教程第一周第 1 章：[week1-01-memtable.md](../../references/mini-lsm/mini-lsm-book/src/week1-01-memtable.md)。
- 已学内容中相关的篇：[原子写批次](../../storage/notes/05-storage-recovery/06-atomic-write-batches.md)、[单机 KV API](../../storage/notes/05-storage-recovery/08-single-node-kv-api.md)。

返回 [本组目录](README.md)。
