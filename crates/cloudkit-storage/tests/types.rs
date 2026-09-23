//! 词汇类型语义测试（interfaces §5 / foundation D6-D3-D2）。

use std::str::FromStr;
use std::time::Duration;

use cloudkit_storage::transport::CloudTransport;
use cloudkit_storage::{
    BackendHandle, Capabilities, EntryId, EntryKind, Page, PageCursor, Quota, Range, RelPath,
    StorageError, VolumeId, WriteHint,
};

// --- RelPath ---------------------------------------------------------------

#[test]
fn relpath_accepts_valid_paths() {
    for ok in ["a", "a/b", "a/b/c.txt", "dir with space/f", "中文/文件"] {
        let p = RelPath::new(ok).unwrap_or_else(|e| panic!("{ok} 应合法: {e}"));
        assert_eq!(p.as_str(), ok);
    }
}

#[test]
fn relpath_root_is_empty_string_and_displays_slash() {
    let root = RelPath::root();
    assert!(root.is_root());
    assert_eq!(root.as_str(), "");
    assert_eq!(root.to_string(), "/");
    assert_eq!(RelPath::new("").unwrap(), root);
}

#[test]
fn relpath_rejects_invalid_forms() {
    for bad in [
        "/", "/a", "a/", "a//b", "..", ".", "./a", "a/./b", "a/../b", "a\\b", "a\0b",
    ] {
        assert_eq!(
            RelPath::new(bad),
            Err(StorageError::Invalid),
            "应拒绝: {bad:?}"
        );
    }
}

#[test]
fn relpath_parent_file_name_components() {
    let p = RelPath::new("a/b/c.txt").unwrap();
    assert_eq!(p.parent(), Some(RelPath::new("a/b").unwrap()));
    assert_eq!(p.file_name(), Some("c.txt"));
    assert_eq!(p.components().collect::<Vec<_>>(), vec!["a", "b", "c.txt"]);
    assert_eq!(
        RelPath::new("a").unwrap().parent(),
        Some(RelPath::root()),
        "顶层组件的父是根"
    );
    assert_eq!(RelPath::root().parent(), None);
    assert_eq!(RelPath::root().file_name(), None);
}

#[test]
fn relpath_join_and_join_validation() {
    let base = RelPath::new("a/b").unwrap();
    assert_eq!(base.join("c").unwrap(), RelPath::new("a/b/c").unwrap());
    assert_eq!(
        RelPath::root().join("top").unwrap(),
        RelPath::new("top").unwrap(),
        "根 join 不产生前导空段"
    );
    for bad in ["", ".", "..", "x/y"] {
        assert_eq!(
            base.join(bad),
            Err(StorageError::Invalid),
            "join 应拒绝: {bad:?}"
        );
    }
}

#[test]
fn relpath_ord_is_lexicographic() {
    let mut v = [
        RelPath::new("b").unwrap(),
        RelPath::new("a/x").unwrap(),
        RelPath::new("a").unwrap(),
        RelPath::root(),
    ];
    v.sort();
    let strs: Vec<&str> = v.iter().map(|p| p.as_str()).collect();
    assert_eq!(strs, ["", "a", "a/x", "b"]);
}

// --- VolumeId / EntryId ------------------------------------------------------

#[test]
fn volume_id_parses_known_forms() {
    for (text, scheme, key) in [
        ("telegram:cydrive-main", "telegram", "cydrive-main"),
        ("baidu:123456", "baidu", "123456"),
        // local 的 key 含盘符冒号：首个 ':' 之后整体为 key
        ("local:E:/data/root", "local", "E:/data/root"),
        ("115:whatever_x", "115", "whatever_x"),
    ] {
        let v = VolumeId::parse(text).unwrap_or_else(|e| panic!("{text} 应合法: {e}"));
        assert_eq!(v.scheme(), scheme, "{text} scheme");
        assert_eq!(v.key(), key, "{text} key");
        assert_eq!(v.to_string(), text, "{text} display 往返");
        assert_eq!(
            VolumeId::from_str(text).unwrap(),
            v,
            "FromStr 与 parse 一致"
        );
    }
}

