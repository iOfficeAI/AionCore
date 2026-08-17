//! Sidebar classification and assembly engine.
//!
//! One request renders the whole left panel. The service loads a thin read
//! snapshot (conversations / teams / projects / pinned refs), classifies every
//! non-pinned unit into its group (5-case matrix, `api-contract-sidebar.md` §2),
//! windows each group, and hydrates only the windowed conversations in one batch
//! (BR-16, no N+1). It opens no transactions — the stores own SQL/txns — and has
//! zero side effects (BR-17/27): path merge and pseudo-dir grouping are
//! display-only, computed by lexical canonicalization with no filesystem access.
//!
//! Pin truth is a `user_order` row's existence: a unit is excluded from its
//! natural group when it appears in the pinned set (anti-join, BR-7), and shows
//! up only in the pinned group; the DTO `pinned` flag is overridden accordingly
//! (never read from the deprecated `conversations.pinned` column, BR — B1).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use aionui_api_types::{
    ArchiveDeleteResult, ConversationResponse, MoveOrderRequest, OrderItemRefDto, RemoveProjectItem,
    RemoveProjectItemKind, RemoveProjectResult, SidebarGroup, SidebarItem, SidebarItemsResponse, SidebarResponse,
    SidebarScope, SidebarTeamItem,
};
use aionui_common::now_ms;
use aionui_conversation::{is_temp_session_workspace, row_to_response_with_extra};
use aionui_db::models::ConversationRow;
use aionui_db::{
    ArchiveScope, ISidebarStore, IUserOrderStore, MoveOutcome, OrderItemRef, OrderItemType, OrderScene, PinnedCursor,
    SidebarConversationThin, SidebarProjectMeta, SidebarTeamThin,
};
use aionui_project::canonical;

use crate::ports::RemoveProjectPorts;
use crate::types::{
    Cursor, DEFAULT_ITEMS_LIMIT, DEFAULT_LIMIT, ScopeToken, SidebarError, canonical_to_dir_key, validate_limit,
};

/// Project-area hard cap: the interleaved (project + pseudo-dir) group list is
/// truncated to this many groups, ordered by activity (BR-5).
const MAX_PROJECT_GROUPS: usize = 100;

/// Standard-project kind marker (`projects.kind`).
const KIND_STANDARD: &str = "standard";

/// The sidebar read/assembly service.
pub struct SidebarService {
    sidebar: Arc<dyn ISidebarStore>,
    user_order: Arc<dyn IUserOrderStore>,
    /// Backend-managed data dir (conversation workspace root). Used both to
    /// detect temp workspaces (classification) and as the hydration `data_dir`.
    work_dir: PathBuf,
    /// Deletion ports for `remove_project`, injected once after startup (the
    /// team service they wrap is built after this service — see
    /// `build_module_states`). Empty in the read/pin paths, which never touch it.
    remove_project_ports: Arc<OnceLock<Arc<dyn RemoveProjectPorts>>>,
}

impl SidebarService {
    pub fn new(sidebar: Arc<dyn ISidebarStore>, user_order: Arc<dyn IUserOrderStore>, work_dir: PathBuf) -> Self {
        Self {
            sidebar,
            user_order,
            work_dir,
            remove_project_ports: Arc::new(OnceLock::new()),
        }
    }

    /// Install the deletion ports `remove_project` drives. Called once, after the
    /// conversation / team / project services exist; later calls are ignored
    /// (set-once). Because the field is shared behind `Arc`, setting it on any
    /// clone makes it visible everywhere.
    pub fn set_remove_project_ports(&self, ports: Arc<dyn RemoveProjectPorts>) {
        let _ = self.remove_project_ports.set(ports);
    }

    // -- Write side (pin / unpin) --------------------------------------------

    /// Pin an item at the top of a scene. Idempotent. `scene` / `item_type` are
    /// closed enums — an unknown value is a 400, never a silent no-op.
    pub async fn pin(&self, user_id: &str, scene: &str, item_type: &str, item_id: &str) -> Result<(), SidebarError> {
        let scene = parse_scene(scene)?;
        let item = OrderItemRef::new(parse_item_type(item_type)?, item_id.to_owned());
        self.user_order.pin(user_id, scene, &item).await?;
        Ok(())
    }

    /// Unpin an item. Idempotent (unpinning an unpinned item is a no-op).
    pub async fn unpin(&self, user_id: &str, scene: &str, item_type: &str, item_id: &str) -> Result<(), SidebarError> {
        let scene = parse_scene(scene)?;
        let item = OrderItemRef::new(parse_item_type(item_type)?, item_id.to_owned());
        self.user_order.unpin(user_id, scene, &item).await?;
        Ok(())
    }

    // -- Archive (D6: archiving unpins) --------------------------------------

    /// Archive a conversation: move it into the archived slice, then drop any
    /// pinned row (D6 — an archived item is never pinned). A missing / foreign id
    /// is `ScopeGone` (404); the flip is otherwise idempotent for the slice.
    pub async fn archive_conversation(&self, user_id: &str, id: &str) -> Result<(), SidebarError> {
        if !self
            .sidebar
            .set_conversation_archived(user_id, id, Some(now_ms()))
            .await?
        {
            return Err(SidebarError::ScopeGone);
        }
        self.unpin_item(user_id, OrderItemType::Conversation, id).await
    }

    /// Unarchive a conversation back into the active slice. Missing id →
    /// `ScopeGone`. No re-pin: the pinned row was dropped at archive time (D6) and
    /// is not restored.
    pub async fn unarchive_conversation(&self, user_id: &str, id: &str) -> Result<(), SidebarError> {
        if !self.sidebar.set_conversation_archived(user_id, id, None).await? {
            return Err(SidebarError::ScopeGone);
        }
        Ok(())
    }

    /// Archive a team. The store cascades the flip to the team's member
    /// conversations so the folded unit stays in one slice; the team's own pinned
    /// row is then dropped (D6). Missing team → `ScopeGone`.
    pub async fn archive_team(&self, user_id: &str, id: &str) -> Result<(), SidebarError> {
        if !self.sidebar.set_team_archived(user_id, id, Some(now_ms())).await? {
            return Err(SidebarError::ScopeGone);
        }
        self.unpin_item(user_id, OrderItemType::Team, id).await
    }

    /// Unarchive a team (the store cascades its members back). Missing team →
    /// `ScopeGone`.
    pub async fn unarchive_team(&self, user_id: &str, id: &str) -> Result<(), SidebarError> {
        if !self.sidebar.set_team_archived(user_id, id, None).await? {
            return Err(SidebarError::ScopeGone);
        }
        Ok(())
    }

    /// Drop an item's pinned row (idempotent) — the D6 unpin shared by both
    /// archive paths.
    async fn unpin_item(&self, user_id: &str, item_type: OrderItemType, id: &str) -> Result<(), SidebarError> {
        let item = OrderItemRef::new(item_type, id.to_owned());
        self.user_order.unpin(user_id, OrderScene::Pinned, &item).await?;
        Ok(())
    }

