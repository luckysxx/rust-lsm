# 写路径

状态：未教学

## 主要内容大纲

- 把只读 memtable 写成 L0 的 SST。
- flush 的触发条件与后台线程。
- 新 SST 加入引擎状态的顺序与加锁。
- 用每个 SST 的首末 key 排除不相关的文件。

## 资料

- mini-lsm 教程第一周第 6 章：[week1-06-write-path.md](../../references/mini-lsm/mini-lsm-book/src/week1-06-write-path.md)。
- 已学内容中相关的篇：[提交顺序与同步](../../storage/notes/05-storage-recovery/04-commit-order-and-sync.md)。

返回 [本组目录](README.md)。
