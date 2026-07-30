use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_common::{TimestampMs, now_ms};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::error::ExtensionError;
use crate::lifecycle::{HookKind, execute_hook, needs_install_hook, resolve_hook_path};
use crate::loader::{ScanPath, resolve_scan_paths};
use crate::registry_helpers::{
    build_state_map, load_and_validate, merge_persisted_states, run_deactivation_hooks, to_summary,
};
use crate::resolvers::{resolve_all_contributions, resolve_i18n_for_all};
use crate::state::ExtensionStateStore;
use crate::types::{
    ExtensionLifecyclePayload, ExtensionState, ExtensionSystemEvent, LoadedExtension, ResolvedAcpAdapter,
    ResolvedAgent, ResolvedAssistant, ResolvedChannelPlugin, ResolvedContributions, ResolvedModelProvider,
    ResolvedSettingsTab, ResolvedSkill, ResolvedTheme, WebuiContribution,
};

// Re-export ExtensionSummary from registry_helpers so that
// `registry::{ExtensionRegistry, ExtensionSummary}` continues to work.
pub use crate::registry_helpers::ExtensionSummary;

const SYSTEM_DEFAULT_USER_ID: &str = "system_default_user";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Central registry orchestrating extension loading, activation, contribution
/// resolution, and event broadcasting.
///
/// Thread-safe: can be shared across HTTP handlers, the file watcher, and
/// other async tasks via `Arc`.
#[derive(Clone)]
pub struct ExtensionRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    state_store: ExtensionStateStore,
    broadcaster: Arc<dyn EventBroadcaster>,
    app_version: String,
}

struct RegistryInner {
    extensions: Vec<LoadedExtension>,
    contributions: ResolvedContributions,
    scan_paths: Vec<ScanPath>,
    initialized: bool,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Create a new registry.
    ///
    /// - `state_store`: persists enabled/disabled states across restarts.
    /// - `broadcaster`: pushes WebSocket events to connected clients.
    /// - `app_version`: current application version for engine compatibility.
    pub fn new(state_store: ExtensionStateStore, broadcaster: Arc<dyn EventBroadcaster>, app_version: String) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner {
                extensions: Vec::new(),
                contributions: ResolvedContributions::default(),
                scan_paths: Vec::new(),
                initialized: false,
            })),
            state_store,
            broadcaster,
            app_version,
        }
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Run the full initialization pipeline using auto-detected scan paths.
    ///
    /// Resolves scan paths from environment variables and platform defaults,
    /// then delegates to [`Self::initialize_with_scan_paths`].
    pub async fn initialize(&self) -> Result<(), ExtensionError> {
        let scan_paths = resolve_scan_paths();
        self.initialize_with_scan_paths(scan_paths).await
    }

    /// Run the full initialization pipeline with explicit scan paths.
    ///
    /// Prefer this over [`Self::initialize`] when the caller already knows
    /// the extension directories (e.g., in tests or embedded deployments).
    ///
    /// Pipeline:
    /// 1. Load manifests from all directories
    /// 2. Filter by engine compatibility
    /// 3. Validate dependencies + topological sort
    /// 4. Merge persisted states (enabled/disabled)
    /// 5. Run lifecycle hooks (onInstall if needed, then onActivate)
    /// 6. Resolve all contributions
    /// 7. Persist updated states
    pub async fn initialize_with_scan_paths(&self, scan_paths: Vec<ScanPath>) -> Result<(), ExtensionError> {
        info!("initializing extension registry");
        debug!(count = scan_paths.len(), "resolved scan paths");

        // 1-3. Load, filter, validate (all sync/blocking).
        let (extensions, dep_result) = load_and_validate(&scan_paths, &self.app_version);

        // 4. Merge persisted states.
        let persisted = self.state_store.load().await?;
        let extensions = merge_persisted_states(extensions, &persisted);

        // 5. Run lifecycle hooks.
        let extensions = self.run_activation_hooks(extensions, &persisted).await;

        // 6. Resolve contributions.
        let contributions = resolve_all_installed_contributions(&extensions);

        // 7. Persist updated states.
        let states = build_state_map(&extensions);
        self.state_store.set_all(states).await;

        // Commit to inner state.
        {
            let mut guard = self.inner.write().await;
            guard.extensions = extensions;
            guard.contributions = contributions;
            guard.scan_paths = scan_paths;
            guard.initialized = true;
        }

        if !dep_result.issues.is_empty() {
            warn!(issues = dep_result.issues.len(), "dependency validation found issues");
        }

        info!("extension registry initialized");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hot reload
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Hot-reload the registry: deactivate all -> clear -> reload -> re-resolve.
    ///
    /// Emits `REGISTRY_RELOADED` event when complete.
    pub async fn hot_reload(&self) {
        info!("hot-reloading extension registry");

        // 1. Snapshot current extensions and scan paths for deactivation.
        let (current_exts, scan_paths) = {
            let guard = self.inner.read().await;
            (guard.extensions.clone(), guard.scan_paths.clone())
        };

        // 2. Run onDeactivate hooks for each currently active extension.
        run_deactivation_hooks(&current_exts).await;

        // 3. Reload pipeline (same as initialize but reuses existing scan paths).
        let (extensions, _dep_result) = load_and_validate(&scan_paths, &self.app_version);

        // Use in-memory state (not file) to preserve pending writes that
        // haven't been flushed yet by the debounce timer.
        let persisted = self.state_store.get_all().await;

        let extensions = merge_persisted_states(extensions, &persisted);
        let extensions = self.run_activation_hooks(extensions, &persisted).await;
        let contributions = resolve_all_installed_contributions(&extensions);

        let states = build_state_map(&extensions);
        self.state_store.set_all(states).await;

        // 4. Commit new state.
        {
            let mut guard = self.inner.write().await;
            guard.extensions = extensions;
            guard.contributions = contributions;
            // scan_paths stay the same
        }

        // 5. Broadcast REGISTRY_RELOADED event.
        self.broadcast_lifecycle_event("registry", ExtensionSystemEvent::RegistryReloaded, None);

        info!("extension registry hot-reloaded");
    }
}

