# 写批次与校验和

状态：未教学

## 主要内容大纲

- 写批次接口：一批写入是 WAL 里的一条记录（[L14](14-wal.md#文件格式) 的里层改成放多对 key 与 value），一起成功或一起失败。
- group commit：几个并发写者的批次合成一次追加、一次同步（RocksDB 的写线程队列）；单线程每次同步时每秒只有几百次（[L14 的基准](../bench/results/2026-10-21-wal.md)）。
- 数据块、SST 元信息、WAL、manifest 各自的校验和。
- 校验失败与文件残尾的处理：WAL 末尾容忍、中间报错之外的几种恢复方式（RocksDB 的 `WALRecoveryMode`，如停在第一处损坏）；断电时不同步的尾部可能不是一个完整的前缀。

## 资料

- mini-lsm 教程第二周第 7 章：[week2-07-snacks.md](../../references/mini-lsm/mini-lsm-book/src/week2-07-snacks.md)。
- 已学内容中相关的篇：[原子写批次](../../storage/notes/05-storage-recovery/06-atomic-write-batches.md)、[文件格式与元数据校验](../../storage/notes/05-storage-recovery/03-file-format-and-meta-validation.md)。

返回 [本组目录](README.md)。
