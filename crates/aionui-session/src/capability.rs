//! `Capabilities` — a backend's declared capability (BackendAdapter
//! responsibility 3). RFC §8 three tiers + the signal set it can emit. Read by
//! the adapter only — NEVER by the pure reducer `step()` (I12).

/// Backend capability declaration (responsibility 3). RFC §8.
///
/// 007 §C6: extended from 2 → 10 fields. ALL new fields are additive with
/// `Default`; the reducer NEVER reads `Capabilities` (I12), so this is a pure
/// a-side/UI surface. The split is "fixed enums (compile-time) vs runtime
/// discovery (handshake-filled open sets)" — these fields are the DISCOVERY
/// half: which Commands a backend accepts, which input blocks, which models/
/// modes it advertises, which auth methods (§0.5 panel).
/// When an accepted mode switch starts governing tool approvals. See
/// [`Capabilities::mode_switch_effect`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModeSwitchEffect {
    /// In force for the very next approval decision, even mid-turn.
    ///
    /// The default: a backend that reconfigures synchronously (aionrs mutates the shared
    /// approval mode in-process) or writes straight to a live agent needs no ceremony,
    /// and this keeps existing backends reporting what they already reported.
    #[default]
    Immediate,
    /// The in-flight turn keeps the old mode; the switch governs from the next turn.
    NextTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Parse tier: Parsed (full-field, e.g. claude) / Hook (lifecycle hooks
    /// only) / Opaque (pure black box).
    pub tier: CapabilityTier,
    /// Which canonical signals this backend can emit (missing heartbeat ⇒
    /// janitor degrades to plain idle-timeout).
    pub emits: SignalSet,
    /// NEW (007 §5.2/§C6): which `Command`s this backend accepts. UI gates
    /// affordances up-front; dispatch rejects unsupported with CommandNotSupported.
    pub supported_commands: CommandSet,
    /// NEW (007 §5.3): which `ContentBlock` kinds `Send` accepts (text-only
    /// backend never offered an image picker).
    pub prompt_blocks: BlockSet,
    /// NEW (007 §1.2/Addendum 3): whether this backend emits a real wire
    /// PromptAccepted, the adapter synthesizes one, or none.
    pub prompt_accepted: PromptAcceptedSource,
    /// NEW (007 §9.10/Addendum 7): models this backend advertises (handshake-
    /// filled; UI renders the switcher). Open set, not a fixed enum.
    pub available_models: Vec<ModelInfo>,
    /// NEW (007 §9.10): modes this backend advertises. Open set, discovery-filled —
    /// codex (feature 012) carries the permission-profile ids `permissionProfile/list`
    /// advertises. The three BUILT-IN tiers are surfaced as the LEGACY bare tokens the old
    /// ACP path used (`:read-only`→`read-only`, `:workspace`→`auto`, `:danger-full-access`
    /// →`full-access`) so the frontend receives byte-identical values and needs zero change
    /// (feature 012 "Plan B"); a user-custom `[permissions.<id>]` profile has no bare form
    /// and keeps its colon id. The colon↔bare pair `profile_id_to_legacy_value`↔
    /// `normalize_to_profile_id` is the sole translation, mirroring legacy ACP's
    /// `apply_advertised_modes` + `normalize_requested_mode`. codex has no separately-exposed
    /// collaborationMode selector; its mode axis IS the permission axis (see codex_conn
    /// `fill_discovery` Permissions arm).
    pub available_modes: Vec<ModeInfo>,
    /// NEW (007 §9.10): currently-selected model (persisted with the Node;
    /// preloaded on resume).
    pub current_model: Option<String>,
    /// NEW (007 §9.10): currently-selected mode.
    pub current_mode: Option<String>,
    /// CP-1: currently-selected reasoning effort (thinking strength), when the
    /// backend has an effort axis (claude `effortLevel`, per-model
    /// `supportedEffortLevels`). UNLIKE `current_model`/`current_mode` (which the
    /// backend reports back), effort is NOT echoed by claude — the backend remembers
    /// the last value it set via `SetConfigOption{effort}` and surfaces it here so the
    /// picker highlights the active level. `None` = no effort axis / never set.
    pub current_effort: Option<String>,
    /// NEW (007 §9.17/Addendum 9): auth methods. NON-EMPTY ⇒ this backend can
    /// raise a mid-session `Permission{kind:Auth}` and accept `AnswerAuth` →
    /// enables the `waiting_on_auth` counter path. Empty = no mid-session auth.
    pub auth_methods: Vec<String>,
    /// NEW (009 R2/§1): whether this backend accepts a PROACTIVE next-turn input
    /// while a turn is in flight — the capability `can_queue` gates on. TRUE only
    /// when the conversation layer can hand the backend a queued message that it
    /// will run as the next turn (claude's persistent stdin FIFO: a write is
    /// buffered and consumed as the following turn). FALSE for backends that have
    /// no such proactive-input path from the conversation layer.
    ///
    /// ⚠️ This is DELIBERATELY NOT `supported_commands.steer`. Steer is a
    /// mid-turn soft-injection wire (codex advertises `steer:true`) but the
    /// conversation layer does not route Steer today, so keying `can_queue` off
    /// `steer` would light the queue affordance for codex while nothing dispatches
    /// it — a dead button (MX-QUEUE-3). `can_queue` MUST read this bit, never
    /// `caps.steer`. Default false; only claude sets it true.
    pub accepts_proactive_input: bool,
    /// Whether a message written while a turn is IN FLIGHT reaches the agent
    /// without waiting for that turn to end.
    ///
    /// ⚠️ DELIBERATELY SEPARATE from `accepts_proactive_input`, whose meaning
    /// varies by backend: agy sets it true merely to keep the input box usable
    /// (one process per turn — it ignores stdin mid-turn), while claude sets it
    /// true because a mid-turn write is genuinely consumed. Gating any UI
    /// affordance on `accepts_proactive_input` would light a mid-turn control on
    /// agy that it cannot honour (the MX-QUEUE-3 dead button).
    ///
    /// This bit is the ONLY one the wire and the frontend may read. Default
    /// false so a newly integrated backend behaves like ACP until proven.
    pub supports_midturn_delivery: bool,
    /// When a mode switch this backend ACCEPTS starts governing tool approvals.
    ///
    /// Reported per call rather than statically, because the same backend answers
    /// differently depending on what it is doing: claude writes a `set_permission_mode`
    /// straight to stdin while idle, but QUEUES one raised mid-turn until the next
    /// prompt. codex is `NextTurn` unconditionally — its schema says so
    /// ("Override the approval policy for subsequent turns",
    /// samples/codex-cli/0.146.0/schema/v2/ThreadSettingsUpdateParams.json).
    ///
    /// Consumed by `set_config_option` to decide between `Observed` and
    /// `PendingNextTurn`; without it the response was self-fulfilling (cache the request
    /// as an override, read it straight back, report success).
    pub mode_switch_effect: ModeSwitchEffect,
    /// NEW (#101): user-invokable slash commands the backend advertises (claude
    /// `control_request{initialize}` response `commands[]`; ACP `session/update`
    /// `available_commands_update`; incl. MCP/plugin/skill-derived). Open set,
    /// discovery-filled, additive (Default empty). The reducer NEVER reads it (I12)
    /// — pure a-side/UI surface, projected by the REST get_slash_commands read.
    pub slash_commands: Vec<SlashCommandInfo>,
}

