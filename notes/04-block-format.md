# 数据块格式

状态：未教学

## 主要内容大纲

- 一条记录的编码。
- 偏移数组与块内二分查找。
- 块的大小上限，以及超过上限的单条记录。
- 块迭代器与定位到第一个不小于给定 key 的记录。
- 与 LevelDB 的 restart point 对照。

## 资料

- mini-lsm 教程第一周第 3 章：[week1-03-block.md](../../references/mini-lsm/mini-lsm-book/src/week1-03-block.md)。
- 已学内容中相关的篇：[页格式与记录定位](../../storage/notes/03-btree-pages/03-node-format-and-record-addressing.md)、[页内布局对照](../../storage/notes/06-storage-comparisons/01-page-layout-alternatives.md)。

返回 [本组目录](README.md)。