    /// Reposition a pinned item by drag-drop (`POST /api/order/{scene}/move`).
    /// `after = None` moves it to the top; otherwise it lands right after
    /// `after`. The store computes the key server-side (BR-26).
    ///
    /// Bad-path mapping (all 400/404, never a silent no-op):
    /// - unknown `scene` / `item_type` → 400 (parse).
    /// - `moved == after` (self-anchor) → 400 (would be a no-op with an
    ///   ambiguous target; reject so the frontend refetches rather than trust a
    ///   stale drag).
    /// - `moved` not pinned → 404 (stale window; the row it dragged is gone).
    /// - `after` not pinned → 400 (stale window; anchor gone — client refetches).
    pub async fn move_order(&self, user_id: &str, scene: &str, req: &MoveOrderRequest) -> Result<(), SidebarError> {
        let scene = parse_scene(scene)?;
        let moved = parse_item_ref(&req.moved)?;
        let after = req.after.as_ref().map(parse_item_ref).transpose()?;

        if after.as_ref() == Some(&moved) {
            return Err(SidebarError::BadRequest(
                "moved and after refer to the same item".into(),
            ));
        }

        match self
            .user_order
            .move_item(user_id, scene, &moved, after.as_ref())
            .await?
        {
            MoveOutcome::Moved => Ok(()),
            MoveOutcome::MovedNotFound => Err(SidebarError::ScopeGone),
            MoveOutcome::AfterNotFound => Err(SidebarError::BadRequest("after anchor is not pinned".into())),
        }
    }

    // -- Project-level operations (BR-19 / D13 "所见即所删") ------------------

    /// Collect the units — teams and independent conversations — that render into
    /// a standard project's group within one archive slice. This is the shared
    /// membership resolution behind every project-level operation
    /// (remove / archive / unarchive / delete-archived).
    ///
    /// The set is computed with the **same** classifier the renderer uses
    /// ([`classify_unit`](Self::classify_unit)): a unit belongs iff it classifies
    /// into `Project(project_id)`. This is why "所见即所删" holds — the set *is*
    /// the render construct, so it also catches path-merged unbound items (a
    /// conversation whose workspace canonicalizes onto this project's root, case
    /// 3) that carry no `project_id`. Pinned units are included: pinning only
    /// hoists a row into the pinned group for display.
    ///
    /// Team-member conversations are not enumerated on their own — they fold into
    /// their team and travel with the team cascade, "无论成员自身 project_id 为何".
    ///
    /// The target must be an owned **standard** project. Temp projects and
    /// pseudo-dir groups are not addressable through this path → `ScopeGone`.
    async fn collect_project_units(
        &self,
        user_id: &str,
        project_id: &str,
        scope: ArchiveScope,
    ) -> Result<(Vec<SidebarTeamThin>, Vec<SidebarConversationThin>), SidebarError> {
        let convs = self.sidebar.list_conversations_thin(user_id, scope).await?;
        let teams = self.sidebar.list_teams_thin(user_id, scope).await?;
        // Projects are never archived; the full enumeration drives both id→meta
        // binding and path-merge in either slice.
        let projects = self.sidebar.list_user_projects(user_id).await?;

        // Same project maps `classify` builds: id→meta and standard-canonical→id.
        let by_id: HashMap<&str, &SidebarProjectMeta> = projects.iter().map(|p| (p.project_id.as_str(), p)).collect();
        let std_canon: HashMap<&str, &str> = projects
            .iter()
            .filter(|p| p.kind == KIND_STANDARD)
            .filter_map(|p| p.workspace_canonical.as_deref().map(|c| (c, p.project_id.as_str())))
            .collect();

        match by_id.get(project_id) {
            Some(meta) if meta.kind == KIND_STANDARD => {}
            _ => return Err(SidebarError::ScopeGone),
        }

        let (team_by_id, independents) = aggregate_teams(convs, teams.clone());
        let target = GroupKey::Project(project_id.to_owned());

        let mut member_teams: Vec<SidebarTeamThin> = Vec::new();
        for team in teams {
            // Skip teams that dropped out of the aggregate (parity with `classify`).
            if !team_by_id.contains_key(&team.id) {
                continue;
            }
            let key = self.classify_unit(
                team.project_id.as_deref(),
                team.workspace.as_deref(),
                &by_id,
                &std_canon,
            );
            if key == target {
                member_teams.push(team);
            }
        }

        let mut member_convs: Vec<SidebarConversationThin> = Vec::new();
        for conv in independents {
            let key = self.classify_unit(
                conv.project_id.as_deref(),
                conv.workspace.as_deref(),
                &by_id,
                &std_canon,
            );
            if key == target {
                member_convs.push(conv);
            }
        }

        Ok((member_teams, member_convs))
    }