#[test]
fn volume_id_new_validates_parts() {
    assert!(VolumeId::new("baidu", "42").is_ok());
    assert_eq!(
        VolumeId::new("", "42"),
        Err(StorageError::Invalid),
        "空 scheme"
    );
    assert_eq!(
        VolumeId::new("baidu", ""),
        Err(StorageError::Invalid),
        "空 key"
    );
    assert_eq!(
        VolumeId::new("bad scheme", "42"),
        Err(StorageError::Invalid),
        "scheme 含空格"
    );
}

#[test]
fn volume_id_rejects_malformed_text() {
    for bad in ["", "nocolon", ":key", "scheme:"] {
        assert_eq!(
            VolumeId::parse(bad),
            Err(StorageError::Invalid),
            "应拒绝: {bad:?}"
        );
    }
}

#[test]
fn volume_id_serde_roundtrip_is_plain_string() {
    let v = VolumeId::parse("baidu:42").unwrap();
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, "\"baidu:42\"");
    let back: VolumeId = serde_json::from_str(&json).unwrap();
    assert_eq!(back, v);
}

#[test]
fn entry_id_handle_is_string_on_the_wire() {
    // interfaces §5：ID 类字符串禁止经 JSON float（PCFS %.0f 教训）
    let id = EntryId::new(
        VolumeId::parse("baidu:1").unwrap(),
        BackendHandle::new("671337245231660"),
    );
    let json = serde_json::to_value(&id).unwrap();
    assert!(
        json["handle"].is_string(),
        "handle 必须序列化为字符串: {json}"
    );
    assert_eq!(json["handle"], serde_json::json!("671337245231660"));
    let back: EntryId = serde_json::from_value(json).unwrap();
    assert_eq!(back, id);
    assert_eq!(id.to_string(), "baidu:1/671337245231660");
}

// --- Range -------------------------------------------------------------------

#[test]
fn range_validates_half_open_invariant() {
    assert!(Range::new(5, Some(5)).is_ok(), "空窗口 [5,5) 合法");
    assert!(Range::new(5, Some(9)).is_ok());
    assert_eq!(
        Range::new(9, Some(5)),
        Err(StorageError::Invalid),
        "start > end 非法"
    );
    assert!(Range::new(0, None).is_ok());
}

#[test]
fn range_clamped_len_semantics() {
    let size = 100u64;
    assert_eq!(Range::new(10, Some(20)).unwrap().clamped_len(size), 10);
    assert_eq!(
        Range::new(10, None).unwrap().clamped_len(size),
        90,
        "开放区间到 EOF"
    );
    assert_eq!(
        Range::new(10, Some(500)).unwrap().clamped_len(size),
        90,
        "end 越界钳制到 EOF"
    );
    assert_eq!(
        Range::new(100, None).unwrap().clamped_len(size),
        0,
        "start == size 为空"
    );
    assert_eq!(
        Range::new(150, Some(200)).unwrap().clamped_len(size),
        0,
        "start 超过 size 为空"
    );
    assert_eq!(Range::new(0, Some(0)).unwrap().clamped_len(size), 0);
}

// --- Page / WriteHint / Quota -------------------------------------------------

#[test]
fn page_defaults_and_serde() {
    let p = Page::default();
    assert_eq!(p.cursor, PageCursor::Start);
    let json = serde_json::to_string(&Page {
        limit: 3,
        cursor: PageCursor::Start,
    })
    .unwrap();
    assert!(json.contains("\"limit\":3"));
    let back: Page = serde_json::from_str(&json).unwrap();
    assert_eq!(back.limit, 3);
}

