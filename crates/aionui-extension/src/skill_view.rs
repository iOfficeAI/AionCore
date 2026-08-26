//! Per-conversation skill VIEW directory, owned exclusively by AionUi.
//!
//! This is the point of the refactor: the symlink LANDING SITE moves out of the
//! user's workspace (which may be a git repository) and into
//! `{data_dir}/session-skills/{user_id}/{conversation_id}/`. Skill sources, the
//! enable model and the snapshot semantics are unchanged — only where the links
//! land.
//!
//! Layout, one tree satisfying both vendor shapes:
//!
//! ```text
//! {view}/.claude-plugin/plugin.json   <- what --plugin-dir needs
//! {view}/skills/{name} -> {real source dir}
//! ```
//!
//! claude takes `{view}` (the plugin root); codex takes `{view}/skills` (the
//! skills root, directly holding `{name}/SKILL.md`). Both were probed against
//! this exact layout, including with the skill directory as a symlink.
//!
//! Because AionUi OWNS this tree, two things differ from the old workspace path:
//! rebuild is an EXACT match against the snapshot (no first-write-wins, so a
//! skill dropped from the snapshot actually disappears), and deletion is safe.
//!
//! There is deliberately NO copy fallback when linking fails. The old workspace
//! path degraded to a recursive copy of real files, which this refactor removes:
//! a failed link is skipped with a `warn`, because materializing user files into
//! a directory we then treat as disposable is worse than the skill being absent.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as BlockingMutex};

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::ExtensionError;
use crate::skill_service::{ResolvedAgentSkill, create_symlink};

const VIEW_ROOT_DIR_NAME: &str = "session-skills";
const SKILLS_SUBDIR: &str = "skills";
const PLUGIN_MANIFEST_DIR: &str = ".claude-plugin";
const PLUGIN_MANIFEST_FILE: &str = "plugin.json";

/// The plugin name.
///
/// This is NOT an internal identifier: a plugin's name becomes the prefix of
/// every skill name the agent sees, so a conversation's `cron` is presented as
/// `aionui:cron` (the same way `superpowers:brainstorming` appears). It is
/// user-visible text.
///
/// Consequence for callers: `extra.skills` stores the BARE name (`cron`) while
/// the agent side is prefixed. Any logic that matches, counts, or correlates by
/// skill name must not assume the two sides are equal.
pub const PLUGIN_NAME: &str = "aionui";

pub fn view_root(data_dir: &Path) -> PathBuf {
    data_dir.join(VIEW_ROOT_DIR_NAME)
}

/// Ids reaching this module come from storage, so a traversal-shaped value is
/// refused here rather than trusted upstream.
fn validate_segment(value: &str, field: &'static str) -> Result<(), ExtensionError> {
    if value.is_empty() || value == "." || value.contains('/') || value.contains('\\') || value.contains("..") {
        return Err(ExtensionError::PathTraversal(format!(
            "{field} is not a safe path segment"
        )));
    }
    Ok(())
}

pub fn view_dir(data_dir: &Path, user_id: &str, conversation_id: &str) -> Result<PathBuf, ExtensionError> {
    validate_segment(user_id, "user_id")?;
    validate_segment(conversation_id, "conversation_id")?;
    Ok(view_root(data_dir).join(user_id).join(conversation_id))
}

/// The SKILLS root (`{view}/skills`), which is what codex's `extraRoots`
/// expects — as opposed to [`view_dir`], the plugin root claude wants.
pub fn view_skills_dir(data_dir: &Path, user_id: &str, conversation_id: &str) -> Result<PathBuf, ExtensionError> {
    Ok(view_dir(data_dir, user_id, conversation_id)?.join(SKILLS_SUBDIR))
}

fn plugin_manifest_body() -> String {
    serde_json::json!({
        "name": PLUGIN_NAME,
        "version": "0.0.1",
        "description": "AionUi session skills",
    })
    .to_string()
}