/// #101: an advertised slash command (discovery, open set). `description` is the
/// human-readable summary; claude/ACP both carry it (claude also has an argument
/// hint we drop — the conversation layer's `SlashCommandItem` has no field for it).
/// serde-derived so `SessionEvent::CatalogUpdated` can carry it across the seam.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: Option<String>,
}

/// 007 §C6: which Commands a backend accepts (capability gating). All-false
/// default = a minimal backend (Send/Cancel are assumed by the orchestrator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommandSet {
    pub steer: bool,
    pub cancel_tool: bool,
    pub answer_permission: bool,
    pub answer_auth: bool,
    pub acknowledge: bool,
    pub set_mode: bool,
    pub set_model: bool,
    pub rewind: bool,
    pub list_checkpoints: bool,
    /// Query the backend's cumulative session info (context-usage budget +
    /// session cost). claude exposes `control_request{get_context_usage}` +
    /// `{get_session_cost}` (live-confirmed 2.1.186); other backends have no such
    /// query wire today → false. Replies `SessionEvent::SessionInfo`.
    pub query_session_info: bool,
}

/// 007 §C6: which `ContentBlock` kinds `Send` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockSet {
    pub text: bool,
    pub image: bool,
    pub audio: bool,
    pub resource: bool,
    pub at_mention: bool,
}

impl BlockSet {
    /// Whether this `Send` content block is advertised by the backend. The
    /// dispatch path calls this BEFORE wire-write so an un-advertised block is
    /// rejected with `CommandNotSupported` — the design's Layer-2 rule ("adapter
    /// authoritatively rejects → CommandNotSupported, never a silent drop", §C6). Mirrors the per-kind bool
    /// against the canonical `ContentBlock` variant.
    pub fn allows(&self, block: &crate::ContentBlock) -> bool {
        use crate::ContentBlock;
        match block {
            ContentBlock::Text(_) => self.text,
            ContentBlock::Image { .. } => self.image,
            ContentBlock::Audio { .. } => self.audio,
            ContentBlock::ResourceLink { .. } => self.resource,
            ContentBlock::AtMention { .. } => self.at_mention,
        }
    }
}

