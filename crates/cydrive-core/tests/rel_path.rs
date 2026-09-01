//! RED-phase tests for `cydrive_core::rel_path`.
//!
//! Contract under test: a virtual path always starts with `/`, uses `/` as
//! the sole separator, directories are plain records (no trailing slash),
//! and pollution (`..`, `.`, empty segments, `\`, NUL) is rejected at the
//! type level — the Python version performs no validation at all.

use std::collections::HashSet;
use std::str::FromStr;

use cydrive_core::rel_path::{PathError, RelPath};

fn rp(s: &str) -> RelPath {
    RelPath::new(s).expect("expected a valid virtual path")
}

// ---------------------------------------------------------------- valid ---

#[test]
fn root_is_valid_and_has_root_semantics() {
    let root = rp("/");
    assert!(root.is_root());
    assert_eq!(root.as_str(), "/");
    assert_eq!(root.name(), "");
    assert_eq!(root.parent(), None);
}

#[test]
fn plain_single_segment_is_valid() {
    let p = rp("/a");
    assert!(!p.is_root());
    assert_eq!(p.as_str(), "/a");
    assert_eq!(p.name(), "a");
    assert_eq!(p.parent().map(|x| x.as_str().to_string()), Some("/".into()));
}

#[test]
fn multi_level_path_is_valid() {
    let p = rp("/a/b/c.txt");
    assert_eq!(p.as_str(), "/a/b/c.txt");
    assert_eq!(p.name(), "c.txt");
    assert_eq!(
        p.parent().map(|x| x.as_str().to_string()),
        Some("/a/b".into())
    );
}

#[test]
fn unicode_and_emoji_segments_are_valid() {
    let p = rp("/数据/📁文件.txt");
    assert_eq!(p.name(), "📁文件.txt");
    assert_eq!(
        p.parent().map(|x| x.as_str().to_string()),
        Some("/数据".into())
    );
}

// -------------------------------------------------------------- rejects ---

fn assert_rejected(s: &str) {
    assert!(
        matches!(RelPath::new(s), Err(PathError::Invalid(_))),
        "expected `{s}` to be rejected"
    );
}

#[test]
fn rejects_missing_leading_slash() {
    assert_rejected("a/b");
    assert_rejected("a");
}

#[test]
fn rejects_empty_string() {
    assert_rejected("");
}

#[test]
fn rejects_empty_segments() {
    assert_rejected("//");
    assert_rejected("/a//b");
    assert_rejected("///a");
}

#[test]
fn rejects_trailing_slash() {
    assert_rejected("/a/");
    assert_rejected("/a/b/");
}

#[test]
fn rejects_dot_and_dotdot_segments() {
    assert_rejected("/..");
    assert_rejected("/.");
    assert_rejected("/a/../b");
    assert_rejected("/a/./b");
    assert_rejected("/a/b/..");
    assert_rejected("/a/b/.");
}

#[test]
fn rejects_backslash_and_nul() {
    assert_rejected("/a\\b");
    assert_rejected("/a/b\\c");
    assert_rejected("/a\0b");
    assert_rejected("/\0");
}

// ----------------------------------------------------------------- join ---

#[test]
fn join_appends_a_single_segment() {
    assert_eq!(rp("/a").join("b").unwrap().as_str(), "/a/b");
    assert_eq!(rp("/").join("a").unwrap().as_str(), "/a");
    assert_eq!(
        rp("/a").join("b").unwrap().join("c.txt").unwrap().as_str(),
        "/a/b/c.txt"
    );
    assert_eq!(rp("/数据").join("📁.bin").unwrap().as_str(), "/数据/📁.bin");
}

#[test]
fn join_rejects_invalid_segments() {
    let base = rp("/a");
    assert!(matches!(base.join(""), Err(PathError::Invalid(_))));
    assert!(matches!(base.join("b/c"), Err(PathError::Invalid(_))));
    assert!(matches!(base.join("b\\"), Err(PathError::Invalid(_))));
    assert!(matches!(base.join("b\0"), Err(PathError::Invalid(_))));
    assert!(matches!(base.join(".."), Err(PathError::Invalid(_))));
    assert!(matches!(base.join("."), Err(PathError::Invalid(_))));
}

// --------------------------------------------------------------- parent ---

#[test]
fn parent_walks_up_to_root_then_none() {
    let mut cur = rp("/a/b/c");
    let mut seen = vec![cur.as_str().to_string()];
    while let Some(p) = cur.parent() {
        seen.push(p.as_str().to_string());
        cur = p;
    }
    assert_eq!(seen, vec!["/a/b/c", "/a/b", "/a", "/"]);
    assert_eq!(cur.parent(), None);
}

// ------------------------------------------------------------ ord / hash ---

#[test]
fn ord_is_lexicographic_over_the_string_form() {
    let mut paths = [
        rp("/b"),
        rp("/a/b"),
        rp("/a"),
        rp("/"),
        rp("/a/b/c"),
        rp("/ab"),
    ];
    paths.sort();
    let rendered: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();
    assert_eq!(rendered, vec!["/", "/a", "/a/b", "/a/b/c", "/ab", "/b"]);

    assert!(rp("/a") < rp("/a/b"));
    assert!(rp("/a/b") < rp("/ab"));
    assert!(rp("/ab") < rp("/b"));
    assert!(rp("/") < rp("/a"));
}

#[test]
fn eq_and_hash_agree_with_string_equality() {
    let a = rp("/a/b.txt");
    let b = rp("/a/b.txt").clone();
    let c = rp("/a/b.dat");
    assert_eq!(a, b);
    assert_ne!(a, c);

    let mut set = HashSet::new();
    set.insert(a.clone());
    set.insert(b);
    set.insert(c);
    assert_eq!(set.len(), 2);
    assert!(set.contains(&rp("/a/b.txt")));
}

// ---------------------------------------------------- display / fromstr ---

#[test]
fn display_matches_as_str() {
    let p = rp("/a/b/c.txt");
    assert_eq!(format!("{}", p), p.as_str());
    assert_eq!(format!("{}", rp("/")), "/");
}

#[test]
fn fromstr_parses_valid_and_rejects_invalid() {
    let p: RelPath = "/a/b.txt"
        .parse()
        .expect("FromStr should accept a valid path");
    assert_eq!(p.as_str(), "/a/b.txt");

    let bad: Result<RelPath, PathError> = "a/b".parse();
    assert!(matches!(bad, Err(PathError::Invalid(_))));
}

#[test]
fn display_fromstr_roundtrip() {
    for s in ["/", "/a", "/a/b", "/数据/📁.bin"] {
        let p = rp(s);
        let parsed = RelPath::from_str(&format!("{}", p)).unwrap();
        assert_eq!(parsed, p);
        assert_eq!(parsed.as_str(), s);
    }
}
