use std::path::{Path, PathBuf};

use notify::event::{Flag, ModifyKind};
use notify::{Event, EventKind};

use super::{RawEvent, map_event};

/// Resolve any path under `/root` to the fixed canonical, everything else None.
fn resolve_root(p: &Path) -> Option<String> {
    if p.starts_with("/root") {
        Some("file:///root".to_owned())
    } else {
        None
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

/// Resolve `/root` and `/lib` to distinct canonicals, everything else None.
fn resolve_two(p: &Path) -> Option<String> {
    if p.starts_with("/root") {
        Some("file:///root".to_owned())
    } else if p.starts_with("/lib") {
        Some("file:///lib".to_owned())
    } else {
        None
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