// ---------------------------------------------------------------------------
// Enable / Disable
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Enable an extension by name.
    ///
    /// Compatibility wrapper for the local system user.
    pub async fn enable_extension(&self, name: &str) -> Result<(), ExtensionError> {
        self.enable_extension_for_user(SYSTEM_DEFAULT_USER_ID, name).await
    }

    /// Enable an installed extension for one application user.
    pub async fn enable_extension_for_user(&self, user_id: &str, name: &str) -> Result<(), ExtensionError> {
        self.require_installed(name).await?;
        if self.extension_enabled_for_user(user_id, name).await {
            debug!(user_id, name, "extension already enabled for user");
            return Ok(());
        }

        self.state_store.set_user_enabled(user_id, name, true).await;
        self.broadcast_state_changed(user_id, name, true);

        info!(user_id, name, "extension enabled for user");
        Ok(())
    }

    /// Disable an extension by name.
    ///
    /// Compatibility wrapper for the local system user.
    pub async fn disable_extension(&self, name: &str, reason: Option<&str>) -> Result<(), ExtensionError> {
        self.disable_extension_for_user(SYSTEM_DEFAULT_USER_ID, name, reason)
            .await
    }

    /// Disable an installed extension for one application user.
    pub async fn disable_extension_for_user(
        &self,
        user_id: &str,
        name: &str,
        reason: Option<&str>,
    ) -> Result<(), ExtensionError> {
        self.require_installed(name).await?;
        if !self.extension_enabled_for_user(user_id, name).await {
            debug!(user_id, name, "extension already disabled for user");
            return Ok(());
        }

        self.state_store.set_user_enabled(user_id, name, false).await;
        self.broadcast_state_changed(user_id, name, false);

        info!(
            user_id,
            name,
            reason_supplied = reason.is_some(),
            "extension disabled for user"
        );
        Ok(())
    }

    async fn require_installed(&self, name: &str) -> Result<(), ExtensionError> {
        let guard = self.inner.read().await;
        guard
            .extensions
            .iter()
            .any(|extension| extension.manifest.name == name)
            .then_some(())
            .ok_or_else(|| ExtensionError::NotFound(name.to_owned()))
    }

    async fn extension_enabled_for_user(&self, user_id: &str, name: &str) -> bool {
        if let Some(enabled) = self.state_store.get_user_enabled(user_id, name).await {
            return enabled;
        }
        if user_id != SYSTEM_DEFAULT_USER_ID {
            return true;
        }
        let guard = self.inner.read().await;
        guard
            .extensions
            .iter()
            .find(|extension| extension.manifest.name == name)
            .is_none_or(|extension| extension.state.enabled)
    }
}