    /// Remove a standard project: delete every unit that renders into its group,
    /// then the project record itself. When `dry_run` is set, nothing is deleted
    /// and the returned counts are the preview (what *would* be removed).
    ///
    /// The delete set is [`collect_project_units`](Self::collect_project_units)
    /// over the active slice, so "所见即所删" holds (incl. path-merged unbound
    /// items). Team-member conversations are removed by the team cascade.
    ///
    /// # Atomicity
    /// Deletion is **best-effort per entity**, mirroring the standalone team
    /// delete: killing agent processes, dropping filesystem dirs, and running
    /// cross-service delete hooks cannot share one DB transaction, so D13's
    /// "单事务原子 + 中途失败全回滚" is not literally achievable. A failed unit is
    /// logged and skipped; the project record is dropped last, so a mid-way
    /// failure leaves a smaller (self-consistent) project rather than orphaning
    /// its contents.
    pub async fn remove_project(
        &self,
        user_id: &str,
        project_id: &str,
        dry_run: bool,
    ) -> Result<RemoveProjectResult, SidebarError> {
        // Project removal operates on the active universe; archived rows are not
        // enumerated here (their deletion is the archive page's own batch path).
        let (member_teams, member_convs) = self
            .collect_project_units(user_id, project_id, ArchiveScope::Active)
            .await?;
        let team_ids: Vec<String> = member_teams.iter().map(|t| t.id.clone()).collect();
        let conv_ids: Vec<String> = member_convs.iter().map(|c| c.id.clone()).collect();

        if dry_run {
            // Name the delete set so the confirm dialog can list *which* items go.
            // Pinned members were hoisted into the top pinned group (B1 anti-join),
            // so the frontend can't reconstruct project membership — the names and
            // pinned flags must come from here.
            let pinned_refs = self.user_order.pinned_refs(user_id, OrderScene::Pinned).await?;
            let pinned_set: HashSet<(String, String)> = pinned_refs
                .iter()
                .map(|r| (r.item_type.as_str().to_owned(), r.item_id.clone()))
                .collect();

            let team_name: HashMap<&str, &str> =
                member_teams.iter().map(|t| (t.id.as_str(), t.name.as_str())).collect();
            let conv_resp = self.hydrate(user_id, &conv_ids, ArchiveScope::Active).await?;

            let mut items: Vec<RemoveProjectItem> = Vec::with_capacity(team_ids.len() + conv_ids.len());
            for team_id in &team_ids {
                items.push(RemoveProjectItem {
                    name: team_name.get(team_id.as_str()).copied().unwrap_or_default().to_owned(),
                    pinned: pinned_set.contains(&(OrderItemType::Team.as_str().to_owned(), team_id.clone())),
                    kind: RemoveProjectItemKind::Team,
                });
            }
            for conv_id in &conv_ids {
                items.push(RemoveProjectItem {
                    name: conv_resp.get(conv_id).map(|c| c.name.clone()).unwrap_or_default(),
                    pinned: pinned_set.contains(&(OrderItemType::Conversation.as_str().to_owned(), conv_id.clone())),
                    kind: RemoveProjectItemKind::Conversation,
                });
            }

            return Ok(RemoveProjectResult {
                teams_deleted: team_ids.len() as i64,
                conversations_deleted: conv_ids.len() as i64,
                items,
            });
        }

        let ports = self
            .remove_project_ports
            .get()
            .ok_or_else(|| SidebarError::Internal("remove_project ports not wired".into()))?;

        let mut teams_deleted = 0i64;
        for team_id in &team_ids {
            match ports.remove_team(user_id, team_id).await {
                Ok(()) => teams_deleted += 1,
                Err(err) => {
                    tracing::warn!(team_id = %team_id, error = %err, "remove_project: team delete failed")
                }
            }
        }

        let mut conversations_deleted = 0i64;
        for conv_id in &conv_ids {
            match ports.delete_conversation(user_id, conv_id).await {
                Ok(()) => conversations_deleted += 1,
                Err(err) => {
                    tracing::warn!(conversation_id = %conv_id, error = %err, "remove_project: conversation delete failed")
                }
            }
        }

        // Project record last: its contents are already gone, so a failure here
        // leaves only an empty shell (which a retry finishes off).
        if let Err(err) = ports.delete_project_record(user_id, project_id).await {
            tracing::warn!(project_id = %project_id, error = %err, "remove_project: project record delete failed");
        }

        tracing::info!(
            project_id = %project_id,
            teams_deleted,
            conversations_deleted,
            "Project removed (best-effort)"
        );

        Ok(RemoveProjectResult {
            teams_deleted,
            conversations_deleted,
            items: Vec::new(),
        })
    }

    /// Archive an entire standard project in one request: archive every unit that
    /// renders into its group (teams cascade to members) and unpin each (D6).
    /// Membership is [`collect_project_units`](Self::collect_project_units) over
    /// the active slice, so path-merged unbound conversations are archived too.
    /// Best-effort per unit; a vanished / failed unit is logged and skipped.
    /// Non-standard / foreign project → `ScopeGone`.
    pub async fn archive_project(&self, user_id: &str, project_id: &str) -> Result<(), SidebarError> {
        let (teams, convs) = self
            .collect_project_units(user_id, project_id, ArchiveScope::Active)
            .await?;
        let now = now_ms();

        for team in &teams {
            match self.sidebar.set_team_archived(user_id, &team.id, Some(now)).await {
                Ok(true) => {
                    if let Err(err) = self.unpin_item(user_id, OrderItemType::Team, &team.id).await {
                        tracing::warn!(team_id = %team.id, error = %err, "archive_project: team unpin failed")
                    }
                }
                Ok(false) => tracing::warn!(team_id = %team.id, "archive_project: team vanished before archive"),
                Err(err) => tracing::warn!(team_id = %team.id, error = %err, "archive_project: team archive failed"),
            }
        }

        for conv in &convs {
            match self
                .sidebar
                .set_conversation_archived(user_id, &conv.id, Some(now))
                .await
            {
                Ok(true) => {
                    if let Err(err) = self.unpin_item(user_id, OrderItemType::Conversation, &conv.id).await {
                        tracing::warn!(conversation_id = %conv.id, error = %err, "archive_project: conversation unpin failed")
                    }
                }
                Ok(false) => {
                    tracing::warn!(conversation_id = %conv.id, "archive_project: conversation vanished before archive")
                }
                Err(err) => {
                    tracing::warn!(conversation_id = %conv.id, error = %err, "archive_project: conversation archive failed")
                }
            }
        }

        tracing::info!(
            project_id = %project_id,
            teams = teams.len(),
            conversations = convs.len(),
            "Project archived (best-effort)"
        );
        Ok(())
    }

    /// Restore an entire archived project in one request: unarchive every unit
    /// that renders into its group within the archived slice (teams cascade to
    /// members). Best-effort per unit. Non-standard / foreign project →
    /// `ScopeGone`. No re-pin (D6 dropped the pinned rows at archive time).
    pub async fn unarchive_project(&self, user_id: &str, project_id: &str) -> Result<(), SidebarError> {
        let (teams, convs) = self
            .collect_project_units(user_id, project_id, ArchiveScope::Archived)
            .await?;

        for team in &teams {
            if let Err(err) = self.sidebar.set_team_archived(user_id, &team.id, None).await {
                tracing::warn!(team_id = %team.id, error = %err, "unarchive_project: team unarchive failed")
            }
        }
        for conv in &convs {
            if let Err(err) = self.sidebar.set_conversation_archived(user_id, &conv.id, None).await {
                tracing::warn!(conversation_id = %conv.id, error = %err, "unarchive_project: conversation unarchive failed")
            }
        }

        tracing::info!(
            project_id = %project_id,
            teams = teams.len(),
            conversations = convs.len(),
            "Project unarchived (best-effort)"
        );
        Ok(())
    }

    /// Hard-delete every **archived** unit of a standard project — the units the
    /// archive page renders under that project. Membership is
    /// [`collect_project_units`](Self::collect_project_units) over the archived
    /// slice; teams cascade to their members. Best-effort per entity (same
    /// rationale as [`delete_all_archived`](Self::delete_all_archived)).
    ///
    /// The project **record is intentionally never deleted** — the project stays
    /// (possibly empty) so future work re-binds to it. Non-standard / foreign
    /// project → `ScopeGone`.
    pub async fn delete_archived_project(
        &self,
        user_id: &str,
        project_id: &str,
    ) -> Result<ArchiveDeleteResult, SidebarError> {
        let (teams, convs) = self
            .collect_project_units(user_id, project_id, ArchiveScope::Archived)
            .await?;

        let ports = self
            .remove_project_ports
            .get()
            .ok_or_else(|| SidebarError::Internal("remove_project ports not wired".into()))?;

        let mut teams_deleted = 0i64;
        for team in &teams {
            match ports.remove_team(user_id, &team.id).await {
                Ok(()) => teams_deleted += 1,
                Err(err) => {
                    tracing::warn!(team_id = %team.id, error = %err, "delete_archived_project: team delete failed")
                }
            }
        }

        let mut conversations_deleted = 0i64;
        for conv in &convs {
            match ports.delete_conversation(user_id, &conv.id).await {
                Ok(()) => conversations_deleted += 1,
                Err(err) => {
                    tracing::warn!(conversation_id = %conv.id, error = %err, "delete_archived_project: conversation delete failed")
                }
            }
        }

        tracing::info!(
            project_id = %project_id,
            teams_deleted,
            conversations_deleted,
            "Archived project emptied (best-effort, project record kept)"
        );
        Ok(ArchiveDeleteResult {
            teams_deleted,
            conversations_deleted,
        })
    }

