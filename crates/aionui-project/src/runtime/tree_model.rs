//! `TreeModel` — the app-lifetime, canonical-keyed, sparse file-fact tree.
//!
//! Each node is one directory level's listing (name → fact) plus its watch
//! handle. There is **no gen/version**: the WS transport is ordered, so applying
//! snapshots/deltas in arrival order is sufficient. Nodes produce canonical-
//! domain [`Snapshot`] / [`DeltaBatch`]; fan-out to the pe-keyed wire is the WS
//! handler's job (stage 1).
//!
//! `apply` never trusts a `notify` event's kind — it takes the affected child
//! names (or `All`) as a hint and reconciles against a fresh provider read, so
//! it is idempotent (repeated/stale events are no-ops) and absorbs coarse macOS
//! events (degrade to a full `read_dir`).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::canonical;

use super::error::FsError;
use super::fs_runtime::{FsRuntimeRegistry, IFsRuntime};
use super::provider::{EntryFact, Kind};
use super::watcher::WatchHandle;

/// A directory's full one-level listing (canonical domain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub canonical: String,
    /// Sorted by name for deterministic output.
    pub entries: Vec<(String, EntryFact)>,
}

/// One reconciled change to a directory level. The tree tracks only name+kind
/// (no size/mtime → no `modified`); a kind change surfaces as removed + added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added { name: String, kind: Kind },
    Removed { name: String },
    Renamed { from: String, to: String },
}

/// A batch of reconciled changes for one canonical (canonical domain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaBatch {
    pub canonical: String,
    pub changes: Vec<Change>,
}

/// What to re-check on `apply`: specific child names (precise Linux/Windows
/// events) or the whole level (coarse macOS events / rescan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    ChildNames(Vec<String>),
    All,
}

/// One mounted directory: its listing and the watch keeping it live.
struct Node {
    entries: BTreeMap<String, EntryFact>,
    watch: WatchHandle,
}

/// The sparse canonical-keyed fact tree.
pub struct TreeModel {
    nodes: HashMap<String, Node>,
    runtimes: FsRuntimeRegistry,
}

/// Resolve the runtime serving `canonical`'s scheme.
fn runtime_for(runtimes: &FsRuntimeRegistry, canonical: &str) -> Result<Arc<dyn IFsRuntime>, FsError> {
    let scheme = match canonical::parse_scheme(canonical) {
        Ok(canonical::Scheme::File) => "file",
        Err(_) => {
            return Err(FsError::Io {
                uri: canonical.to_owned(),
                message: "cannot parse scheme".to_owned(),
            });
        }
    };
    runtimes.get(scheme).ok_or_else(|| FsError::UnsupportedScheme {
        scheme: scheme.to_owned(),
    })
}

/// Build the child `file:` URI for `name` under directory `canonical`.
fn child_uri(canonical: &str, name: &str) -> Result<String, FsError> {
    let dir = canonical::uri_to_path(canonical).map_err(|_| FsError::Io {
        uri: canonical.to_owned(),
        message: "invalid canonical".to_owned(),
    })?;
    let child = dir.join(name);
    canonical::to_file_uri(&child).map_err(|_| FsError::Io {
        uri: canonical.to_owned(),
        message: "cannot build child uri".to_owned(),
    })
}

/// Reconcile `old` → `fresh` into added/removed/renamed changes. A same-inode
/// removed+added pair is synthesized into a rename (falls back to removed+added
/// when the provider cannot supply an inode).
fn diff(old: &BTreeMap<String, EntryFact>, fresh: &BTreeMap<String, EntryFact>) -> Vec<Change> {
    // Candidate removals/additions by name; a same-name kind change counts as
    // both (removed old kind + added new kind).
    let mut removed: Vec<(String, EntryFact)> = Vec::new();
    let mut added: Vec<(String, EntryFact)> = Vec::new();
    for (name, of) in old {
        match fresh.get(name) {
            None => removed.push((name.clone(), of.clone())),
            Some(nf) if nf.kind != of.kind => {
                removed.push((name.clone(), of.clone()));
                added.push((name.clone(), nf.clone()));
            }
            Some(_) => {}
        }
    }
    for (name, nf) in fresh {
        // Same-name kind change already pushed above; same kind = unchanged.
        if !old.contains_key(name) {
            added.push((name.clone(), nf.clone()));
        }
    }

    // Synthesize renames from same-inode removed+added pairs (inode 0 = unknown,
    // never matched → degrades to removed+added).
    let mut changes = Vec::new();
    let mut used_added = vec![false; added.len()];
    removed.retain(|(rname, rf)| {
        if rf.inode != 0
            && let Some(idx) = added
                .iter()
                .enumerate()
                // A rename preserves kind. Requiring `af.kind == rf.kind` stops a
                // same-name file→dir kind change from being read as a rename when
                // the OS reuses the freed inode for the new entry (Linux does;
                // macOS assigns a fresh inode) — that case is remove + add.
                .position(|(i, (_, af))| !used_added[i] && af.inode == rf.inode && af.kind == rf.kind)
        {
            used_added[idx] = true;
            changes.push(Change::Renamed {
                from: rname.clone(),
                to: added[idx].0.clone(),
            });
            false
        } else {
            true
        }
    });

    for (i, (name, af)) in added.into_iter().enumerate() {
        if !used_added[i] {
            changes.push(Change::Added { name, kind: af.kind });
        }
    }
    for (name, _) in removed {
        changes.push(Change::Removed { name });
    }
    changes
}

