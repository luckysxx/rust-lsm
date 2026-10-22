# 快照读

状态：未教学

## 主要内容大纲

- 带读时间戳的迭代器：只看不晚于读时间戳的版本。
- 多版本下的点查与范围扫描。
- SST 记录自己包含的最大时间戳。
- 重启后恢复提交时间戳。

## 资料

- mini-lsm 教程第三周第 3 章：[week3-03-snapshot-read-part-2.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week3-03-snapshot-read-part-2.md)。
- 已学内容中相关的篇：存储笔记[页回收与读者](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/07-page-reclamation-and-readers.md)。

返回 [本组目录](README.md)。