    // -- Empty archive (batch hard-delete) -----------------------------------

    /// Hard-delete **everything** in the archive slice: every archived team (its
    /// member conversations cascade with it) and every independent archived
    /// conversation. Member conversations are not deleted individually — they are
    /// folded into their team and removed by the team cascade, mirroring
    /// [`remove_project`](Self::remove_project)'s accounting.
    ///
    /// Deletion is **best-effort per entity** (same rationale as `remove_project`:
    /// killing agents / dropping dirs / cross-service hooks cannot share one DB
    /// transaction); a failed unit is logged and skipped, and the reported counts
    /// are of units actually removed.
    pub async fn delete_all_archived(&self, user_id: &str) -> Result<ArchiveDeleteResult, SidebarError> {
        let convs = self
            .sidebar
            .list_conversations_thin(user_id, ArchiveScope::Archived)
            .await?;
        let teams = self.sidebar.list_teams_thin(user_id, ArchiveScope::Archived).await?;

        // Fold members into their (archived) team; independents are the loose
        // archived conversations to delete on their own. A member whose team is
        // *not* in the archived slice downgrades to an independent here (BR-8) and
        // is deleted directly rather than left dangling.
        let (team_by_id, independents) = aggregate_teams(convs, teams.clone());
        let team_ids: Vec<&str> = teams
            .iter()
            .filter(|t| team_by_id.contains_key(&t.id))
            .map(|t| t.id.as_str())
            .collect();

        let ports = self
            .remove_project_ports
            .get()
            .ok_or_else(|| SidebarError::Internal("remove_project ports not wired".into()))?;

        let mut teams_deleted = 0i64;
        for team_id in &team_ids {
            match ports.remove_team(user_id, team_id).await {
                Ok(()) => teams_deleted += 1,
                Err(err) => tracing::warn!(team_id = %team_id, error = %err, "empty_archive: team delete failed"),
            }
        }

        let mut conversations_deleted = 0i64;
        for conv in &independents {
            match ports.delete_conversation(user_id, &conv.id).await {
                Ok(()) => conversations_deleted += 1,
                Err(err) => {
                    tracing::warn!(conversation_id = %conv.id, error = %err, "empty_archive: conversation delete failed")
                }
            }
        }

        tracing::info!(teams_deleted, conversations_deleted, "Archive emptied (best-effort)");
        Ok(ArchiveDeleteResult {
            teams_deleted,
            conversations_deleted,
        })
    }

    // -- Delete one archived unit (single hard-delete) -----------------------

    /// Hard-delete a single archived **independent** conversation — the same
    /// unit the archive page renders as its own row. The id is validated against
    /// the archived independents slice (members fold into their team and are
    /// removed via [`delete_archived_team`]); a missing, active, foreign, or
    /// team-member id is [`SidebarError::ScopeGone`], so this path can never
    /// reach an active conversation or split a live team.
    pub async fn delete_archived_conversation(&self, user_id: &str, id: &str) -> Result<(), SidebarError> {
        let convs = self
            .sidebar
            .list_conversations_thin(user_id, ArchiveScope::Archived)
            .await?;
        let teams = self.sidebar.list_teams_thin(user_id, ArchiveScope::Archived).await?;
        let (_team_by_id, independents) = aggregate_teams(convs, teams);
        if !independents.iter().any(|c| c.id == id) {
            return Err(SidebarError::ScopeGone);
        }

        let ports = self
            .remove_project_ports
            .get()
            .ok_or_else(|| SidebarError::Internal("remove_project ports not wired".into()))?;
        ports
            .delete_conversation(user_id, id)
            .await
            .map_err(SidebarError::Internal)?;
        tracing::info!(conversation_id = %id, "Archived conversation deleted");
        Ok(())
    }

    /// Hard-delete a single archived team; its member conversations cascade with
    /// it, mirroring [`delete_all_archived`]. The id is validated against the
    /// archived teams slice — a missing, active, or foreign id is
    /// [`SidebarError::ScopeGone`].
    pub async fn delete_archived_team(&self, user_id: &str, id: &str) -> Result<(), SidebarError> {
        let teams = self.sidebar.list_teams_thin(user_id, ArchiveScope::Archived).await?;
        if !teams.iter().any(|t| t.id == id) {
            return Err(SidebarError::ScopeGone);
        }

        let ports = self
            .remove_project_ports
            .get()
            .ok_or_else(|| SidebarError::Internal("remove_project ports not wired".into()))?;
        ports.remove_team(user_id, id).await.map_err(SidebarError::Internal)?;
        tracing::info!(team_id = %id, "Archived team deleted");
        Ok(())
    }

    // -- Read side (first screen / paging) -----------------------------------

    /// First screen: `pinned → project-area (project + dir interleaved) → chats`.
    /// `win` is the already-parsed per-scope window overrides; `limit` is the
    /// default window for any scope not named in `win`.
    pub async fn first_screen(
        &self,
        user_id: &str,
        limit: Option<i64>,
        win: &[(String, i64)],
        scope: ArchiveScope,
    ) -> Result<SidebarResponse, SidebarError> {
        let default_limit = limit.unwrap_or(DEFAULT_LIMIT);
        let win_map: HashMap<&str, i64> = win.iter().map(|(t, l)| (t.as_str(), *l)).collect();
        let snapshot = self.classify(user_id, scope).await?;

        let mut groups: Vec<SidebarGroup> = Vec::new();

        // Pinned group (active slice only, and only rendered when non-empty). The
        // archive page has no pinned section (D6: archiving unpins).
        if scope == ArchiveScope::Active {
            let pinned_limit = win_map.get("pinned").copied().unwrap_or(default_limit);
            let pinned = self
                .page_pinned(user_id, None, pinned_limit, &snapshot.team_by_id)
                .await?;
            if !pinned.items.is_empty() {
                groups.push(SidebarGroup {
                    scope: SidebarScope::Pinned,
                    items: pinned.items,
                    has_more: pinned.has_more,
                    next_cursor: pinned.next_cursor,
                });
            }
        }

        // Natural groups: project area (already ordered) then chats. Collect the
        // windowed conversation ids across all of them for a single hydration.
        let mut natural: Vec<&NaturalGroup> = snapshot.project_area.iter().collect();
        natural.push(&snapshot.chats);

        let mut conv_ids: Vec<String> = Vec::new();
        let mut windows: Vec<(&NaturalGroup, usize, bool)> = Vec::with_capacity(natural.len());
        for group in natural {
            let lim = win_map.get(group.token_str.as_str()).copied().unwrap_or(default_limit) as usize;
            let has_more = group.items.len() > lim;
            let take = lim.min(group.items.len());
            for item in &group.items[..take] {
                if let GroupItemRef::Conv { id, .. } = item {
                    conv_ids.push(id.clone());
                }
            }
            windows.push((group, take, has_more));
        }

        let hydrated = self.hydrate(user_id, &conv_ids, scope).await?;

        for (group, take, has_more) in windows {
            let items = assemble_items(&group.items[..take], &hydrated, &snapshot.team_by_id, false);
            let next_cursor = has_more
                .then(|| {
                    group
                        .items
                        .get(take - 1)
                        .map(|i| i.activity_cursor().encode(&group.token))
                })
                .flatten();
            groups.push(SidebarGroup {
                scope: group.scope.clone(),
                items,
                has_more,
                next_cursor,
            });
        }

        Ok(SidebarResponse {
            groups,
            has_more_groups: snapshot.has_more_groups,
        })
    }

