# 带时间戳的 key

状态：未教学

## 主要内容大纲

- key 带上时间戳之后的排序规则：同一个 key 的新版本排在前面。
- 数据块与 SST 中时间戳的编码。
- 迭代器、memtable 与读路径的相应改动。

## 资料

- mini-lsm 教程第三周第 1 章：[week3-01-ts-key-refactor.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week3-01-ts-key-refactor.md)。
- 已学内容中相关的篇：存储笔记《MVCC 与版本回收》。

返回 [本组目录](README.md)。