// ---------------------------------------------------------------------------
// Query methods
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Return summaries of all loaded extensions.
    pub async fn get_loaded_extensions(&self) -> Vec<ExtensionSummary> {
        self.get_loaded_extensions_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    /// Return summaries with enabled state overlaid for one user.
    pub async fn get_loaded_extensions_for_user(&self, user_id: &str) -> Vec<ExtensionSummary> {
        self.extensions_for_user(user_id).await.iter().map(to_summary).collect()
    }

    pub(crate) fn event_broadcaster(&self) -> Arc<dyn EventBroadcaster> {
        self.broadcaster.clone()
    }

    /// Look up a single loaded extension by name.
    pub async fn get_extension_by_name(&self, name: &str) -> Option<LoadedExtension> {
        let guard = self.inner.read().await;
        guard.extensions.iter().find(|e| e.manifest.name == name).cloned()
    }

    /// Look up an installed extension with one user's enabled state overlaid.
    pub async fn get_extension_by_name_for_user(&self, user_id: &str, name: &str) -> Option<LoadedExtension> {
        self.extensions_for_user(user_id)
            .await
            .into_iter()
            .find(|extension| extension.manifest.name == name)
    }

    /// Snapshot of all resolved contributions.
    pub async fn get_contributions(&self) -> ResolvedContributions {
        self.get_contributions_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_contributions_for_user(&self, user_id: &str) -> ResolvedContributions {
        let enabled: HashSet<String> = self
            .extensions_for_user(user_id)
            .await
            .into_iter()
            .filter(|extension| extension.state.enabled)
            .map(|extension| extension.manifest.name)
            .collect();
        let mut contributions = {
            let guard = self.inner.read().await;
            guard.contributions.clone()
        };
        retain_contributions(&mut contributions, &enabled);
        contributions
    }

    pub async fn get_themes(&self) -> Vec<ResolvedTheme> {
        self.get_themes_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_themes_for_user(&self, user_id: &str) -> Vec<ResolvedTheme> {
        self.get_contributions_for_user(user_id).await.themes
    }

    pub async fn get_assistants(&self) -> Vec<ResolvedAssistant> {
        self.get_assistants_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_assistants_for_user(&self, user_id: &str) -> Vec<ResolvedAssistant> {
        self.get_contributions_for_user(user_id).await.assistants
    }

    /// Return `true` if any extension contributes an assistant with this id.
    pub async fn has_assistant(&self, id: &str) -> bool {
        self.has_assistant_for_user(SYSTEM_DEFAULT_USER_ID, id).await
    }

    pub async fn has_assistant_for_user(&self, user_id: &str, id: &str) -> bool {
        self.get_assistants_for_user(user_id)
            .await
            .iter()
            .any(|assistant| assistant.id == id)
    }

    /// Lookup a single extension-contributed assistant by id.
    pub async fn get_assistant_by_id(&self, id: &str) -> Option<ResolvedAssistant> {
        self.get_assistant_by_id_for_user(SYSTEM_DEFAULT_USER_ID, id).await
    }

    pub async fn get_assistant_by_id_for_user(&self, user_id: &str, id: &str) -> Option<ResolvedAssistant> {
        self.get_assistants_for_user(user_id)
            .await
            .into_iter()
            .find(|assistant| assistant.id == id)
    }

    pub async fn get_acp_adapters(&self) -> Vec<ResolvedAcpAdapter> {
        self.get_acp_adapters_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_acp_adapters_for_user(&self, user_id: &str) -> Vec<ResolvedAcpAdapter> {
        self.get_contributions_for_user(user_id).await.acp_adapters
    }

    pub async fn get_agents(&self) -> Vec<ResolvedAgent> {
        self.get_agents_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_agents_for_user(&self, user_id: &str) -> Vec<ResolvedAgent> {
        self.get_contributions_for_user(user_id).await.agents
    }

    pub async fn get_mcp_servers(&self) -> Vec<crate::types::ResolvedMcpServer> {
        self.get_mcp_servers_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_mcp_servers_for_user(&self, user_id: &str) -> Vec<crate::types::ResolvedMcpServer> {
        self.get_contributions_for_user(user_id).await.mcp_servers
    }

    pub async fn get_skills(&self) -> Vec<ResolvedSkill> {
        self.get_skills_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_skills_for_user(&self, user_id: &str) -> Vec<ResolvedSkill> {
        self.get_contributions_for_user(user_id).await.skills
    }

    pub async fn get_settings_tabs(&self) -> Vec<ResolvedSettingsTab> {
        self.get_settings_tabs_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_settings_tabs_for_user(&self, user_id: &str) -> Vec<ResolvedSettingsTab> {
        self.get_contributions_for_user(user_id).await.settings_tabs
    }

    pub async fn get_webui_contributions(&self) -> Vec<WebuiContribution> {
        self.get_webui_contributions_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_webui_contributions_for_user(&self, user_id: &str) -> Vec<WebuiContribution> {
        self.get_contributions_for_user(user_id).await.webui
    }

    pub async fn get_channel_plugins(&self) -> Vec<ResolvedChannelPlugin> {
        self.get_channel_plugins_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_channel_plugins_for_user(&self, user_id: &str) -> Vec<ResolvedChannelPlugin> {
        self.get_contributions_for_user(user_id).await.channel_plugins
    }

    pub async fn get_model_providers(&self) -> Vec<ResolvedModelProvider> {
        self.get_model_providers_for_user(SYSTEM_DEFAULT_USER_ID).await
    }

    pub async fn get_model_providers_for_user(&self, user_id: &str) -> Vec<ResolvedModelProvider> {
        self.get_contributions_for_user(user_id).await.model_providers
    }

    /// Resolve i18n data for a given locale across all enabled extensions.
    pub async fn get_i18n_for_locale(&self, locale: &str) -> HashMap<String, HashMap<String, String>> {
        self.get_i18n_for_locale_for_user(SYSTEM_DEFAULT_USER_ID, locale).await
    }

    pub async fn get_i18n_for_locale_for_user(
        &self,
        user_id: &str,
        locale: &str,
    ) -> HashMap<String, HashMap<String, String>> {
        resolve_i18n_for_all(&self.extensions_for_user(user_id).await, locale)
    }

    /// Whether the registry has been initialized.
    pub async fn is_initialized(&self) -> bool {
        let guard = self.inner.read().await;
        guard.initialized
    }

    async fn extensions_for_user(&self, user_id: &str) -> Vec<LoadedExtension> {
        let mut extensions = {
            let guard = self.inner.read().await;
            guard.extensions.clone()
        };
        let overrides = self.state_store.get_user_enabled_all(user_id).await;
        for extension in &mut extensions {
            extension.state.enabled = overrides
                .get(&extension.manifest.name)
                .copied()
                .unwrap_or(user_id != SYSTEM_DEFAULT_USER_ID || extension.state.enabled);
        }
        extensions
    }
}

fn resolve_all_installed_contributions(extensions: &[LoadedExtension]) -> ResolvedContributions {
    let mut installed = extensions.to_vec();
    for extension in &mut installed {
        extension.state.enabled = true;
    }
    resolve_all_contributions(&installed)
}

fn retain_contributions(contributions: &mut ResolvedContributions, enabled: &HashSet<String>) {
    contributions
        .acp_adapters
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .mcp_servers
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .assistants
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .agents
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .skills
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .themes
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .channel_plugins
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .webui
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .settings_tabs
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .model_providers
        .retain(|item| enabled.contains(&item.extension_name));
    contributions
        .i18n
        .retain(|extension_name, _| enabled.contains(extension_name));
}

// ---------------------------------------------------------------------------
// Event broadcasting helpers
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    fn broadcast_state_changed(&self, user_id: &str, name: &str, enabled: bool) {
        let event = WebSocketMessage::new(
            "extensions.state-changed",
            json!({ "user_id": user_id, "name": name, "enabled": enabled }),
        );
        self.broadcaster.broadcast(event);
    }

    fn broadcast_lifecycle_event(
        &self,
        extension_name: &str,
        event: ExtensionSystemEvent,
        data: Option<serde_json::Value>,
    ) {
        let payload = ExtensionLifecyclePayload {
            extension_name: extension_name.to_owned(),
            event,
            timestamp: now_ms(),
            data,
        };
        let msg = WebSocketMessage::new(
            "extensions.lifecycle",
            serde_json::to_value(&payload).unwrap_or_default(),
        );
        self.broadcaster.broadcast(msg);
    }
}

// ---------------------------------------------------------------------------
// Activation hooks
// ---------------------------------------------------------------------------

impl ExtensionRegistry {
    /// Run lifecycle hooks for each extension in order:
    /// - `onInstall` if first time or version changed
    /// - `onActivate` for each enabled extension
    ///
    /// Hook failures are logged but do not prevent other extensions from
    /// activating.
    async fn run_activation_hooks(
        &self,
        mut extensions: Vec<LoadedExtension>,
        persisted: &HashMap<String, ExtensionState>,
    ) -> Vec<LoadedExtension> {
        let now: TimestampMs = now_ms();

        for ext in &mut extensions {
            if !ext.state.enabled {
                continue;
            }

            let ext_name = ext.manifest.name.clone();
            let ext_dir = Path::new(&ext.directory);

            // Check onInstall + onActivate hooks.
            if let Some(hooks) = &ext.manifest.lifecycle {
                let persisted_version = persisted.get(&ext_name).map(|s| s.version.as_str());

                if needs_install_hook(&ext.manifest.version, persisted_version)
                    && let Some(hook_path) = resolve_hook_path(hooks, HookKind::OnInstall)
                    && let Err(e) = execute_hook(ext_dir, hook_path, HookKind::OnInstall, &ext_name).await
                {
                    warn!(
                        extension = %ext_name,
                        error = %e,
                        "onInstall hook failed, continuing"
                    );
                }

                // Run onActivate
                if let Some(hook_path) = resolve_hook_path(hooks, HookKind::OnActivate)
                    && let Err(e) = execute_hook(ext_dir, hook_path, HookKind::OnActivate, &ext_name).await
                {
                    warn!(
                        extension = %ext_name,
                        error = %e,
                        "onActivate hook failed, continuing"
                    );
                }
            }

            // Update activation timestamp and install time.
            ext.state.last_activated_at = Some(now);
            if ext.state.installed_at.is_none() {
                ext.state.installed_at = Some(now);
            }

            self.broadcast_lifecycle_event(&ext_name, ExtensionSystemEvent::ExtensionActivated, None);
        }

        extensions
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExtensionManifest, ExtensionSource, ExtensionState};
    use aionui_realtime::BroadcastEventBus;

    fn make_test_ext(name: &str, enabled: bool) -> LoadedExtension {
        LoadedExtension {
            manifest: ExtensionManifest {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                display_name: Some(format!("{name} Display")),
                description: Some(format!("{name} description")),
                author: None,
                license: None,
                homepage: None,
                icon: None,
                engine: None,
                api_version: None,
                dependencies: HashMap::new(),
                entry_point: None,
                permissions: None,
                contributes: None,
                lifecycle: None,
                i18n: None,
            },
            directory: format!("/tmp/ext/{name}"),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: name.to_owned(),
                version: "1.0.0".to_owned(),
                enabled,
                installed_at: Some(1000),
                last_activated_at: None,
            },
        }
    }

    fn make_registry() -> (ExtensionRegistry, ExtensionStateStore, Arc<BroadcastEventBus>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store.clone(), bus.clone(), "1.0.0".to_owned());
        (registry, store, bus)
    }

    // -- enable_extension / disable_extension -----------------------------------

    #[tokio::test]
    async fn enable_nonexistent_returns_not_found() {
        let (registry, _, _) = make_registry();
        let result = registry.enable_extension("no-such-ext").await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn disable_nonexistent_returns_not_found() {
        let (registry, _, _) = make_registry();
        let result = registry.disable_extension("no-such-ext", None).await;
        assert!(matches!(result, Err(ExtensionError::NotFound(_))));
    }

    #[tokio::test]
    async fn enable_disable_roundtrip() {
        let (registry, _, bus) = make_registry();

        // Seed the registry with a disabled extension.
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("test-ext", false)];
            guard.initialized = true;
        }

        let mut rx = bus.subscribe();

        // Enable
        registry.enable_extension("test-ext").await.unwrap();
        assert!(registry.get_loaded_extensions().await[0].enabled);

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.name, "extensions.state-changed");
        assert_eq!(msg.data["user_id"], SYSTEM_DEFAULT_USER_ID);
        assert_eq!(msg.data["enabled"], true);

        // Disable
        registry
            .disable_extension("test-ext", Some("test reason"))
            .await
            .unwrap();
        assert!(!registry.get_loaded_extensions().await[0].enabled);

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.name, "extensions.state-changed");
        assert_eq!(msg.data["enabled"], false);
    }

    #[tokio::test]
    async fn enable_already_enabled_is_noop() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext", true)];
        }
        // Should succeed without error.
        registry.enable_extension("ext").await.unwrap();
    }

    #[tokio::test]
    async fn disable_already_disabled_is_noop() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext", false)];
        }
        registry.disable_extension("ext", None).await.unwrap();
    }

    // -- query methods --------------------------------------------------------

    #[tokio::test]
    async fn get_loaded_extensions_returns_summaries() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("ext-a", true), make_test_ext("ext-b", false)];
        }

        let summaries = registry.get_loaded_extensions().await;
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].name, "ext-a");
        assert!(summaries[0].enabled);
        assert_eq!(summaries[1].name, "ext-b");
        assert!(!summaries[1].enabled);
    }

    #[tokio::test]
    async fn get_extension_by_name_found_and_not_found() {
        let (registry, _, _) = make_registry();
        {
            let mut guard = registry.inner.write().await;
            guard.extensions = vec![make_test_ext("my-ext", true)];
        }

        assert!(registry.get_extension_by_name("my-ext").await.is_some());
        assert!(registry.get_extension_by_name("nope").await.is_none());
    }

    #[tokio::test]
    async fn is_initialized_before_and_after() {
        let (registry, _, _) = make_registry();
        assert!(!registry.is_initialized().await);

        {
            let mut guard = registry.inner.write().await;
            guard.initialized = true;
        }
        assert!(registry.is_initialized().await);
    }
}
