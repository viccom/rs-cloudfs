//! 上传会话磁盘层（L2 共享机制件——内存优先 + 可选磁盘回填）。
//!
//! **契约来源**：ck-baidu/ck-pan115/ck-pan123 的 `SessionStore` 磁盘机制逐
//! 形（架构审查 D4 项③）：`<root>/<ns>/sessions/<digest>.json` 落盘 +
//! tmp+rename 原子写 + 读回填。**键构造各驱动自留**（baidu `path|size`
//! 两元是协议决定——precreate 时刻 block_list 可能不全，见各驱动
//! upload.rs）——本件只收「给定 key 字符串 → 落盘/读回」的机制。
//!
//! **tmp 命名统一为 `pid+seq`**（baidu 现形态最正确：同进程并发写同
//! key 不互踩；pan115/pan123 的裸 `json.tmp` 有该隐患——统一即既修
//! 隐患，非纯搬运）。落盘文件名摘要算法由调用方注入（各驱动沿用自家
//! hash——文件名不跨驱动共享，无兼容问题）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// 进程内 tmp 序号（与 pid 共同保证同 key 并发写的 tmp 名唯一）。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 会话磁盘层（内存 map 由调用方持有——本件只管磁盘 IO）。
///
/// 用法：调用方给出 `<root>/<ns>` 的根与命名空间（如 `pan115_state`），
/// 以及对 key 做摘要的函数；本件据此落盘/读回。内存 map 的读写、key 的
/// 构造、record 的序列化都由调用方负责——本件不含任何协议面。
#[derive(Debug, Clone)]
pub struct SessionDiskStore {
    /// 磁盘层根（`None` = 纯内存形态，本件全程短路）。
    root: Option<PathBuf>,
    /// 命名空间子目录（如 `pan115_state`/`pan123_state`/`baidu_state`）。
    namespace: &'static str,
}

impl SessionDiskStore {
    /// 构造（`root = None` → 纯内存形态，所有磁盘操作短路）。
    pub fn new(root: Option<PathBuf>, namespace: &'static str) -> Self {
        SessionDiskStore { root, namespace }
    }

    /// key 摘要 → 落盘文件路径（`<root>/<ns>/sessions/<digest>.json`）；
    /// 纯内存形态返回 `None`。
    pub fn file_path(&self, digest_hex: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|root| {
            root.join(self.namespace)
                .join("sessions")
                .join(format!("{digest_hex}.json"))
        })
    }

    /// 读回会话正文（miss/解析失败 → `None`；纯内存形态 → `None`）。
    pub async fn read(&self, digest_hex: &str) -> Option<String> {
        let file = self.file_path(digest_hex)?;
        tokio::fs::read_to_string(&file).await.ok()
    }

    /// 原子写会话正文（同目录 tmp + rename；tmp 名带 pid+seq 防同进程
    /// 并发同 key 互踩）。写失败静默（会话是可重建资产——落盘失败不
    /// 阻塞上传，与三驱动原实现的 `let _ =` 形态逐形）。
    pub async fn write(&self, digest_hex: &str, body: &str) {
        let Some(file) = self.file_path(digest_hex) else {
            return;
        };
        if let Some(parent) = file.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let tmp = file.with_extension(format!(
            "json.tmp-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        if tokio::fs::write(&tmp, body).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &file).await;
        }
    }

    /// 删除会话文件（幂等；纯内存形态短路）。
    pub async fn remove(&self, digest_hex: &str) {
        if let Some(file) = self.file_path(digest_hex) {
            let _ = tokio::fs::remove_file(&file).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn write_then_read_roundtrips_through_the_disk_layer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionDiskStore::new(Some(dir.path().to_path_buf()), "pan115_state");
        store.write("deadbeef", r#"{"a":1}"#).await;
        assert_eq!(store.read("deadbeef").await.as_deref(), Some(r#"{"a":1}"#));
        // 落盘路径形态：`<root>/pan115_state/sessions/deadbeef.json`。
        assert!(dir
            .path()
            .join("pan115_state/sessions/deadbeef.json")
            .exists());
    }

    #[tokio::test]
    async fn remove_is_idempotent_and_clears_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionDiskStore::new(Some(dir.path().to_path_buf()), "baidu_state");
        store.write("cafe", "x").await;
        store.remove("cafe").await;
        assert_eq!(store.read("cafe").await, None);
        store.remove("cafe").await; // 幂等：再删不炸
    }

    #[tokio::test]
    async fn in_memory_form_short_circuits_all_disk_io() {
        let store = SessionDiskStore::new(None, "pan123_state");
        assert_eq!(store.file_path("abc"), None);
        store.write("abc", "y").await;
        assert_eq!(store.read("abc").await, None);
        store.remove("abc").await;
    }

    #[tokio::test]
    async fn concurrent_writes_to_the_same_key_use_distinct_tmp_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionDiskStore::new(Some(dir.path().to_path_buf()), "pan115_state");
        // 并发同 key 写：pid+seq 的 tmp 名保证互不覆盖（裸 json.tmp 会互踩）。
        let (a, b) = (store.clone(), store.clone());
        let h1 = tokio::spawn(async move { a.write("same", "1").await });
        let h2 = tokio::spawn(async move { b.write("same", "2").await });
        h1.await.unwrap();
        h2.await.unwrap();
        // 终态是二者之一（rename 后唯一文件）；tmp 残留为零。
        assert!(matches!(
            store.read("same").await.as_deref(),
            Some("1" | "2")
        ));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("pan115_state/sessions"))
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "tmp 不应残留: {leftovers:?}");
    }
}
