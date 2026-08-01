use std::path::{Path, PathBuf};

use notify::event::{Flag, ModifyKind};
use notify::{Event, EventKind};

use super::{RawEvent, map_event};

/// Resolve any path under `/root` to the fixed canonical, everything else empty.
fn resolve_root(p: &Path) -> Vec<String> {
    if p.starts_with("/root") {
        vec!["file:///root".to_owned()]
    } else {
        vec![]
    }
}

#[test]
fn map_event_groups_changed_paths_by_canonical() {
    let event = Event::new(EventKind::Modify(ModifyKind::Any))
        .add_path(PathBuf::from("/root/a.txt"))
        .add_path(PathBuf::from("/root/b.txt"));

    let out = map_event(&event, &resolve_root);

    assert_eq!(
        out,
        vec![RawEvent::Changed {
            canonical: "file:///root".to_owned(),
            paths: vec!["/root/a.txt".to_owned(), "/root/b.txt".to_owned()],
        }]
    );
}

#[test]
fn map_event_drops_unresolved_paths() {
    let event = Event::new(EventKind::Modify(ModifyKind::Any))
        .add_path(PathBuf::from("/root/a.txt"))
        .add_path(PathBuf::from("/elsewhere/x.txt"));

    let out = map_event(&event, &resolve_root);

    assert_eq!(
        out,
        vec![RawEvent::Changed {
            canonical: "file:///root".to_owned(),
            paths: vec!["/root/a.txt".to_owned()],
        }]
    );
}

/// Resolve `/root` and `/lib` to distinct canonicals, everything else empty.
fn resolve_two(p: &Path) -> Vec<String> {
    if p.starts_with("/root") {
        vec!["file:///root".to_owned()]
    } else if p.starts_with("/lib") {
        vec!["file:///lib".to_owned()]
    } else {
        vec![]
    }
}

#[test]
fn map_event_groups_multiple_canonicals_first_seen_order() {
    // Paths under two watched dirs, interleaved → one Changed per canonical,
    // first-seen canonical order preserved, per-canonical path order preserved.
    let event = Event::new(EventKind::Modify(ModifyKind::Any))
        .add_path(PathBuf::from("/root/a.txt"))
        .add_path(PathBuf::from("/lib/x.txt"))
        .add_path(PathBuf::from("/root/b.txt"));

    let out = map_event(&event, &resolve_two);

    assert_eq!(
        out,
        vec![
            RawEvent::Changed {
                canonical: "file:///root".to_owned(),
                paths: vec!["/root/a.txt".to_owned(), "/root/b.txt".to_owned()],
            },
            RawEvent::Changed {
                canonical: "file:///lib".to_owned(),
                paths: vec!["/lib/x.txt".to_owned()],
            },
        ]
    );
}

#[test]
fn map_event_rescan_dedups_overflow_per_canonical() {
    // A rescan touching a canonical more than once emits one Overflow for it,
    // one per distinct canonical, order-stable by first appearance.
    let event = Event::new(EventKind::Any)
        .add_path(PathBuf::from("/root/a.txt"))
        .add_path(PathBuf::from("/lib/x.txt"))
        .add_path(PathBuf::from("/root/b.txt"))
        .set_flag(Flag::Rescan);

    let out = map_event(&event, &resolve_two);

    assert_eq!(
        out,
        vec![
            RawEvent::Overflow {
                canonical: "file:///root".to_owned()
            },
            RawEvent::Overflow {
                canonical: "file:///lib".to_owned()
            },
        ]
    );
}

/// Resolve a watched subdir path to BOTH the subdir itself and its parent root
/// (the self-then-parent shape `resolve_owner` returns for a watched subdir);
/// any other path under `/root` resolves to root only.
fn resolve_self_and_parent(p: &Path) -> Vec<String> {
    if p == Path::new("/root/a") {
        vec!["file:///root/a".to_owned(), "file:///root".to_owned()]
    } else if p.starts_with("/root") {
        vec!["file:///root".to_owned()]
    } else {
        vec![]
    }
}

#[test]
fn map_event_fans_watched_subdir_to_both_self_and_parent() {
    // Deleting a watched subdir `a`: the event path is `/root/a`, which is both a
    // watched directory (self) and a child of watched root (parent). map_event
    // must emit a Changed for the PARENT root mentioning `a` — that is the change
    // that reconciles root's listing and removes the stale `a`. Reverting
    // resolve to self-only (the old exact-match precedence) drops the root group
    // and this assertion fails — the regression tripwire.
    let event = Event::new(EventKind::Remove(notify::event::RemoveKind::Folder)).add_path(PathBuf::from("/root/a"));

    let out = map_event(&event, &resolve_self_and_parent);

    assert_eq!(
        out,
        vec![
            RawEvent::Changed {
                canonical: "file:///root/a".to_owned(),
                paths: vec!["/root/a".to_owned()],
            },
            RawEvent::Changed {
                canonical: "file:///root".to_owned(),
                paths: vec!["/root/a".to_owned()],
            },
        ],
        "a watched subdir's event must reconcile its parent, not only itself"
    );
}

#[test]
fn map_event_rescan_flag_yields_overflow() {
    let event = Event::new(EventKind::Any)
        .add_path(PathBuf::from("/root/a.txt"))
        .set_flag(Flag::Rescan);

    let out = map_event(&event, &resolve_root);

    assert_eq!(
        out,
        vec![RawEvent::Overflow {
            canonical: "file:///root".to_owned()
        }]
    );
}