#[test]
fn write_hint_defaults_are_all_neutral() {
    let h = WriteHint::default();
    assert_eq!(h.size, None);
    assert_eq!(h.content_hash, None);
    assert!(!h.rapid_upload);
    // interfaces §4：可选字段 None 时不出现在 wire 上
    let json = serde_json::to_string(&h).unwrap();
    assert_eq!(json, "{}", "空提示序列化为空对象");
}

#[test]
fn quota_available_saturates() {
    let q = Quota {
        total: Some(10),
        used: 30,
    };
    assert_eq!(q.available(), 0);
    let unknown = Quota {
        total: None,
        used: 5,
    };
    assert_eq!(unknown.available(), 0, "total 未知时 available 报 0");
}

// --- Capabilities -------------------------------------------------------------

#[test]
fn capabilities_default_is_none() {
    let c = Capabilities::none();
    assert!(c.is_empty());
    assert!(!c.range_read && !c.resume && !c.multipart && !c.server_side_move);
    assert!(!c.rapid_upload && !c.authoritative_index && !c.change_feed && !c.inbound && !c.chat);
    assert!(!c.remote_delete);
}

#[test]
fn capabilities_contains_is_reflexive_and_subset() {
    let all_on = Capabilities {
        range_read: true,
        resume: true,
        multipart: true,
        server_side_move: true,
        rapid_upload: true,
        authoritative_index: true,
        change_feed: true,
        inbound: true,
        chat: true,
        remote_delete: true,
    };
    assert!(all_on.contains(&all_on), "自反");
    assert!(all_on.contains(&Capabilities::none()), "全集包含空集");
    assert!(!Capabilities::none().contains(&all_on), "空集不包含全集");
    let partial = Capabilities {
        range_read: true,
        ..Capabilities::none()
    };
    assert!(all_on.contains(&partial));
    assert!(!partial.contains(&all_on));
    assert!(
        !partial.contains(&Capabilities {
            resume: true,
            ..Capabilities::none()
        }),
        "互不包含"
    );
}

// --- StorageError -------------------------------------------------------------

#[test]
fn storage_error_equality_and_display() {
    assert_eq!(
        StorageError::Unauthorized { recoverable: true },
        StorageError::Unauthorized { recoverable: true }
    );
    assert_ne!(
        StorageError::Unauthorized { recoverable: true },
        StorageError::Unauthorized { recoverable: false }
    );
    assert_eq!(
        StorageError::RateLimited {
            retry_after: Some(Duration::from_secs(42))
        },
        StorageError::RateLimited {
            retry_after: Some(Duration::from_secs(42))
        }
    );
    assert_eq!(
        StorageError::Unavailable("baidu errno -6".into()).to_string(),
        "backend unavailable: baidu errno -6",
        "未知错误载荷保留原始码"
    );
    assert!(!StorageError::QuotaExceeded.to_string().is_empty());
}

// --- EntryKind -----------------------------------------------------------------

#[test]
fn entry_kind_serde_lowercase() {
    assert_eq!(serde_json::to_string(&EntryKind::File).unwrap(), "\"file\"");
    assert_eq!(serde_json::to_string(&EntryKind::Dir).unwrap(), "\"dir\"");
}

// --- CloudTransport::as_driver probe（Phase 8 / D1）-----------------------------

/// 探针默认形态（[`CloudTransport::as_inbound`] 同款先例）：窄面
/// transport（telegram 形态）默认不持有宽面——消费方拿 `None` 只降级，
/// 绝不 panic。宽面 `Some` 腿由各驱动 crate 的 transport_face 测试钉
/// （ck-local `as_driver_probe_reports_the_wide_face`）。
#[test]
fn as_driver_probe_defaults_to_none_and_reports_the_wide_face() {
    let t = cloudkit_storage::transport::mock::MockTransport::builder().build();
    assert!(
        t.as_driver().is_none(),
        "窄面 transport 默认不持有宽面（telegram 零变化）"
    );
}