/// The stable `content_block:<kind>` name a `Send` rejection reports in
/// `CommandNotSupported{command}` for an un-advertised block (§C6). Kept next to
/// `BlockSet` so the name set and the bool set never drift.
pub fn block_kind_name(block: &crate::ContentBlock) -> &'static str {
    use crate::ContentBlock;
    match block {
        ContentBlock::Text(_) => "content_block:text",
        ContentBlock::Image { .. } => "content_block:image",
        ContentBlock::Audio { .. } => "content_block:audio",
        ContentBlock::ResourceLink { .. } => "content_block:resource",
        ContentBlock::AtMention { .. } => "content_block:at_mention",
    }
}

/// 007 §1.2: source of the PromptAccepted confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptAcceptedSource {
    /// Real wire signal (codex turn/started, opencode POST-ack, hermes prompt-accepted).
    Native,
    /// Adapter fabricates it from a transport milestone (claude stdin flush-ok).
    Synthesized,
    /// Unavailable; pending queue falls back to the optimistic TurnStarted (degraded).
    #[default]
    None,
}

/// 007 §9.10: an advertised model (discovery, open set).
/// serde-derived so `SessionEvent::CatalogUpdated` can carry it across the seam.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub reasoning_efforts: Vec<String>,
}

/// 007 §9.10: an advertised mode (discovery, open set).
/// serde-derived so `SessionEvent::CatalogUpdated` can carry it across the seam.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// RFC §8 tier names (prose uses uppercase PARSED/HOOK/OPAQUE) map 1:1 to these
/// CamelCase variants (minor#3 alignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CapabilityTier {
    #[default]
    Parsed,
    Hook,
    Opaque,
}

/// The canonical signals a backend can emit. Three-field kernel behavior is
/// graded in D11 (heartbeat = P0-verified with teeth; the others = forward-
/// compatible declarations).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SignalSet {
    /// false ⇒ backend emits no `Heartbeat` liveness signal (no deadline to
    /// idle-timeout (RFC §8). [P0-verified, T13 teeth]
    pub heartbeat: bool,
    /// Whether the backend parses tool lifecycle. [does NOT enter the reducer]:
    /// a backend without it simply never emits ToolCall/ToolResult, and
    /// OUTPUT-PRESENCE stays purely event-driven (FIX 6a/D11). Forward-compat
    /// declaration, no kernel degradation branch.
    pub tool_lifecycle: bool,
    /// Whether there is an explicit terminal result. false ⇒ the adapter
    /// synthesizes a `TurnResult` at EOF (D11/FIX 6b), so `crash_outcome` is
    /// unchanged. P0's only backend (claude) = true (always emits result);
    /// the false branch has no fixture (recorded as a 04 residual).
    pub terminal_result: bool,
}

/// Backend-STATIC view of [`Capabilities::supports_midturn_delivery`], keyed by
/// the runtime backend identifier (the `extra.backend` / assistant
/// `runtime_backend` string the conversation layer persists and the session
/// factories dispatch on).
///
/// The bit is a property of the backend TYPE, not of any live session: claude
/// and codex are the direct-CLI backends whose mid-turn write genuinely reaches
/// the agent; antigravity, aionrs, and every ACP agent are one-prompt-at-a-time.
/// Consumers use this when no live `SessionBackend` exists (fresh or dormant
/// conversations) so the reported capability cannot flap with agent liveness.
/// Unknown/empty identifiers are conservatively `false` (ACP-like until proven,
/// same default as `Capabilities`). Locked against the real capability
/// constructors by tests below and module-local asserts in each backend.
pub fn backend_supports_midturn_delivery(backend: &str) -> bool {
    matches!(backend, "claude" | "codex")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midturn_delivery_is_opt_in_and_antigravity_is_excluded() {
        // Default MUST be false: a new backend is ACP-like until proven otherwise.
        assert!(!Capabilities::default().supports_midturn_delivery);
    }

    /// The static per-backend table must never drift from the bit the real
    /// capability constructors declare. (claude has no pub constructor — its
    /// lock lives module-local in `adapter::claude` tests.)
    #[test]
    fn static_backend_table_matches_capability_constructors() {
        assert_eq!(
            backend_supports_midturn_delivery("codex"),
            crate::backend::codex_capabilities().supports_midturn_delivery,
        );
        assert_eq!(
            backend_supports_midturn_delivery("antigravity"),
            crate::backend::antigravity_capabilities().supports_midturn_delivery,
        );
        // Any ACP agent id (open set) falls through to the ACP connection's bit.
        assert_eq!(
            backend_supports_midturn_delivery("gemini"),
            crate::backend::acp_capabilities().supports_midturn_delivery,
        );
        // Unknown/empty backends are conservatively false.
        assert!(!backend_supports_midturn_delivery(""));
        assert!(!backend_supports_midturn_delivery("aionrs"));
    }
}
