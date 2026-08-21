//! Wire-format unit tests: scope-token grammar, dir-key codec, keyset cursor
//! round-trip + the 400 rejection matrix, and the `win` parser.

use super::*;

// -- ScopeToken --------------------------------------------------------------

#[test]
fn scope_token_parses_each_variant() {
    assert_eq!(ScopeToken::parse("pinned"), Some(ScopeToken::Pinned));
    assert_eq!(ScopeToken::parse("chats"), Some(ScopeToken::Chats));
    assert_eq!(ScopeToken::parse("project:P1"), Some(ScopeToken::Project("P1".into())));

    let key = canonical_to_dir_key("file:///work/a");
    let dir = ScopeToken::parse(&format!("dir:{key}"));
    assert_eq!(dir, Some(ScopeToken::Dir("file:///work/a".into())));
}

#[test]
fn scope_token_rejects_unknown_and_empty() {
    assert_eq!(ScopeToken::parse("bogus"), None);
    assert_eq!(ScopeToken::parse("project:"), None);
    assert_eq!(ScopeToken::parse("dir:@@not-base64@@"), None);
}

#[test]
fn scope_token_round_trips_through_token() {
    for token in ["pinned", "chats", "project:abc"] {
        let parsed = ScopeToken::parse(token).unwrap();
        assert_eq!(parsed.to_token(), token);
    }
    // Dir round-trips via canonical, not the raw key string.
    let dir = ScopeToken::Dir("file:///x/y".into());
    assert_eq!(ScopeToken::parse(&dir.to_token()), Some(dir));
}

// -- dir key codec -----------------------------------------------------------

#[test]
fn dir_key_codec_round_trips_paths_with_colons() {
    let canonical = "file:///Users/me/Projects/a-b_c";
    let key = canonical_to_dir_key(canonical);
    // URL-safe: no '/', '+', or '=' that would break a query token.
    assert!(!key.contains('/') && !key.contains('+') && !key.contains('='));
    assert_eq!(dir_key_to_canonical(&key).as_deref(), Some(canonical));
}

#[test]
fn dir_key_rejects_garbage() {
    assert_eq!(dir_key_to_canonical("*not*base64*"), None);
}

// -- Cursor ------------------------------------------------------------------

#[test]
fn activity_cursor_round_trips() {
    let scope = ScopeToken::Project("P1".into());
    let cursor = Cursor::Activity {
        updated_at: 42,
        item_type: "conversation".into(),
        item_id: "c1".into(),
    };
    let encoded = cursor.encode(&scope);
    assert_eq!(Cursor::decode(&encoded, &scope).unwrap(), cursor);
}

#[test]
fn pinned_cursor_round_trips() {
    let scope = ScopeToken::Pinned;
    let cursor = Cursor::Pinned {
        order_key: -1000,
        item_type: "team".into(),
        item_id: "t1".into(),
    };
    let encoded = cursor.encode(&scope);
    assert_eq!(Cursor::decode(&encoded, &scope).unwrap(), cursor);
}

#[test]
fn cursor_rejects_bad_base64() {
    let err = Cursor::decode("*** not base64 ***", &ScopeToken::Pinned).unwrap_err();
    assert!(matches!(err, SidebarError::BadRequest(_)));
}

#[test]
fn cursor_rejects_bad_json() {
    let raw = B64.encode(b"{not json");
    let err = Cursor::decode(&raw, &ScopeToken::Pinned).unwrap_err();
    assert!(matches!(err, SidebarError::BadRequest(_)));
}

#[test]
fn cursor_rejects_version_mismatch() {
    let raw = B64.encode(
        serde_json::to_vec(&serde_json::json!({
            "v": CURSOR_VERSION + 1, "scope": "pinned", "order_key": 1, "item_type": "team", "item_id": "t1"
        }))
        .unwrap(),
    );
    let err = Cursor::decode(&raw, &ScopeToken::Pinned).unwrap_err();
    assert!(matches!(err, SidebarError::BadRequest(_)));
}

#[test]
fn cursor_rejects_cross_scope_replay() {
    // Minted for project:P1, replayed against project:P2.
    let minted = Cursor::Activity {
        updated_at: 1,
        item_type: "conversation".into(),
        item_id: "c1".into(),
    }
    .encode(&ScopeToken::Project("P1".into()));
    let err = Cursor::decode(&minted, &ScopeToken::Project("P2".into())).unwrap_err();
    assert!(matches!(err, SidebarError::BadRequest(_)));
}

#[test]
fn cursor_rejects_missing_keyset_field() {
    // A pinned scope needs order_key; an activity payload lacks it.
    let raw = B64.encode(
        serde_json::to_vec(&serde_json::json!({
            "v": CURSOR_VERSION, "scope": "pinned", "updated_at": 5, "item_type": "team", "item_id": "t1"
        }))
        .unwrap(),
    );
    let err = Cursor::decode(&raw, &ScopeToken::Pinned).unwrap_err();
    assert!(matches!(err, SidebarError::BadRequest(_)));
}

// -- parse_win ---------------------------------------------------------------

#[test]
fn parse_win_reads_token_and_limit() {
    let dir_key = canonical_to_dir_key("file:///w/a");
    let entries = vec![
        "pinned:5".to_owned(),
        "project:P1:10".to_owned(),
        format!("dir:{dir_key}:3"),
        "chats:20".to_owned(),
    ];
    let parsed = parse_win(&entries).unwrap();
    assert_eq!(parsed.len(), 4);
    assert!(parsed.contains(&("project:P1".to_owned(), 10)));
    assert!(parsed.contains(&(format!("dir:{dir_key}"), 3)));
}

#[test]
fn parse_win_rejects_bad_entries() {
    assert!(matches!(
        parse_win(&["nolimit".to_owned()]),
        Err(SidebarError::BadRequest(_))
    ));
    assert!(matches!(
        parse_win(&["pinned:abc".to_owned()]),
        Err(SidebarError::BadRequest(_))
    ));
    assert!(matches!(
        parse_win(&["bogus:5".to_owned()]),
        Err(SidebarError::BadRequest(_))
    ));
    assert!(matches!(
        parse_win(&["pinned:0".to_owned()]),
        Err(SidebarError::BadRequest(_))
    ));
    assert!(matches!(
        parse_win(&["pinned:5".to_owned(), "pinned:6".to_owned()]),
        Err(SidebarError::BadRequest(_))
    ));
}

#[test]
fn parse_win_rejects_too_many_entries() {
    let entries: Vec<String> = (0..(MAX_WIN_ENTRIES + 1)).map(|i| format!("project:P{i}:5")).collect();
    assert!(matches!(parse_win(&entries), Err(SidebarError::BadRequest(_))));
}

// -- validate_limit ----------------------------------------------------------

#[test]
fn validate_limit_enforces_bounds() {
    assert!(validate_limit(1).is_ok());
    assert!(validate_limit(MAX_LIMIT).is_ok());
    assert!(validate_limit(0).is_err());
    assert!(validate_limit(MAX_LIMIT + 1).is_err());
    assert!(validate_limit(-3).is_err());
}
