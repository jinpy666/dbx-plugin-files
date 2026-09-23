//! files/listStream 升级决策缓存（P2）：巨型目录的二次访问免 5s 软超时直通。
//!
//! 决策的代价不对称（issue #49 设计记录）：误触发（实际不大的目录直通流式）
//! 只付一次 rclone 冷启动（~100-400ms）；漏触发（巨型目录反复等满软超时）
//! 用户每次导航都干等。因此缓存允许粗糙——记小了有软超时兜底，记大了单次
//! 付清后由完成回写自愈，不存在锁死在错误决策上的状态。
//!
//! 记忆只来自**成功完成的列举**（done 帧），失败/取消不写——失败不是关于
//! 目录大小的证据；外部变更靠 TTL 自愈，sidecar 自己的写操作走
//! [`DecisionCache::invalidate_around`] 精确失效。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 已知 ≥ 此条目数的目录直接升级 lsjson 流式（跳过 rc 快路径尝试）。
/// 与软超时 5s 的经济性对齐：S3 上 2 万条 ≈ 20 页 ≈ 1-4s，再往上用户
/// 已经在等；本地 fs 永远到不了 5s，此阈值只对慢后端有意义。
pub const ESCALATE_THRESHOLD: u64 = 20_000;

/// LRU 容量：双栏浏览的活跃目录集远小于此。
const CAPACITY: usize = 256;
/// 记忆寿命：外部写（其他客户端/宿主）造成的目录变化最多污染 5 分钟。
const TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy)]
struct Entry {
    count: u64,
    at: Instant,
}

#[derive(Default)]
pub struct DecisionCache {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<(String, String), Entry>,
    /// LRU 淘汰序：命中/写入都把 key 挪到队尾，队首最旧。
    order: VecDeque<(String, String)>,
}

/// 列举键的父目录（`/a/b` → `/a`，`/a` → `/`，`/` → `/`）。
/// 写操作失效钩子用：在 D 内增删条目改变的是 D 的列表计数。
pub fn parent_of(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
        None => "/".to_string(),
    }
}