/// Serializes [`rebuild_view`] per conversation.
///
/// A first turn rebuilds the view THREE times -- conversation create, runtime
/// ensure, and send -- and the last two land under a millisecond apart. Because
/// the rebuild is "snapshot the link names, wipe the tree, relink", two
/// concurrent calls interleave destructively: one call's `remove_dir_all` can
/// land after the other has already created links, so links that were made
/// disappear while the call that made them has moved past those names. Observed
/// live as four `File exists` link failures plus two `skills 2 / requested 4`
/// lines on a view that in fact held all four -- i.e. the reported counts were
/// wrong, and the benign on-disk outcome was timing luck rather than a property
/// of the code.
///
/// Keyed per (user, conversation) rather than one global lock so unrelated
/// conversations keep rebuilding concurrently.
static VIEW_LOCKS: LazyLock<BlockingMutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| BlockingMutex::new(HashMap::new()));

/// The lock guarding one conversation's view. Never held across an await inside
/// this function -- only the returned `Arc` is.
fn view_lock(user_id: &str, conversation_id: &str) -> Arc<Mutex<()>> {
    let key = format!("{user_id}/{conversation_id}");
    // A poisoned registry is not a reason to refuse a rebuild: the map holds no
    // invariant beyond "one lock per key", which a panicking holder cannot break.
    let mut locks = VIEW_LOCKS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Entries nobody holds are dropped here, so the map cannot grow with every
    // conversation the process has ever opened. A lock currently held (or being
    // awaited) has a second owner, so it survives.
    locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    Arc::clone(locks.entry(key).or_default())
}

/// Rebuild the view so it matches `skills` EXACTLY. Returns the number of links
/// the view holds afterwards. Idempotent: build-task calls this on every session
/// open, and a call that finds the view already correct changes nothing.
pub async fn rebuild_view(
    data_dir: &Path,
    user_id: &str,
    conversation_id: &str,
    skills: &[ResolvedAgentSkill],
) -> Result<usize, ExtensionError> {
    // Validated before the lock is taken, so a traversal-shaped id cannot even
    // register a lock entry.
    let view = view_dir(data_dir, user_id, conversation_id)?;
    let skills_dir = view.join(SKILLS_SUBDIR);

    let lock = view_lock(user_id, conversation_id);
    let _serialized = lock.lock().await;

    // Which skills can actually be linked. Resolved BEFORE the tree is touched so
    // the desired set is known up front, which is what makes the "already correct"
    // check below possible.
    let mut linkable = Vec::with_capacity(skills.len());
    for skill in skills {
        if tokio::fs::try_exists(&skill.source_path).await.unwrap_or(false) {
            linkable.push(skill);
        } else {
            warn!(
                skill = %skill.name,
                conversation_id = %conversation_id,
                "skill_view: source directory missing; skipping this skill"
            );
        }
    }
    let desired: HashSet<String> = linkable.iter().map(|skill| skill.name.clone()).collect();

    // Names already linked, captured BEFORE the tree is replaced, so a mid-session
    // addition can be reported. Allow-listing (`--add-dir`) is a SPAWN argument:
    // a skill added while the agent is running gets into the view and the index,
    // but its supplementary files stay unreadable to the already-started process
    // until the runtime restarts. Silence here would present as "the agent can
    // see the skill but cannot open its references", with nothing to point at.
    let previously_linked = existing_link_names(&skills_dir).await;

    tokio::fs::create_dir_all(view.join(PLUGIN_MANIFEST_DIR)).await?;
    tokio::fs::write(
        view.join(PLUGIN_MANIFEST_DIR).join(PLUGIN_MANIFEST_FILE),
        plugin_manifest_body(),
    )
    .await?;

    // Already exactly right: return without touching the tree. This is the common
    // case -- two of a first turn's three rebuilds ask for the same set the first
    // one just linked -- and skipping them is not merely an optimization: a wipe
    // and relink would briefly empty a directory a CLI may be reading, and would
    // re-derive an "added" count against a snapshot of our own making.
    if previously_linked == desired {
        debug!(
            user_id = %user_id,
            conversation_id = %conversation_id,
            skills = desired.len(),
            "skill_view: session skill view already matches the snapshot; left untouched"
        );
        return Ok(desired.len());
    }

    // Replace the skills tree wholesale. Safe because it holds only our own
    // symlinks, and REQUIRED by G2: a skill dropped from the snapshot has to
    // actually disappear, which a merge-style update would not achieve.
    //
    // `remove_dir_all` on a directory of symlinks removes the LINKS, not their
    // targets, so the user's real skill sources are untouched.
    match tokio::fs::remove_dir_all(&skills_dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(ExtensionError::Io(e)),
    }
    tokio::fs::create_dir_all(&skills_dir).await?;

    let mut created = 0usize;
    for skill in &linkable {
        let target = skills_dir.join(&skill.name);
        match create_symlink(&skill.source_path, &target).await {
            Ok(()) => created += 1,
            Err(e) => warn!(
                skill = %skill.name,
                conversation_id = %conversation_id,
                error = %e,
                "skill_view: failed to link skill into the view directory"
            ),
        }
    }

    if !previously_linked.is_empty() {
        let added = desired.difference(&previously_linked).count();
        if added > 0 {
            warn!(
                user_id = %user_id,
                conversation_id = %conversation_id,
                added,
                "skill_view: skills added to a conversation that already had a view; their \
                 supplementary files stay unreadable to an already-running agent until the \
                 runtime restarts (directory allow-listing is a spawn argument)"
            );
        }
    }

    info!(
        user_id = %user_id,
        conversation_id = %conversation_id,
        skills = created,
        linkable = linkable.len(),
        requested = skills.len(),
        "skill_view: rebuilt session skill view"
    );
    Ok(created)
}

