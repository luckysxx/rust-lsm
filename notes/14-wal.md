# WAL

状态：未教学

## 主要内容大纲

- WAL 记录的编码。
- 每个 memtable 对应一个 WAL 文件。
- 重启时重放 WAL 重建 memtable。
- 同步策略，以及与 group commit 的对照。
- WAL 何时可以删除。

## 资料

- mini-lsm 教程第二周第 6 章：[week2-06-wal.md](../../references/mini-lsm/mini-lsm-book/src/week2-06-wal.md)。
- 已学内容中相关的篇：[提交顺序与同步](../../storage/notes/05-storage-recovery/04-commit-order-and-sync.md)、[打开与故障恢复](../../storage/notes/05-storage-recovery/05-open-and-crash-recovery.md)。

返回 [本组目录](README.md)。