impl DecisionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` = 记忆中的巨型目录，跳过 rc 快路径直通流式。
    /// miss/过期/未达阈值一律 `false`（保守：软超时兜底）。
    pub fn escalate_hint(&self, connection_id: &str, path: &str) -> bool {
        let mut inner = self.inner.lock().expect("decision cache poisoned");
        let key = (connection_id.to_string(), path.to_string());
        let Some(entry) = inner.map.get(&key).copied() else {
            return false;
        };
        if entry.at.elapsed() > TTL {
            inner.map.remove(&key);
            inner.order.retain(|k| *k != key);
            return false;
        }
        touch(&mut inner, key);
        entry.count >= ESCALATE_THRESHOLD
    }

    /// 成功完成的列举回写（done 帧的全量计数）。任何计数都记录——
    /// 「已知不大」的记忆同样消除下一次的决策犹豫成本。
    pub fn record(&self, connection_id: &str, path: &str, count: u64) {
        let mut inner = self.inner.lock().expect("decision cache poisoned");
        let key = (connection_id.to_string(), path.to_string());
        inner.map.insert(key.clone(), Entry { count, at: Instant::now() });
        touch(&mut inner, key);
        while inner.map.len() > CAPACITY {
            evict_oldest(&mut inner);
        }
    }

    /// 精确失效（单 key）。
    pub fn invalidate(&self, connection_id: &str, path: &str) {
        let mut inner = self.inner.lock().expect("decision cache poisoned");
        let key = (connection_id.to_string(), path.to_string());
        inner.map.remove(&key);
        inner.order.retain(|k| *k != key);
    }

    /// 写操作波及面：被列举目录是操作路径的**父目录**（其中条目增删），
    /// 操作路径自身的子树计数同样可能变化（purge/rename 落点）。
    pub fn invalidate_around(&self, connection_id: &str, path: &str) {
        self.invalidate(connection_id, path);
        self.invalidate(connection_id, &parent_of(path));
    }
}

fn touch(inner: &mut Inner, key: (String, String)) {
    inner.order.retain(|k| *k != key);
    inner.order.push_back(key);
}

fn evict_oldest(inner: &mut Inner) {
    if let Some(oldest) = inner.order.pop_front() {
        inner.map.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_hint_only_for_known_huge_dirs() {
        let cache = DecisionCache::new();
        assert!(!cache.escalate_hint("c1", "/a"), "miss is conservative");
        cache.record("c1", "/a", ESCALATE_THRESHOLD - 1);
        assert!(!cache.escalate_hint("c1", "/a"), "below threshold stays on the rc fast path");
        cache.record("c1", "/a", ESCALATE_THRESHOLD);
        assert!(cache.escalate_hint("c1", "/a"));
        cache.record("c1", "/a", 0);
        assert!(!cache.escalate_hint("c1", "/a"), "empty dir memory demotes");
    }

    #[test]
    fn memory_is_per_connection_and_path() {
        let cache = DecisionCache::new();
        cache.record("c1", "/a", ESCALATE_THRESHOLD);
        assert!(!cache.escalate_hint("c2", "/a"));
        assert!(!cache.escalate_hint("c1", "/b"));
        assert!(cache.escalate_hint("c1", "/a"));
    }

    #[test]
    fn ttl_expiry_falls_back_to_conservative() {
        let cache = DecisionCache::new();
        cache.record("c1", "/a", ESCALATE_THRESHOLD);
        // 直接拨表：把 at 改到 TTL 之前（避免 sleep 300s）。
        let key = ("c1".to_string(), "/a".to_string());
        {
            let mut inner = cache.inner.lock().unwrap();
            let entry = inner.map.get_mut(&key).unwrap();
            entry.at = Instant::now() - TTL - Duration::from_secs(1);
        }
        assert!(!cache.escalate_hint("c1", "/a"), "expired memory demotes");
        // 过期项顺手清除。
        assert!(!cache.inner.lock().unwrap().map.contains_key(&key));
    }

    #[test]
    fn lru_evicts_oldest_beyond_capacity() {
        let cache = DecisionCache::new();
        for index in 0..CAPACITY {
            cache.record("c1", &format!("/d{index}"), ESCALATE_THRESHOLD);
        }
        // 命中 /d0（挪到队尾），随后写入新 key 淘汰最旧的 /d1。
        assert!(cache.escalate_hint("c1", "/d0"));
        cache.record("c1", "/new", ESCALATE_THRESHOLD);
        assert!(!cache.escalate_hint("c1", "/d1"), "oldest unaccessed entry evicted");
        assert!(cache.escalate_hint("c1", "/d0"), "recently touched entry survives");
        assert_eq!(cache.inner.lock().unwrap().map.len(), CAPACITY);
    }

    #[test]
    fn invalidate_and_invalidate_around() {
        let cache = DecisionCache::new();
        cache.record("c1", "/a", ESCALATE_THRESHOLD);
        cache.record("c1", "/a/b", ESCALATE_THRESHOLD);
        cache.invalidate("c1", "/a");
        assert!(!cache.escalate_hint("c1", "/a"));
        assert!(cache.escalate_hint("c1", "/a/b"));

        // 在 /a/b 里写文件：波及 /a（父目录列表变化）与 /a/b 自身。
        cache.invalidate_around("c1", "/a/b/c.txt");
        assert!(!cache.escalate_hint("c1", "/a"), "parent listing changed");
        assert!(!cache.escalate_hint("c1", "/a/b"), "subtree count changed");
    }

    #[test]
    fn parent_of_covers_root_and_depth() {
        assert_eq!(parent_of("/"), "/");
        assert_eq!(parent_of("/a"), "/");
        assert_eq!(parent_of("/a/"), "/");
        assert_eq!(parent_of("/a/b"), "/a");
        assert_eq!(parent_of("/a/b/c"), "/a/b");
        assert_eq!(parent_of("relative"), "/");
    }
}
