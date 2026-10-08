# 读路径

状态：未教学

## 主要内容大纲

- 把内存来源与磁盘来源并成一个有序视图的两路归并。
- 范围扫描跨越 memtable 与 L0 的 SST。
- 点查依次查找各个来源，查到值或墓碑即停。
- 读操作怎样取得一份不变的引擎状态。

## 资料

- mini-lsm 教程第一周第 5 章：[week1-05-read-path.md](../../references/mini-lsm/mini-lsm-book/src/week1-05-read-path.md)。
- 已学内容中相关的篇：[根查找](../../storage/notes/04-btree-tree/02-root-and-tree-search.md)、[页回收与读者](../../storage/notes/05-storage-recovery/07-page-reclamation-and-readers.md)。

返回 [本组目录](README.md)。
