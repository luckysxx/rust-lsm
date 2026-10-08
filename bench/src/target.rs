//! 被测对象。基准驱动只通过 `KvTarget` 操作它，换一个实现就能在相同负载下对照。

use std::collections::BTreeMap;
use std::sync::Mutex;

use lsm::stats::StatsSnapshot;

pub trait KvTarget {
    /// 写进报告里的名字。
    fn name(&self) -> &'static str;

    fn put(&self, key: &[u8], value: &[u8]);

    /// 返回 value 的副本；key 不存在时返回 `None`。
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;

    /// 引擎内部的计数器。不写磁盘的对象没有，返回 `None`。
    fn stats(&self) -> Option<StatsSnapshot> {
        None
    }
}

/// 内存基线：一把互斥锁保护的 `BTreeMap`。
///
/// 没有 WAL、没有文件，重启即丢。它给出"只在内存里维护一张有序表"的
/// 吞吐与延迟，引擎的数字与它相比，差出来的部分就是持久化与分层的代价。
#[derive(Default)]
pub struct BTreeMapTarget {
    map: Mutex<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl BTreeMapTarget {
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvTarget for BTreeMapTarget {
    fn name(&self) -> &'static str {
        "btreemap"
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        let mut map = self.map.lock().unwrap();
        map.insert(key.to_vec(), value.to_vec());
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let map = self.map.lock().unwrap();
        map.get(key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btreemap_target_reads_back_latest_value() {
        let target = BTreeMapTarget::new();

        assert_eq!(target.get(b"k"), None);

        target.put(b"k", b"v1");
        target.put(b"k", b"v2");

        assert_eq!(target.get(b"k"), Some(b"v2".to_vec()));
        assert_eq!(target.stats(), None);
    }
}
