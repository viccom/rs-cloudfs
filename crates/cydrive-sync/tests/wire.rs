//! Wire-protocol JSON shapes: the exact bytes cydrive-cli's sync
//! module (Batch B) will both produce and consume — frozen here so
//! client and server cannot drift.

use cydrive_sync::wire::{
    PullRequest, PullResponse, PulledRow, PushRequest, PushResponse, PushRow,
};

#[test]
fn push_request_parses_the_full_shape() {
    let json = r#"{"key":"deadbeef","secret":"s3cret","rows":[
        {"rel_path":"/a.txt","deleted":false,"payload":"{}"},
        {"rel_path":"/b","deleted":true,"payload":""}]}"#;
    let req: PushRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.key, "deadbeef");
    assert_eq!(req.secret.as_deref(), Some("s3cret"));
    assert_eq!(req.rows.len(), 2);
    assert_eq!(req.rows[0].rel_path, "/a.txt");
    assert!(!req.rows[0].deleted);
    assert_eq!(req.rows[0].payload, "{}");
    assert!(req.rows[1].deleted);
    assert_eq!(req.rows[1].payload, "");
}

#[test]
fn push_request_secret_is_optional() {
    let req: PushRequest = serde_json::from_str(r#"{"key":"k","rows":[]}"#).unwrap();
    assert_eq!(req.secret, None);
    assert!(req.rows.is_empty());
}

#[test]
fn push_response_serializes_to_exact_shape() {
    let body = serde_json::to_value(PushResponse { max_version: 12 }).unwrap();
    assert_eq!(body, serde_json::json!({"max_version": 12}));
}

#[test]
fn pull_request_parses_the_full_shape() {
    let req: PullRequest = serde_json::from_str(r#"{"key":"k","since":7}"#).unwrap();
    assert_eq!(req.key, "k");
    assert_eq!(req.since, 7);
}

/// The pre-secret pull form `{"key","since"}` must keep parsing (the
/// secret defaults to absent) and re-serialize byte-stably: no
/// `"secret": null` key may appear, so a secretless new client sends
/// exactly the bytes an old server always accepted.
#[test]
fn pull_request_without_secret_keeps_the_old_wire_shape() {
    let req: PullRequest = serde_json::from_str(r#"{"key":"k","since":7}"#).unwrap();
    assert_eq!(req.key, "k");
    assert_eq!(req.since, 7);
    let back = serde_json::to_value(&req).unwrap();
    assert_eq!(back, serde_json::json!({"key": "k", "since": 7}));
}

/// A pull carrying a secret must survive a parse -> serialize ->
/// parse roundtrip without the field being dropped (the gate lives on
/// the server, but the client serializes these same structs).
#[test]
fn pull_request_with_secret_survives_roundtrip() {
    let json = r#"{"key":"k","since":7,"secret":"s3cret"}"#;
    let req: PullRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.key, "k");
    assert_eq!(req.since, 7);
    let back = serde_json::to_value(&req).unwrap();
    assert_eq!(
        back.get("secret").and_then(|v| v.as_str()),
        Some("s3cret"),
        "secret must survive a parse->serialize roundtrip: {back}"
    );
    let again: PullRequest = serde_json::from_value(back).unwrap();
    let out = serde_json::to_value(&again).unwrap();
    assert_eq!(
        out.get("secret").and_then(|v| v.as_str()),
        Some("s3cret"),
        "roundtripped shape must parse back with the secret: {out}"
    );
}

#[test]
fn pull_response_serializes_to_exact_shape() {
    let body = serde_json::to_value(PullResponse {
        rows: vec![PulledRow {
            rel_path: "/a".to_string(),
            version: 3,
            deleted: true,
            payload: String::new(),
        }],
        max_version: 4,
    })
    .unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "rows": [{"rel_path": "/a", "version": 3, "deleted": true, "payload": ""}],
            "max_version": 4
        })
    );
}

#[test]
fn wire_rows_roundtrip_through_json() {
    let push_row = PushRow {
        rel_path: "/docs/a.txt".to_string(),
        deleted: false,
        payload: "{\"size\":314}".to_string(),
    };
    let back: PushRow = serde_json::from_str(&serde_json::to_string(&push_row).unwrap()).unwrap();
    assert_eq!(back, push_row);

    let pulled = PulledRow {
        rel_path: "/docs/b.txt".to_string(),
        version: 42,
        deleted: false,
        payload: "{\"size\":271}".to_string(),
    };
    let back: PulledRow = serde_json::from_str(&serde_json::to_string(&pulled).unwrap()).unwrap();
    assert_eq!(back, pulled);
}
