# 提交时间戳与多版本

状态：未教学

## 主要内容大纲

- 写入时分配提交时间戳。
- memtable 与 WAL 中的时间戳。
- compaction 保留同一个 key 的全部版本。
- 迭代器只返回每个 key 的最新版本。

## 资料

- mini-lsm 教程第三周第 2 章：[week3-02-snapshot-read-part-1.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week3-02-snapshot-read-part-1.md)。
- 已学内容中相关的篇：存储笔记《MVCC 与版本回收》。

返回 [本组目录](README.md)。
