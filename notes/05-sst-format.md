# SST 格式

状态：未教学

## 主要内容大纲

- SST 的组成：数据块、每个块的元信息、文件末尾的定位信息。
- 构建 SST：何时结束一个块、何时写出文件。
- SST 迭代器：先用元信息找到块，再在块内查找。
- 按需读块与 block cache。
- 读块的方式：`pread` 加 block cache 与 mmap 的取舍。

## 资料

- mini-lsm 教程第一周第 4 章：[week1-04-sst.md](../../references/mini-lsm/mini-lsm-book/src/week1-04-sst.md)。
- 已学内容中相关的篇：[文件格式与元数据校验](../../storage/notes/05-storage-recovery/03-file-format-and-meta-validation.md)、[页缓存与 buffer pool](../../storage/notes/02-cache-pager/07-page-cache-versus-buffer-pool.md)。

返回 [本组目录](README.md)。
