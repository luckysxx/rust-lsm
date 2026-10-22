# watermark 与旧版本回收

状态：未教学

## 主要内容大纲

- watermark：仍在进行的读事务中最小的读时间戳。
- 事务开始与结束时维护 watermark。
- compaction 时回收 watermark 以下不再需要的旧版本。

## 资料

- mini-lsm 教程第三周第 4 章：[week3-04-watermark.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week3-04-watermark.md)。
- 已学内容中相关的篇：存储笔记[页回收与读者](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/07-page-reclamation-and-readers.md)、[MVCC 与版本回收](https://github.com/luckysxx/go-btree-kv/blob/main/notes/06-storage-comparisons/04-mvcc-and-version-reclamation.md)。

返回 [本组目录](README.md)。