    /// Page one more window of a single group.
    pub async fn items(
        &self,
        user_id: &str,
        scope: &str,
        cursor: Option<&str>,
        limit: Option<i64>,
        archive: ArchiveScope,
    ) -> Result<SidebarItemsResponse, SidebarError> {
        let token =
            ScopeToken::parse(scope).ok_or_else(|| SidebarError::BadRequest(format!("unknown scope: {scope}")))?;
        let limit = limit.unwrap_or(DEFAULT_ITEMS_LIMIT);
        validate_limit(limit)?;

        if let ScopeToken::Pinned = token {
            // Pinned is an active-only group; the archive slice has no such scope,
            // so a pinned page request against it targets a group that isn't there.
            if archive != ArchiveScope::Active {
                return Err(SidebarError::ScopeGone);
            }
            let after = match cursor {
                Some(raw) => Some(to_pinned_cursor(Cursor::decode(raw, &token)?)?),
                None => None,
            };
            let team_by_id = self.load_team_aggregates(user_id, ArchiveScope::Active).await?;
            let page = self.page_pinned(user_id, after.as_ref(), limit, &team_by_id).await?;
            return Ok(SidebarItemsResponse {
                items: page.items,
                has_more: page.has_more,
                next_cursor: page.next_cursor,
            });
        }

        // Natural scope: classify, find the named group (stale → 404), page it.
        let snapshot = self.classify(user_id, archive).await?;
        let group = match &token {
            ScopeToken::Chats => &snapshot.chats,
            _ => snapshot
                .project_area
                .iter()
                .find(|g| g.token == token)
                .ok_or(SidebarError::ScopeGone)?,
        };
        let cursor = match cursor {
            Some(raw) => Some(Cursor::decode(raw, &token)?),
            None => None,
        };
        self.page_natural(user_id, group, cursor.as_ref(), limit, archive).await
    }

    /// Window a natural group after `cursor`, hydrate, and assemble.
    async fn page_natural(
        &self,
        user_id: &str,
        group: &NaturalGroup,
        cursor: Option<&Cursor>,
        limit: i64,
        scope: ArchiveScope,
    ) -> Result<SidebarItemsResponse, SidebarError> {
        // Items are sorted later-first, so the "after cursor" set is a suffix.
        let after: Vec<&GroupItemRef> = group.items.iter().filter(|i| i.is_after(cursor)).collect();
        let take = (limit as usize).min(after.len());
        let has_more = after.len() > limit as usize;

        let conv_ids: Vec<String> = after[..take]
            .iter()
            .filter_map(|i| match i {
                GroupItemRef::Conv { id, .. } => Some(id.clone()),
                GroupItemRef::Team { .. } => None,
            })
            .collect();
        let hydrated = self.hydrate(user_id, &conv_ids, scope).await?;

        let window: Vec<GroupItemRef> = after[..take].iter().map(|i| (*i).clone()).collect();
        let items = assemble_items(&window, &hydrated, &group.team_by_id_ref(), false);
        let next_cursor = has_more
            .then(|| window.last().map(|i| i.activity_cursor().encode(&group.token)))
            .flatten();
        Ok(SidebarItemsResponse {
            items,
            has_more,
            next_cursor,
        })
    }

    /// Window the pinned scene after `cursor` (order_key ascending) and assemble.
    async fn page_pinned(
        &self,
        user_id: &str,
        after: Option<&PinnedCursor>,
        limit: i64,
        team_by_id: &HashMap<String, TeamAgg>,
    ) -> Result<PinnedPage, SidebarError> {
        let rows = self
            .user_order
            .list_pinned(user_id, OrderScene::Pinned, after, limit + 1)
            .await?;
        let has_more = rows.len() as i64 > limit;
        let window = &rows[..(limit as usize).min(rows.len())];

        // Path-4 read-side defense (design §4.3): a live team member never has an
        // independent row — it is folded into its team. The only teamId writer
        // (`build_team_extra`) creates fresh conversations, so a member cannot
        // carry a pre-existing pinned row through normal flows; this guard closes
        // the residual dirty-data / future-write hole and self-heals, so no
        // write-side chokepoint is needed.
        let member_ids: HashSet<&str> = team_by_id
            .values()
            .flat_map(|t| t.member_ids.iter())
            .map(String::as_str)
            .collect();

        let conv_ids: Vec<String> = window
            .iter()
            .filter(|r| r.item_type == OrderItemType::Conversation.as_str() && !member_ids.contains(r.item_id.as_str()))
            .map(|r| r.item_id.clone())
            .collect();
        // The pinned scene is active-only (D6: archiving unpins), so its members
        // always hydrate from the active slice.
        let hydrated = self.hydrate(user_id, &conv_ids, ArchiveScope::Active).await?;

        let mut items: Vec<SidebarItem> = Vec::with_capacity(window.len());
        for row in window {
            if row.item_type == OrderItemType::Conversation.as_str() {
                if let Some(resp) = hydrated.get(&row.item_id) {
                    items.push(conversation_item(resp.clone(), true));
                }
            } else if let Some(agg) = team_by_id.get(&row.item_id) {
                items.push(team_item(agg, true));
            }
        }

        let next_cursor = if has_more {
            window.last().and_then(|row| {
                OrderItemType::parse(&row.item_type).map(|t| {
                    Cursor::Pinned {
                        order_key: row.order_key,
                        item_type: t.as_str().to_owned(),
                        item_id: row.item_id.clone(),
                    }
                    .encode(&ScopeToken::Pinned)
                })
            })
        } else {
            None
        };

        Ok(PinnedPage {
            items,
            has_more,
            next_cursor,
        })
    }

