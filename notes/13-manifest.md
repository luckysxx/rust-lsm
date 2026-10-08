# manifest

状态：未教学

## 主要内容大纲

- manifest 记录什么，怎样编码。
- flush 与 compaction 在哪一步写 manifest。
- 关闭时的 flush。
- 重启时从 manifest 恢复 SST 集合。
- 父目录同步，以及与 CURRENT 文件做法的对照。

## 资料

- mini-lsm 教程第二周第 5 章：[week2-05-manifest.md](../../references/mini-lsm/mini-lsm-book/src/week2-05-manifest.md)。
- 已学内容中相关的篇：[文件格式与元数据校验](../../storage/notes/05-storage-recovery/03-file-format-and-meta-validation.md)、[打开与故障恢复](../../storage/notes/05-storage-recovery/05-open-and-crash-recovery.md)、[提交顺序与同步](../../storage/notes/05-storage-recovery/04-commit-order-and-sync.md)。

返回 [本组目录](README.md)。
