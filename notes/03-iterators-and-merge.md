# 迭代器接口与多路归并

状态：未教学

## 主要内容大纲

- 统一的迭代器接口：当前 key、当前 value、前进、是否有效。
- memtable 迭代器，以及迭代器持有它所遍历的数据。
- 多路归并：用堆选最小的 key，同一个 key 取较新的来源。
- 墓碑在哪一层被过滤掉。
- 出错之后迭代器的状态。
- 范围扫描。

## 资料

- mini-lsm 教程第一周第 2 章：[week1-02-merge-iterator.md](../../references/mini-lsm/mini-lsm-book/src/week1-02-merge-iterator.md)。
- 已学内容中相关的篇：[扫描与游标](../../storage/notes/04-btree-tree/05-scan-and-leaf-links.md)。

返回 [本组目录](README.md)。