    /// One batched hydration of windowed conversations into response DTOs. A row
    /// with unparsable `extra` is skipped (warn) rather than failing the request.
    async fn hydrate(
        &self,
        user_id: &str,
        ids: &[String],
        scope: ArchiveScope,
    ) -> Result<HashMap<String, ConversationResponse>, SidebarError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = self.sidebar.hydrate_conversations(user_id, ids, scope).await?;
        let mut out = HashMap::with_capacity(rows.len());
        for row in rows {
            let id = row.id.clone();
            match parse_row(row, &self.work_dir) {
                Ok(resp) => {
                    out.insert(id, resp);
                }
                Err(err) => {
                    tracing::warn!(conversation_id = %id, error = %err, "sidebar: skipping unhydratable conversation")
                }
            }
        }
        Ok(out)
    }

    // -- Classification ------------------------------------------------------

    /// Build team aggregates only (used by the pinned items path, which needs no
    /// group classification). Members are folded into their team; orphan members
    /// downgrade to independents (BR-8) but independents are discarded here.
    async fn load_team_aggregates(
        &self,
        user_id: &str,
        scope: ArchiveScope,
    ) -> Result<HashMap<String, TeamAgg>, SidebarError> {
        let convs = self.sidebar.list_conversations_thin(user_id, scope).await?;
        let teams = self.sidebar.list_teams_thin(user_id, scope).await?;
        Ok(aggregate_teams(convs, teams).0)
    }

    /// Full classification snapshot: team aggregates, independents + teams sorted
    /// into groups (5-case matrix), the ordered/truncated project area, and the
    /// chats group.
    async fn classify(&self, user_id: &str, scope: ArchiveScope) -> Result<Snapshot, SidebarError> {
        let convs = self.sidebar.list_conversations_thin(user_id, scope).await?;
        let teams = self.sidebar.list_teams_thin(user_id, scope).await?;
        // Projects are never archived: the whole enumeration drives grouping and
        // path-merge in both slices.
        let projects = self.sidebar.list_user_projects(user_id).await?;
        // Pins are an active-only concept (D6: archiving unpins). In the archived
        // slice no row is pinned, so the pinned set is empty and the pinned-refs
        // query (global, not archive-scoped) is skipped entirely.
        let pinned_set: HashSet<(String, String)> = match scope {
            ArchiveScope::Active => {
                let pinned_refs = self.user_order.pinned_refs(user_id, OrderScene::Pinned).await?;
                pinned_refs
                    .iter()
                    .map(|r| (r.item_type.as_str().to_owned(), r.item_id.clone()))
                    .collect()
            }
            ArchiveScope::Archived => HashSet::new(),
        };

        // Project maps: id→meta and standard-canonical→id (path merge, BR case 3).
        let by_id: HashMap<&str, &SidebarProjectMeta> = projects.iter().map(|p| (p.project_id.as_str(), p)).collect();
        let std_canon: HashMap<&str, &str> = projects
            .iter()
            .filter(|p| p.kind == KIND_STANDARD)
            .filter_map(|p| p.workspace_canonical.as_deref().map(|c| (c, p.project_id.as_str())))
            .collect();

        let (team_by_id, independents) = aggregate_teams(convs, teams.clone());

        let mut builder = GroupBuilder::new();

        // Teams as units.
        for team in &teams {
            let agg = match team_by_id.get(&team.id) {
                Some(a) => a,
                None => continue,
            };
            let key = self.classify_unit(
                team.project_id.as_deref(),
                team.workspace.as_deref(),
                &by_id,
                &std_canon,
            );
            let pinned = pinned_set.contains(&(OrderItemType::Team.as_str().to_owned(), team.id.clone()));
            builder.push(
                key,
                &by_id,
                agg.updated_at,
                pinned,
                GroupItemRef::Team {
                    team_id: team.id.clone(),
                    updated_at: agg.updated_at,
                },
            );
        }

        // Independent conversations as units.
        for conv in &independents {
            let key = self.classify_unit(
                conv.project_id.as_deref(),
                conv.workspace.as_deref(),
                &by_id,
                &std_canon,
            );
            let pinned = pinned_set.contains(&(OrderItemType::Conversation.as_str().to_owned(), conv.id.clone()));
            builder.push(
                key,
                &by_id,
                conv.updated_at,
                pinned,
                GroupItemRef::Conv {
                    id: conv.id.clone(),
                    updated_at: conv.updated_at,
                },
            );
        }

        // Spine: every standard project surfaces even with zero units (BR-5) —
        // active slice only. The archive page lists only projects that actually
        // hold archived items, so an empty standard project must not appear there.
        if scope == ArchiveScope::Active {
            for project in projects.iter().filter(|p| p.kind == KIND_STANDARD) {
                builder.ensure_standard_spine(project);
            }
        }

        let (project_area, chats, has_more_groups) = builder.finish(team_by_id.clone());
        Ok(Snapshot {
            project_area,
            chats,
            has_more_groups,
            team_by_id,
        })
    }

    /// Classify one unit (`project_id`, `workspace`) into its group key. Pure and
    /// filesystem-free: dangling project ids fall through to the path branch, and
    /// any path we cannot lexically canonicalize degrades to chats.
    fn classify_unit(
        &self,
        project_id: Option<&str>,
        workspace: Option<&str>,
        by_id: &HashMap<&str, &SidebarProjectMeta>,
        std_canon: &HashMap<&str, &str>,
    ) -> GroupKey {
        if let Some(pid) = project_id
            && let Some(meta) = by_id.get(pid)
        {
            return if meta.kind == KIND_STANDARD {
                GroupKey::Project(pid.to_owned())
            } else {
                GroupKey::Chats
            };
        }
        // No project_id, or a dangling id (foreign / deleted project): fall
        // through to path-based classification.
        let ws = match workspace {
            Some(ws) if !ws.is_empty() => ws,
            _ => return GroupKey::Chats,
        };
        let ws_path = Path::new(ws);
        if is_temp_session_workspace(ws_path) {
            return GroupKey::Chats;
        }
        let uri = match canonical::to_file_uri(ws_path) {
            Ok(u) => u,
            Err(_) => return GroupKey::Chats,
        };
        let canon = match canonical::canonicalize(&uri) {
            Ok(c) => c,
            Err(_) => return GroupKey::Chats,
        };
        match std_canon.get(canon.as_str()) {
            Some(pid) => GroupKey::Project((*pid).to_owned()),
            None => GroupKey::Dir(canon.as_str().to_owned()),
        }
    }
}

// -- Snapshot / group model --------------------------------------------------

struct Snapshot {
    project_area: Vec<NaturalGroup>,
    chats: NaturalGroup,
    has_more_groups: bool,
    team_by_id: HashMap<String, TeamAgg>,
}

/// A team collapsed into one sidebar row.
#[derive(Clone)]
struct TeamAgg {
    team_id: String,
    name: String,
    /// `MAX(updated_at)` over active members, else the team's own `updated_at`.
    updated_at: i64,
    /// Active member conversation ids, `created_at` ascending.
    member_ids: Vec<String>,
}