/// Skill names currently linked in the view. Empty when the view does not exist
/// yet, which is indistinguishable from "no skills" on purpose: both mean there
/// is no previous state to diff against.
async fn existing_link_names(skills_dir: &Path) -> HashSet<String> {
    let mut names = HashSet::new();
    let Ok(mut entries) = tokio::fs::read_dir(skills_dir).await else {
        return names;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(name) = entry.file_name().to_str() {
            names.insert(name.to_owned());
        }
    }
    names
}

/// Drop this conversation's view. `Ok(false)` means there was nothing to remove.
pub async fn remove_view(data_dir: &Path, user_id: &str, conversation_id: &str) -> Result<bool, ExtensionError> {
    let view = view_dir(data_dir, user_id, conversation_id)?;
    match tokio::fs::remove_dir_all(&view).await {
        Ok(()) => {
            info!(
                user_id = %user_id,
                conversation_id = %conversation_id,
                "skill_view: removed session skill view"
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ExtensionError::Io(e)),
    }
}

/// Reap views whose `(user_id, conversation_id)` is absent from `live`.
///
/// Keyed by the PAIR, not by conversation alone: two Core users can hold
/// same-shaped conversation ids, and dropping one because the other user's
/// conversation is gone would break G3.
///
/// A view outlives its conversation only when the delete hook did not run
/// (crash, forced kill), so this runs once at startup rather than on a timer.
pub async fn cleanup_orphan_views(data_dir: &Path, live: &HashSet<(String, String)>) -> Result<usize, ExtensionError> {
    let root = view_root(data_dir);
    let mut users = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(ExtensionError::Io(e)),
    };

    let mut removed = 0usize;
    while let Some(user_entry) = users.next_entry().await? {
        let Some(user_id) = user_entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let mut conversations = match tokio::fs::read_dir(user_entry.path()).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        while let Some(entry) = conversations.next_entry().await? {
            let Some(conversation_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if live.contains(&(user_id.clone(), conversation_id.clone())) {
                continue;
            }
            if tokio::fs::remove_dir_all(entry.path()).await.is_ok() {
                removed += 1;
            }
        }
    }

    if removed > 0 {
        info!(removed, "skill_view: reaped orphan session skill views");
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn write_source_skill(root: &Path, name: &str) -> ResolvedAgentSkill {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("references")).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nbody"),
        )
        .unwrap();
        std::fs::write(dir.join("references").join("notes.md"), "REFTOKEN").unwrap();
        ResolvedAgentSkill {
            name: name.to_owned(),
            source_path: dir,
        }
    }

    #[test]
    fn the_view_path_isolates_users_and_conversations() {
        let a = view_dir(Path::new("/data"), "user_a", "conv_1").unwrap();
        let b = view_dir(Path::new("/data"), "user_b", "conv_1").unwrap();
        assert_eq!(a, Path::new("/data/session-skills/user_a/conv_1"));
        assert_ne!(a, b, "the user segment is what keeps multi-Core installs apart");
        assert_eq!(
            view_skills_dir(Path::new("/data"), "user_a", "conv_1").unwrap(),
            a.join("skills")
        );
    }

    /// An id reaching this function comes from storage, so traversal must be
    /// refused here rather than trusted upstream.
    #[test]
    fn traversal_ids_are_refused() {
        for bad in ["..", "../escape", "a/b", "a\\b", "", "."] {
            assert!(
                view_dir(Path::new("/data"), bad, "conv_1").is_err(),
                "user_id {bad:?} must be refused"
            );
            assert!(
                view_dir(Path::new("/data"), "user_a", bad).is_err(),
                "conversation_id {bad:?} must be refused"
            );
        }
    }

    #[tokio::test]
    async fn rebuild_creates_the_plugin_manifest_and_one_link_per_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        let skills = vec![
            write_source_skill(&sources, "cron"),
            write_source_skill(&sources, "officecli"),
        ];

        let linked = rebuild_view(&data_dir, "user_a", "conv_1", &skills).await.unwrap();
        assert_eq!(linked, 2);

        let view = view_dir(&data_dir, "user_a", "conv_1").unwrap();
        let manifest = std::fs::read_to_string(view.join(".claude-plugin").join("plugin.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        // This name becomes the agent-visible skill-name PREFIX (`aionui:cron`),
        // so it is user-visible text, not an internal id.
        assert_eq!(parsed["name"], PLUGIN_NAME);

        // The supplementary file must be reachable THROUGH the link, which is
        // what makes a symlinked skill dir usable at all.
        assert_eq!(
            std::fs::read_to_string(view.join("skills").join("cron").join("references").join("notes.md")).unwrap(),
            "REFTOKEN"
        );
    }

    /// AionUi OWNS this directory, so unlike the old workspace path there is no
    /// first-write-wins: a snapshot change must produce an exact match.
    #[tokio::test]
    async fn rebuild_drops_skills_that_left_the_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        let cron = write_source_skill(&sources, "cron");
        let pdf = write_source_skill(&sources, "pdf");

        rebuild_view(&data_dir, "user_a", "conv_1", &[cron.clone(), pdf])
            .await
            .unwrap();
        rebuild_view(&data_dir, "user_a", "conv_1", &[cron]).await.unwrap();

        let skills_dir = view_skills_dir(&data_dir, "user_a", "conv_1").unwrap();
        assert!(skills_dir.join("cron").exists());
        assert!(
            skills_dir.join("pdf").symlink_metadata().is_err(),
            "a removed skill must not linger"
        );
    }

    /// Rebuilding with an unchanged snapshot must be a no-op from the caller's
    /// point of view: build-task runs it on every session open.
    #[tokio::test]
    async fn rebuild_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        let cron = write_source_skill(&sources, "cron");

        assert_eq!(
            rebuild_view(&data_dir, "user_a", "conv_1", std::slice::from_ref(&cron))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            rebuild_view(&data_dir, "user_a", "conv_1", std::slice::from_ref(&cron))
                .await
                .unwrap(),
            1,
            "a second pass over the same snapshot links the same set, not a duplicate"
        );

        let skills_dir = view_skills_dir(&data_dir, "user_a", "conv_1").unwrap();
        let entries: Vec<String> = std::fs::read_dir(&skills_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["cron"]);
    }

    /// Adding a skill mid-session is the R12 case: the view and index pick it up,
    /// but its supplementary files stay unreadable to an already-running agent
    /// because directory allow-listing is a spawn argument. The rebuild must still
    /// succeed and link the new skill -- the limitation is reported, not enforced.
    #[tokio::test]
    async fn adding_a_skill_to_an_existing_view_still_links_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        let cron = write_source_skill(&sources, "cron");
        let pdf = write_source_skill(&sources, "pdf");

        rebuild_view(&data_dir, "user_a", "conv_1", std::slice::from_ref(&cron))
            .await
            .unwrap();
        assert_eq!(
            rebuild_view(&data_dir, "user_a", "conv_1", &[cron, pdf]).await.unwrap(),
            2
        );

        let skills_dir = view_skills_dir(&data_dir, "user_a", "conv_1").unwrap();
        assert!(skills_dir.join("cron").exists());
        assert!(skills_dir.join("pdf").exists(), "the added skill is linked regardless");
    }

    #[tokio::test]
    async fn a_missing_source_is_skipped_without_failing_the_rest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        let good = write_source_skill(&sources, "cron");
        let missing = ResolvedAgentSkill {
            name: "ghost".to_owned(),
            source_path: sources.join("ghost"),
        };

        let linked = rebuild_view(&data_dir, "user_a", "conv_1", &[good, missing])
            .await
            .unwrap();
        assert_eq!(linked, 1, "one bad source must not cost the other skills");
        let skills_dir = view_skills_dir(&data_dir, "user_a", "conv_1").unwrap();
        assert!(skills_dir.join("cron").exists());
        assert!(skills_dir.join("ghost").symlink_metadata().is_err());
    }

    #[tokio::test]
    async fn remove_view_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        rebuild_view(&data_dir, "user_a", "conv_1", &[write_source_skill(&sources, "cron")])
            .await
            .unwrap();

        assert!(remove_view(&data_dir, "user_a", "conv_1").await.unwrap());
        assert!(!view_dir(&data_dir, "user_a", "conv_1").unwrap().exists());
        assert!(!remove_view(&data_dir, "user_a", "conv_1").await.unwrap());
    }

    /// Removing the view must never follow a skill symlink and delete the real
    /// source tree — that would destroy the user's own skills.
    #[tokio::test]
    async fn remove_view_does_not_touch_the_real_skill_sources() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        let cron = write_source_skill(&sources, "cron");
        rebuild_view(&data_dir, "user_a", "conv_1", std::slice::from_ref(&cron))
            .await
            .unwrap();

        remove_view(&data_dir, "user_a", "conv_1").await.unwrap();

        assert!(cron.source_path.join("SKILL.md").exists(), "the source must survive");
        assert!(cron.source_path.join("references").join("notes.md").exists());
    }

    #[tokio::test]
    async fn orphan_cleanup_keeps_live_conversations_and_drops_the_rest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let sources = tmp.path().join("sources");
        let cron = write_source_skill(&sources, "cron");
        rebuild_view(&data_dir, "user_a", "live", std::slice::from_ref(&cron))
            .await
            .unwrap();
        rebuild_view(&data_dir, "user_a", "dead", std::slice::from_ref(&cron))
            .await
            .unwrap();
        rebuild_view(&data_dir, "user_b", "live", std::slice::from_ref(&cron))
            .await
            .unwrap();

        let live: HashSet<(String, String)> = [
            ("user_a".to_owned(), "live".to_owned()),
            ("user_b".to_owned(), "live".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(cleanup_orphan_views(&data_dir, &live).await.unwrap(), 1);

        assert!(view_dir(&data_dir, "user_a", "live").unwrap().exists());
        assert!(!view_dir(&data_dir, "user_a", "dead").unwrap().exists());
        assert!(
            view_dir(&data_dir, "user_b", "live").unwrap().exists(),
            "cleanup must be keyed by (user, conversation), not by conversation alone"
        );
    }

    /// An empty `live` set means "no conversations exist", which must reap every
    /// view -- but it must still not reach outside the view root.
    #[tokio::test]
    async fn orphan_cleanup_on_a_missing_root_is_not_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("never-created");
        assert_eq!(cleanup_orphan_views(&data_dir, &HashSet::new()).await.unwrap(), 0);
    }

    /// A first turn rebuilds the same view three times, the last two under a
    /// millisecond apart. Sequential calls never exercise that: the destructive
    /// interleaving needs the wipe of one call to land inside another's relink
    /// loop, which is why this bug survived a green unit suite and only appeared
    /// against a real backend (`File exists` link failures plus a `skills 2 /
    /// requested 4` line on a view that held all four).
    ///
    /// The assertion is on the RESULT, not on timing: however the calls
    /// interleave, every requested skill must be linked when they have all
    /// returned, and every call must report the full count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_rebuilds_converge_on_the_full_link_set() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sources = tmp.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        let data_dir = tmp.path().join("data");

        let names = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
        let skills: Vec<ResolvedAgentSkill> = names.iter().map(|n| write_source_skill(&sources, n)).collect();

        // Same (user, conversation) from several tasks at once -- the shape the
        // create / ensure_runtime / send_message trio produces in production.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let data_dir = data_dir.clone();
            let skills = skills.clone();
            handles.push(tokio::spawn(async move {
                rebuild_view(&data_dir, "user_a", "conv_1", &skills).await
            }));
        }

        for handle in handles {
            let linked = handle.await.unwrap().expect("a concurrent rebuild must not fail");
            assert_eq!(
                linked,
                names.len(),
                "every call must report the whole set; a short count means it raced with a wipe"
            );
        }

        let on_disk = existing_link_names(&view_dir(&data_dir, "user_a", "conv_1").unwrap().join(SKILLS_SUBDIR)).await;
        assert_eq!(
            on_disk,
            names.iter().map(|n| (*n).to_owned()).collect::<HashSet<String>>(),
            "the final view must hold exactly the requested skills"
        );
    }

    /// The second and third rebuild of a turn ask for the set the first already
    /// linked. Those must be true no-ops: a wipe-and-relink would briefly empty a
    /// directory a CLI may be reading, and would recompute an "added" count
    /// against a snapshot of our own making.
    #[tokio::test]
    async fn an_unchanged_snapshot_leaves_the_existing_links_in_place() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sources = tmp.path().join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        let data_dir = tmp.path().join("data");
        let skills = vec![
            write_source_skill(&sources, "alpha"),
            write_source_skill(&sources, "beta"),
        ];

        assert_eq!(rebuild_view(&data_dir, "user_a", "conv_1", &skills).await.unwrap(), 2);
        let skills_dir = view_dir(&data_dir, "user_a", "conv_1").unwrap().join(SKILLS_SUBDIR);
        let first_link_ctime = std::fs::symlink_metadata(skills_dir.join("alpha"))
            .unwrap()
            .modified()
            .ok();

        assert_eq!(rebuild_view(&data_dir, "user_a", "conv_1", &skills).await.unwrap(), 2);
        assert_eq!(
            std::fs::symlink_metadata(skills_dir.join("alpha"))
                .unwrap()
                .modified()
                .ok(),
            first_link_ctime,
            "an unchanged rebuild must not have replaced the link"
        );

        // A genuinely changed snapshot still replaces the tree exactly (G2).
        assert_eq!(
            rebuild_view(&data_dir, "user_a", "conv_1", &skills[..1]).await.unwrap(),
            1
        );
        assert_eq!(
            existing_link_names(&skills_dir).await,
            HashSet::from(["alpha".to_owned()]),
            "a dropped skill must actually disappear"
        );
    }
}