impl TreeModel {
    pub fn new(runtimes: FsRuntimeRegistry) -> Self {
        Self {
            nodes: HashMap::new(),
            runtimes,
        }
    }

    /// Mount a canonical: arm watch, read baseline, store node. Ordering is the
    /// TOCTOU guard — watch is armed *before* the baseline read so no change
    /// between the two is lost (events buffered by the actor are replayed via
    /// idempotent `apply`). Returns the baseline snapshot. Re-mount is a no-op
    /// that returns the current snapshot (subscription layer guards duplicates).
    pub async fn mount(&mut self, canonical: &str) -> Result<Snapshot, FsError> {
        if self.nodes.contains_key(canonical) {
            return Ok(self.snapshot(canonical).expect("mounted node has snapshot"));
        }
        let runtime = runtime_for(&self.runtimes, canonical)?;
        // 1. Arm watch first — any change from here on is buffered by the actor.
        let watch = runtime.watcher().watch(canonical)?;
        // 2. Read the baseline listing.
        let entries: BTreeMap<String, EntryFact> = runtime.provider().read_dir(canonical).await?.into_iter().collect();
        // 3. Store the node. (Buffered events are replayed later via idempotent apply.)
        self.nodes.insert(canonical.to_owned(), Node { entries, watch });
        Ok(self.snapshot(canonical).expect("just-mounted node has snapshot"))
    }

    /// Unmount: drop the node and stop its watch.
    pub fn unmount(&mut self, canonical: &str) {
        if let Some(node) = self.nodes.remove(canonical)
            && let Ok(runtime) = runtime_for(&self.runtimes, canonical)
        {
            runtime.watcher().unwatch(&node.watch);
        }
    }

    /// Reconcile the node against a fresh provider read per `hint`. `None` when
    /// nothing changed (idempotent) or the node is not mounted (in-flight event
    /// after unmount).
    pub async fn apply(&mut self, canonical: &str, hint: Hint) -> Result<Option<DeltaBatch>, FsError> {
        // In-flight event after unmount (or never mounted) → drop silently.
        let Some(old) = self.nodes.get(canonical).map(|n| n.entries.clone()) else {
            return Ok(None);
        };
        let runtime = runtime_for(&self.runtimes, canonical)?;

        let fresh: BTreeMap<String, EntryFact> = match hint {
            // Coarse event / rescan: re-read the whole level.
            Hint::All => runtime.provider().read_dir(canonical).await?.into_iter().collect(),
            // Precise event: re-stat only the named children, over the old base.
            Hint::ChildNames(child_names) => {
                let mut map = old.clone();
                for name in child_names {
                    let uri = child_uri(canonical, &name)?;
                    match runtime.provider().stat(&uri).await? {
                        Some(fact) => {
                            map.insert(name, fact);
                        }
                        None => {
                            map.remove(&name);
                        }
                    }
                }
                map
            }
        };

        let changes = diff(&old, &fresh);
        if changes.is_empty() {
            return Ok(None);
        }
        if let Some(node) = self.nodes.get_mut(canonical) {
            node.entries = fresh;
        }
        Ok(Some(DeltaBatch {
            canonical: canonical.to_owned(),
            changes,
        }))
    }

    /// Current snapshot of a mounted node, or `None` if not mounted.
    pub fn snapshot(&self, canonical: &str) -> Option<Snapshot> {
        self.nodes.get(canonical).map(|node| Snapshot {
            canonical: canonical.to_owned(),
            entries: node.entries.iter().map(|(n, f)| (n.clone(), f.clone())).collect(),
        })
    }
}

#[cfg(test)]
#[path = "tree_model_test.rs"]
mod tree_model_test;