/// A finished natural group: ordered non-pinned item refs plus the DTO scope.
struct NaturalGroup {
    scope: SidebarScope,
    token: ScopeToken,
    token_str: String,
    /// Sorted later-first (updated_at DESC, then item-type ASC, then id ASC).
    items: Vec<GroupItemRef>,
    /// Shared team aggregates (set at finish); assembly reads names/members here.
    team_by_id: Arc<HashMap<String, TeamAgg>>,
}

impl NaturalGroup {
    fn team_by_id_ref(&self) -> Arc<HashMap<String, TeamAgg>> {
        self.team_by_id.clone()
    }
}

#[derive(Clone)]
enum GroupItemRef {
    Conv { id: String, updated_at: i64 },
    Team { team_id: String, updated_at: i64 },
}

impl GroupItemRef {
    fn updated_at(&self) -> i64 {
        match self {
            GroupItemRef::Conv { updated_at, .. } | GroupItemRef::Team { updated_at, .. } => *updated_at,
        }
    }

    fn type_ord(&self) -> u8 {
        match self {
            GroupItemRef::Conv { .. } => 0,
            GroupItemRef::Team { .. } => 1,
        }
    }

    fn item_id(&self) -> &str {
        match self {
            GroupItemRef::Conv { id, .. } => id,
            GroupItemRef::Team { team_id, .. } => team_id,
        }
    }

    fn item_type_str(&self) -> &'static str {
        match self {
            GroupItemRef::Conv { .. } => OrderItemType::Conversation.as_str(),
            GroupItemRef::Team { .. } => OrderItemType::Team.as_str(),
        }
    }

    fn activity_cursor(&self) -> Cursor {
        Cursor::Activity {
            updated_at: self.updated_at(),
            item_type: self.item_type_str().to_owned(),
            item_id: self.item_id().to_owned(),
        }
    }

    /// Is this item strictly after `cursor` in later-first order? `None` cursor
    /// means "from the top" (everything qualifies).
    ///
    /// Later-first: a comes before b iff `a.updated_at > b.updated_at`, or on a
    /// tie iff `(a.type_ord, a.id) < (b.type_ord, b.id)`. So "after the cursor"
    /// is: smaller `updated_at`, or equal `updated_at` with a larger
    /// `(type_ord, id)` pair.
    fn is_after(&self, cursor: Option<&Cursor>) -> bool {
        let Some(Cursor::Activity {
            updated_at,
            item_type,
            item_id,
        }) = cursor
        else {
            return true;
        };
        let cur = (type_ord_of(item_type), item_id.as_str());
        let this = (self.type_ord(), self.item_id());
        self.updated_at() < *updated_at || (self.updated_at() == *updated_at && this > cur)
    }
}

/// Group key used only during classification (before scope DTOs are built).
#[derive(Clone, PartialEq, Eq, Hash)]
enum GroupKey {
    Project(String),
    Dir(String),
    Chats,
}

/// Accumulates units into groups, tracking activity for ordering.
struct GroupBuilder {
    map: HashMap<GroupKey, GroupAccum>,
}

struct GroupAccum {
    items: Vec<GroupItemRef>,
    /// MAX(updated_at) over ALL units incl pinned (group order, BR-6).
    latest_activity: Option<i64>,
    /// created_at fallback for empty standard-project groups.
    created_at: i64,
    /// Cached DTO scope + tokens (filled when the group is first created).
    scope: SidebarScope,
    token: ScopeToken,
    token_str: String,
}

impl GroupBuilder {
    fn new() -> Self {
        Self { map: HashMap::new() }
    }

    fn push(
        &mut self,
        key: GroupKey,
        by_id: &HashMap<&str, &SidebarProjectMeta>,
        updated_at: i64,
        pinned: bool,
        item: GroupItemRef,
    ) {
        let accum = self
            .map
            .entry(key.clone())
            .or_insert_with(|| GroupAccum::new(&key, by_id));
        accum.latest_activity = Some(accum.latest_activity.map_or(updated_at, |a| a.max(updated_at)));
        if !pinned {
            accum.items.push(item);
        }
    }

    fn ensure_standard_spine(&mut self, project: &SidebarProjectMeta) {
        let key = GroupKey::Project(project.project_id.clone());
        self.map.entry(key).or_insert_with(|| GroupAccum {
            items: Vec::new(),
            latest_activity: None,
            created_at: project.created_at,
            scope: project_scope(project),
            token: ScopeToken::Project(project.project_id.clone()),
            token_str: format!("project:{}", project.project_id),
        });
    }

    /// Split into ordered project area (truncated to MAX_PROJECT_GROUPS) + chats.
    fn finish(self, team_by_id: HashMap<String, TeamAgg>) -> (Vec<NaturalGroup>, NaturalGroup, bool) {
        let shared = Arc::new(team_by_id);
        let mut chats: Option<GroupAccum> = None;
        let mut area: Vec<GroupAccum> = Vec::new();
        for (key, accum) in self.map {
            match key {
                GroupKey::Chats => chats = Some(accum),
                _ => area.push(accum),
            }
        }

        area.sort_by(cmp_group);
        let has_more_groups = area.len() > MAX_PROJECT_GROUPS;
        area.truncate(MAX_PROJECT_GROUPS);

        let project_area: Vec<NaturalGroup> = area.into_iter().map(|a| a.into_group(shared.clone())).collect();
        let chats = chats
            .unwrap_or_else(|| GroupAccum {
                items: Vec::new(),
                latest_activity: None,
                created_at: 0,
                scope: SidebarScope::Chats,
                token: ScopeToken::Chats,
                token_str: "chats".to_owned(),
            })
            .into_group(shared);
        (project_area, chats, has_more_groups)
    }
}

impl GroupAccum {
    fn new(key: &GroupKey, by_id: &HashMap<&str, &SidebarProjectMeta>) -> Self {
        match key {
            GroupKey::Project(pid) => {
                // Invariant: a Project key only arises from a resolved standard
                // project (direct binding or path merge), so meta is present.
                let meta = by_id.get(pid.as_str()).expect("project key without meta");
                GroupAccum {
                    items: Vec::new(),
                    latest_activity: None,
                    created_at: meta.created_at,
                    scope: project_scope(meta),
                    token: ScopeToken::Project(pid.clone()),
                    token_str: format!("project:{pid}"),
                }
            }
            GroupKey::Dir(canonical) => {
                let (path, name) = dir_display(canonical);
                GroupAccum {
                    items: Vec::new(),
                    latest_activity: None,
                    created_at: 0,
                    scope: SidebarScope::Dir {
                        key: canonical_to_dir_key(canonical),
                        path,
                        name,
                    },
                    token: ScopeToken::Dir(canonical.clone()),
                    token_str: format!("dir:{}", canonical_to_dir_key(canonical)),
                }
            }
            GroupKey::Chats => GroupAccum {
                items: Vec::new(),
                latest_activity: None,
                created_at: 0,
                scope: SidebarScope::Chats,
                token: ScopeToken::Chats,
                token_str: "chats".to_owned(),
            },
        }
    }

