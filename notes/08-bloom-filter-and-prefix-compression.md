# Bloom filter 与 key 前缀压缩

状态：未教学

## 主要内容大纲

- Bloom filter 的原理，位数、哈希个数与误判率。
- Bloom filter 存进 SST，并用在点查路径上。
- 块内 key 的前缀压缩与解码。
- 两项改动在基准中的前后对比。

## 资料

- mini-lsm 教程第一周第 7 章：[week1-07-sst-optimizations.md](../../references/mini-lsm/mini-lsm-book/src/week1-07-sst-optimizations.md)。
- 已学内容中相关的篇：[长 key 与分隔键](../../storage/notes/06-storage-comparisons/02-large-keys-and-separators.md)。

返回 [本组目录](README.md)。
