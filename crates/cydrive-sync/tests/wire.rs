//! Wire-protocol JSON shapes: the exact bytes cydrive-cli's sync
//! module (Batch B) will both produce and consume — frozen here so
//! client and server cannot drift.

use cydrive_sync::wire::{
    PullRequest, PullResponse, PulledRow, PushRequest, PushResponse, PushRow, SubscribeEvent,
    SubscribeRequest,
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

// ---- SSE doorbell batch: optional `client_id`, SubscribeRequest,
// SubscribeEvent ----

/// PushRequest's `client_id`: `Some` serializes the field, `None`
/// omits it entirely (the secret-field pattern), and the
/// pre-client_id push JSON of old clients keeps parsing with
/// `client_id: None`.
#[test]
fn push_request_client_id_shape_and_old_json_compat() {
    let with = serde_json::to_value(PushRequest {
        key: "k".to_string(),
        secret: None,
        client_id: Some("laptop-01".to_string()),
        rows: Vec::new(),
    })
    .unwrap();
    assert_eq!(
        with.get("client_id").and_then(|v| v.as_str()),
        Some("laptop-01"),
        "client_id=Some must serialize the field: {with}"
    );

    let without = serde_json::to_value(PushRequest {
        key: "k".to_string(),
        secret: None,
        client_id: None,
        rows: Vec::new(),
    })
    .unwrap();
    assert!(
        without.get("client_id").is_none(),
        "client_id=None must not serialize a key: {without}"
    );

    let old: PushRequest =
        serde_json::from_str(r#"{"key":"deadbeef","secret":"s3cret","rows":[]}"#).unwrap();
    assert_eq!(
        old.client_id, None,
        "old push JSON (no client_id) must keep parsing"
    );
}

/// PullRequest's `client_id`: same shape rules, and the whole
/// client_id-less pull stays byte-identical to the pre-doorbell wire
/// form (both optional fields vanish when `None`).
#[test]
fn pull_request_client_id_shape_and_old_json_compat() {
    let with = serde_json::to_value(PullRequest {
        key: "k".to_string(),
        since: 7,
        secret: None,
        client_id: Some("desktop-9".to_string()),
    })
    .unwrap();
    assert_eq!(
        with.get("client_id").and_then(|v| v.as_str()),
        Some("desktop-9"),
        "client_id=Some must serialize the field: {with}"
    );

    let without = serde_json::to_value(PullRequest {
        key: "k".to_string(),
        since: 7,
        secret: None,
        client_id: None,
    })
    .unwrap();
    assert_eq!(
        without,
        serde_json::json!({"key": "k", "since": 7}),
        "a client_id-less pull stays byte-identical to the old wire form"
    );

    let old: PullRequest = serde_json::from_str(r#"{"key":"k","since":7,"secret":"s3cret"}"#)
        .unwrap();
    assert_eq!(
        old.client_id, None,
        "old pull JSON (no client_id) must keep parsing"
    );
}

/// SubscribeRequest: the minimal `{"key"}` form is the old-shape byte
/// form (both optional fields omitted when `None`), the full shape
/// parses field by field.
#[test]
fn subscribe_request_parses_full_and_minimal_shapes() {
    let full: SubscribeRequest =
        serde_json::from_str(r#"{"key":"deadbeef","secret":"s3cret","client_id":"laptop-01"}"#)
            .unwrap();
    assert_eq!(full.key, "deadbeef");
    assert_eq!(full.secret.as_deref(), Some("s3cret"));
    assert_eq!(full.client_id.as_deref(), Some("laptop-01"));

    let minimal: SubscribeRequest = serde_json::from_str(r#"{"key":"k"}"#).unwrap();
    assert_eq!(minimal.secret, None);
    assert_eq!(minimal.client_id, None);
    let back = serde_json::to_value(&minimal).unwrap();
    assert_eq!(back, serde_json::json!({"key": "k"}));

    let with_client = serde_json::to_value(SubscribeRequest {
        key: "k".to_string(),
        secret: None,
        client_id: Some("laptop-01".to_string()),
    })
    .unwrap();
    assert_eq!(
        with_client,
        serde_json::json!({"key": "k", "client_id": "laptop-01"})
    );
}

/// The SSE event payload is a fixed two-field shape; `origin` is
/// ALWAYS present — `null` when the pusher sent no client_id (the
/// subscriber cannot distinguish "no origin" from "origin unknown"
/// any other way, and the doorbell contract fixes the bytes).
#[test]
fn subscribe_event_serializes_origin_as_null_when_absent() {
    let some =
        serde_json::to_string(&SubscribeEvent {
            max_version: 5,
            origin: Some("laptop-01".to_string()),
        })
        .unwrap();
    assert_eq!(some, r#"{"max_version":5,"origin":"laptop-01"}"#);

    let none = serde_json::to_string(&SubscribeEvent {
        max_version: 5,
        origin: None,
    })
    .unwrap();
    assert_eq!(none, r#"{"max_version":5,"origin":null}"#);

    let back: SubscribeEvent = serde_json::from_str(&none).unwrap();
    assert_eq!(
        back,
        SubscribeEvent {
            max_version: 5,
            origin: None,
        }
    );
}