    fn into_group(mut self, team_by_id: Arc<HashMap<String, TeamAgg>>) -> NaturalGroup {
        self.items.sort_by(cmp_item);
        NaturalGroup {
            scope: self.scope,
            token: self.token,
            token_str: self.token_str,
            items: self.items,
            team_by_id,
        }
    }
}

// -- Free helpers ------------------------------------------------------------

/// Fold conversations into their live team; return (team aggregates, independents).
/// A member whose `team_id` names no live team is an orphan and downgrades to an
/// independent row (BR-8).
fn aggregate_teams(
    convs: Vec<SidebarConversationThin>,
    teams: Vec<SidebarTeamThin>,
) -> (HashMap<String, TeamAgg>, Vec<SidebarConversationThin>) {
    let live: HashSet<&str> = teams.iter().map(|t| t.id.as_str()).collect();
    let mut members: HashMap<String, Vec<SidebarConversationThin>> = HashMap::new();
    let mut independents: Vec<SidebarConversationThin> = Vec::new();
    for conv in convs {
        match &conv.team_id {
            Some(tid) if live.contains(tid.as_str()) => members.entry(tid.clone()).or_default().push(conv),
            _ => independents.push(conv),
        }
    }

    let mut team_by_id = HashMap::with_capacity(teams.len());
    for team in teams {
        let mut mem = members.remove(&team.id).unwrap_or_default();
        mem.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        let updated_at = mem.iter().map(|c| c.updated_at).max().unwrap_or(team.updated_at);
        let member_ids = mem.into_iter().map(|c| c.id).collect();
        team_by_id.insert(
            team.id.clone(),
            TeamAgg {
                team_id: team.id,
                name: team.name,
                updated_at,
                member_ids,
            },
        );
    }
    (team_by_id, independents)
}

/// Group order (BR-6): active groups by latest_activity DESC; empty standard
/// projects sink below and order by created_at DESC; ties break by token DESC.
fn cmp_group(a: &GroupAccum, b: &GroupAccum) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a.latest_activity, b.latest_activity) {
        (Some(x), Some(y)) => y.cmp(&x).then_with(|| b.token_str.cmp(&a.token_str)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => b
            .created_at
            .cmp(&a.created_at)
            .then_with(|| b.token_str.cmp(&a.token_str)),
    }
}

/// Item order within a group: later-first (updated_at DESC), tie-break
/// (item_type ASC, id ASC) — matching the keyset cursor's "strictly after".
fn cmp_item(a: &GroupItemRef, b: &GroupItemRef) -> std::cmp::Ordering {
    b.updated_at()
        .cmp(&a.updated_at())
        .then_with(|| a.type_ord().cmp(&b.type_ord()))
        .then_with(|| a.item_id().cmp(b.item_id()))
}

fn type_ord_of(item_type: &str) -> u8 {
    match item_type {
        t if t == OrderItemType::Conversation.as_str() => 0,
        t if t == OrderItemType::Team.as_str() => 1,
        _ => u8::MAX,
    }
}

fn project_scope(meta: &SidebarProjectMeta) -> SidebarScope {
    SidebarScope::Project {
        project_id: meta.project_id.clone(),
        name: meta.name.clone(),
        workspace: meta.workspace_uri.clone(),
    }
}

/// Human display path + last-segment name for a pseudo-dir canonical URI.
fn dir_display(canonical: &str) -> (String, String) {
    match canonical::uri_to_path(canonical) {
        Ok(path) => {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            (path.display().to_string(), name)
        }
        Err(_) => (canonical.to_owned(), canonical.to_owned()),
    }
}

fn assemble_items(
    window: &[GroupItemRef],
    hydrated: &HashMap<String, ConversationResponse>,
    team_by_id: &HashMap<String, TeamAgg>,
    pinned: bool,
) -> Vec<SidebarItem> {
    let mut out = Vec::with_capacity(window.len());
    for item in window {
        match item {
            GroupItemRef::Conv { id, .. } => {
                if let Some(resp) = hydrated.get(id) {
                    out.push(conversation_item(resp.clone(), pinned));
                }
            }
            GroupItemRef::Team { team_id, .. } => {
                if let Some(agg) = team_by_id.get(team_id) {
                    out.push(team_item(agg, pinned));
                }
            }
        }
    }
    out
}

fn conversation_item(mut resp: ConversationResponse, pinned: bool) -> SidebarItem {
    resp.pinned = pinned;
    resp.pinned_at = None;
    SidebarItem::Conversation { conversation: resp }
}

fn team_item(agg: &TeamAgg, pinned: bool) -> SidebarItem {
    SidebarItem::Team(SidebarTeamItem {
        team_id: agg.team_id.clone(),
        name: agg.name.clone(),
        updated_at: agg.updated_at,
        pinned,
        member_conversation_ids: agg.member_ids.clone(),
    })
}

fn parse_row(row: ConversationRow, work_dir: &Path) -> Result<ConversationResponse, SidebarError> {
    let extra: serde_json::Value =
        serde_json::from_str(&row.extra).map_err(|e| SidebarError::Internal(format!("invalid extra JSON: {e}")))?;
    row_to_response_with_extra(row, extra, work_dir).map_err(|e| SidebarError::Internal(e.to_string()))
}

fn parse_scene(scene: &str) -> Result<OrderScene, SidebarError> {
    OrderScene::parse(scene).ok_or_else(|| SidebarError::BadRequest(format!("unknown scene: {scene}")))
}

fn parse_item_type(item_type: &str) -> Result<OrderItemType, SidebarError> {
    OrderItemType::parse(item_type).ok_or_else(|| SidebarError::BadRequest(format!("unknown item type: {item_type}")))
}

/// Parse an `OrderItemRefDto` body field into a validated `OrderItemRef`; an
/// unknown `item_type` is a 400 (mirroring the pin/unpin path params).
fn parse_item_ref(dto: &OrderItemRefDto) -> Result<OrderItemRef, SidebarError> {
    Ok(OrderItemRef::new(parse_item_type(&dto.item_type)?, dto.item_id.clone()))
}

fn to_pinned_cursor(cursor: Cursor) -> Result<PinnedCursor, SidebarError> {
    match cursor {
        Cursor::Pinned {
            order_key,
            item_type,
            item_id,
        } => {
            let item_type = OrderItemType::parse(&item_type)
                .ok_or_else(|| SidebarError::BadRequest(format!("unknown cursor item type: {item_type}")))?;
            Ok(PinnedCursor {
                order_key,
                item_type,
                item_id,
            })
        }
        Cursor::Activity { .. } => Err(SidebarError::BadRequest("expected a pinned cursor".into())),
    }
}

struct PinnedPage {
    items: Vec<SidebarItem>,
    has_more: bool,
    next_cursor: Option<String>,
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
