# compaction 的执行

状态：未教学

## 主要内容大纲

- 把若干 SST 归并成新的 SST。
- 归并时哪些记录保留、哪些可以丢弃，墓碑何时能丢。
- 用于同一层内不重叠文件的串接迭代器。
- 用新文件替换旧文件的状态切换，旧文件何时删除。
- 读路径加入 L1。

## 资料

- mini-lsm 教程第二周第 1 章：[week2-01-compaction.md](../../references/mini-lsm/mini-lsm-book/src/week2-01-compaction.md)。
- 已学内容中相关的篇：[页回收与读者](../../storage/notes/05-storage-recovery/07-page-reclamation-and-readers.md)。

返回 [本组目录](README.md)。
