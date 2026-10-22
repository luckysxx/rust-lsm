# 事务工作区与原子提交

状态：未教学

## 主要内容大纲

- 事务私有的工作区，put 与 delete 先写在其中。
- 事务内的点查与扫描：先看自己的写入。
- 提交。
- 一个事务的全部写入原子地进入 WAL。

## 资料

- mini-lsm 教程第三周第 5 章：[week3-05-txn-occ.md](https://github.com/skyzh/mini-lsm/blob/v202608/mini-lsm-book/src/week3-05-txn-occ.md)。
- 已学内容中相关的篇：存储笔记[原子写批次](https://github.com/luckysxx/go-btree-kv/blob/main/notes/05-storage-recovery/06-atomic-write-batches.md)。

返回 [本组目录](README.md)。
