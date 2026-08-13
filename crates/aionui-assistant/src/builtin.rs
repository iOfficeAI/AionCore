//! Built-in assistant registry — embeds the manifest + rule/avatar
//! assets into the binary via `include_dir`, with an optional filesystem
//! fallback for E2E tests.
//!
//! This kills the "binary must live next to an on-disk assets/ sibling"
//! assumption, which was fragile in two ways:
//!
//! 1. Dev: Electron launches the backend through a symlink
//!    (`~/.cargo/bin/aioncore` → `target/debug/aioncore`) and
//!    `std::env::current_exe().parent()` would resolve to the symlink's
//!    directory, not the real binary's, missing the `assets/` sibling.
//! 2. Prod: `AionUi/scripts/prepareAionuiBackend.js` only copies the
//!    binary from GitHub releases — the `assets/` directory never shipped.
//!
//! Embedding avoids both. E2E tests that want to inject a custom fixture
//! still can, via the `AIONUI_BUILTIN_ASSISTANTS_PATH` env var → disk
//! fallback path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, warn};

/// Assets compiled into the binary at build time. Paths are relative to
/// this embedded root, matching the on-disk layout under
/// `crates/aionui-app/assets/builtin-assistants/`.
static BUILTIN_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../aionui-app/assets/builtin-assistants");

/// Single built-in assistant entry, loaded from `assistants.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct BuiltinAssistant {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub name_i18n: HashMap<String, String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_i18n: HashMap<String, String>,
    #[serde(default)]
    pub avatar: Option<String>,
    pub agent_ref: String,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    #[serde(default)]
    pub custom_skill_names: Vec<String>,
    #[serde(default)]
    pub disabled_builtin_skills: Vec<String>,
    /// Relative to the asset root; may contain `{locale}`.
    #[serde(default)]
    pub rule_file: Option<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub prompts_i18n: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub models: Vec<String>,
    /// Default position in the official assistant list. Lower comes first.
    /// Owned by this manifest (users cannot reorder official assistants), so
    /// this value is authoritative across versions. Defaults to 0.
    #[serde(default)]
    pub sort_order: i32,
    /// Whether this official assistant is enabled by default when a user has
    /// no overlay for it. The butler, game-dev-studio, and promo team lead ship
    /// enabled; other specialists stay off so they don't crowd selection lists.
    #[serde(default)]
    pub default_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct BuiltinManifest {
    #[serde(default)]
    #[allow(dead_code)]
    version: String,
    #[serde(default)]
    assistants: Vec<BuiltinAssistant>,
}

/// An avatar asset loaded from either the embedded bundle or a disk override.
///
/// Carries the raw bytes plus the file extension (lower-case, without the
/// leading dot) so the HTTP layer can set `Content-Type`.
#[derive(Debug, Clone)]
pub struct AvatarAsset {
    pub bytes: Vec<u8>,
    pub extension: Option<String>,
}

/// Source of built-in asset content.
///
/// The disk branch exists for E2E tests that point
/// `AIONUI_BUILTIN_ASSISTANTS_PATH` at a fixture directory.
enum Source {
    Embedded,
    Disk(PathBuf),
}

/// In-memory registry of built-in assistants.
pub struct BuiltinAssistantRegistry {
    assistants: HashMap<String, BuiltinAssistant>,
    source: Source,
}

impl BuiltinAssistantRegistry {
    /// Construct the registry.
    ///
    /// If `AIONUI_BUILTIN_ASSISTANTS_PATH` is set and points to a readable
    /// directory, read from disk (test-only override). Otherwise use the
    /// assets embedded at compile time.
    pub fn load() -> Self {
        if let Ok(env) = std::env::var("AIONUI_BUILTIN_ASSISTANTS_PATH") {
            let p = PathBuf::from(env);
            if p.exists() {
                return Self::load_from_dir(p);
            }
            warn!(
                "AIONUI_BUILTIN_ASSISTANTS_PATH points to missing directory; \
                 falling back to embedded assets"
            );
        }
        Self::load_embedded()
    }

    /// Load the compiled-in assets.
    pub fn load_embedded() -> Self {
        let content = match BUILTIN_ASSETS.get_file("assistants.json") {
            Some(f) => f.contents(),
            None => {
                // This can only happen if the embedded bundle itself is
                // missing the manifest — treat as a build error, but stay
                // graceful at runtime.
                error!("Embedded built-in manifest missing (assistants.json)");
                return Self::with_assistants(HashMap::new(), Source::Embedded);
            }
        };
        let assistants = parse_manifest_bytes(content);
        Self::with_assistants(assistants, Source::Embedded)
    }

    /// Load from an explicit on-disk directory. Preserved for
    /// `AIONUI_BUILTIN_ASSISTANTS_PATH` E2E fixtures — the three
    /// graceful-degradation branches below mirror the original filesystem
    /// behaviour.
    pub fn load_from_dir(assets_dir: PathBuf) -> Self {
        let manifest_path = assets_dir.join("assistants.json");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Built-in manifest missing at {}: {}", manifest_path.display(), e);
                return Self::with_assistants(HashMap::new(), Source::Disk(assets_dir));
            }
        };
        let assistants = parse_manifest_str(&content);
        Self::with_assistants(assistants, Source::Disk(assets_dir))
    }

    fn with_assistants(assistants: HashMap<String, BuiltinAssistant>, source: Source) -> Self {
        Self { assistants, source }
    }

    /// Construct an empty registry (safe fallback + test helper). Treated
    /// as embedded-source with zero entries; callers should prefer
    /// [`load`](Self::load) in production.
    pub fn empty() -> Self {
        Self::with_assistants(HashMap::new(), Source::Embedded)
    }

    pub fn has(&self, id: &str) -> bool {
        self.assistants.contains_key(id)
    }

    pub fn get(&self, id: &str) -> Option<&BuiltinAssistant> {
        self.assistants.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &BuiltinAssistant> {
        self.assistants.values()
    }

    pub fn is_empty(&self) -> bool {
        self.assistants.is_empty()
    }

    pub fn len(&self) -> usize {
        self.assistants.len()
    }

    /// Read the rule file bytes for a built-in assistant. Substitutes
    /// `{locale}` in the manifest-declared `rule_file` path. Returns `None`
    /// when the assistant has no declared rule or the file is missing.
    pub fn rule_bytes(&self, id: &str, locale: &str) -> Option<Vec<u8>> {
        let rel = self.assistants.get(id)?.rule_file.as_ref()?;
        self.read_asset(&rel.replace("{locale}", locale))
    }

    /// Read the avatar asset for a built-in assistant along with its
    /// extension (for Content-Type inference). Returns `None` when the
    /// manifest does not declare an avatar or the file is missing.
    ///
    /// Note: when the manifest `avatar` field is an emoji string
    /// (like `"📝"`) rather than a relative path, no file is resolved and
    /// this method returns `None`. Callers treating an assistant without a
    /// shipped avatar should fall back to the text avatar on the client.
    pub fn avatar_asset(&self, id: &str) -> Option<AvatarAsset> {
        let a = self.assistants.get(id)?;
        let rel = a.avatar.as_ref()?;
        // Emoji / text avatars have no path separator and no extension.
        if !looks_like_relative_path(rel) {
            return None;
        }
        let bytes = self.read_asset(rel)?;
        let extension = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        Some(AvatarAsset { bytes, extension })
    }

    /// Dispatch read to embedded bundle or disk, depending on source.
    fn read_asset(&self, rel: &str) -> Option<Vec<u8>> {
        match &self.source {
            Source::Embedded => BUILTIN_ASSETS.get_file(rel).map(|f| f.contents().to_vec()),
            Source::Disk(root) => std::fs::read(root.join(rel)).ok(),
        }
    }
}

impl Default for BuiltinAssistantRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

fn parse_manifest_bytes(bytes: &[u8]) -> HashMap<String, BuiltinAssistant> {
    match serde_json::from_slice::<Value>(bytes).and_then(parse_manifest_value) {
        Ok(m) => m.assistants.into_iter().map(|a| (a.id.clone(), a)).collect(),
        Err(e) => {
            error!("Embedded built-in manifest parse failed: {e}");
            HashMap::new()
        }
    }
}

fn parse_manifest_str(content: &str) -> HashMap<String, BuiltinAssistant> {
    match serde_json::from_str::<Value>(content).and_then(parse_manifest_value) {
        Ok(m) => m.assistants.into_iter().map(|a| (a.id.clone(), a)).collect(),
        Err(e) => {
            error!("Built-in manifest parse failed: {e}");
            HashMap::new()
        }
    }
}

fn parse_manifest_value(value: Value) -> Result<BuiltinManifest, serde_json::Error> {
    if let Some(assistants) = value.get("assistants").and_then(Value::as_array) {
        for assistant in assistants {
            if assistant.get("skill_file").is_some() {
                return Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "builtin assistant legacy field `skill_file` is no longer supported",
                )));
            }
        }
    }
    serde_json::from_value(value)
}

/// Heuristic for distinguishing a relative-path avatar (`"rules/x.svg"`)
/// from an inline emoji/text avatar (`"📝"`). Path-like strings contain a
/// `/` or at least one `.` extension separator.
fn looks_like_relative_path(s: &str) -> bool {
    s.contains('/') || (Path::new(s).extension().is_some() && !s.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join("assistants.json"), body).unwrap();
    }

    // -----------------------------------------------------------------------
    // Embedded-source sanity: the bundle shipped with the crate must be
    // well-formed and non-empty. Acts as a compile-time → runtime bridge
    // guard (if the manifest is ever broken or the include_dir path is
    // wrong, this test fails immediately rather than at user-hit-404 time).
    // -----------------------------------------------------------------------

    #[test]
    fn load_embedded_has_expected_builtins() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        assert!(!reg.is_empty(), "embedded registry should contain the shipped presets");
        // Sanity-check a couple of known ids from the committed manifest.
        assert!(reg.has("word-creator"));
        assert!(reg.has("cowork"));
    }

    #[test]
    fn load_embedded_threejs_skills_default_on_web_game_assistants() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        let expected = [
            "threejs-game-director",
            "threejs-gameplay-systems",
            "threejs-aaa-graphics-builder",
            "threejs-game-ui-designer",
            "threejs-debug-profiler",
            "threejs-qa-release",
            "threejs-3d-generator",
            "threejs-image-generator",
            "threejs-audio-generator",
        ];
        for id in ["game-dev-studio", "game-3d"] {
            let assistant = reg.get(id).unwrap_or_else(|| panic!("missing builtin {id}"));
            assert_eq!(
                assistant.enabled_skills, expected,
                "{id} should default-enable threejs skills"
            );
        }
        let butler = reg.get("aionui-assistant").expect("aionui-assistant");
        for skill in expected {
            assert!(
                butler.enabled_skills.iter().any(|s| s == skill),
                "butler should default-enable {skill} for Guid out-of-box web games"
            );
        }
        assert!(
            butler.enabled_skills.iter().any(|s| s == "aionui-config"),
            "butler must keep config skills"
        );
        let studio = reg.get("game-dev-studio").expect("game-dev-studio");
        assert!(
            studio.default_enabled,
            "game-dev-studio must be selectable without a settings toggle"
        );
        let promo_lead = reg.get("team-promo-creator").expect("team-promo-creator");
        assert!(
            promo_lead.default_enabled,
            "promo team lead must be selectable without a settings toggle"
        );
        assert!(
            promo_lead.enabled_skills.iter().any(|s| s == "bgm-prompting"),
            "promo lead single-session fallback needs bgm-prompting"
        );
        for id in [
            "spatial-GameDesigner",
            "spatial-TechnicalArtist",
            "spatial-NarrativeDesigner",
            "spatial-LevelDesigner",
            "spatial-GameAudioEngineer",
            "spatial-UnityArchitect",
        ] {
            let assistant = reg.get(id).unwrap_or_else(|| panic!("missing builtin {id}"));
            assert!(
                !assistant.enabled_skills.iter().any(|s| s.starts_with("threejs-")),
                "{id} must not default-enable threejs skills"
            );
        }
        let word = reg.get("word-creator").expect("word-creator");
        assert!(
            !word.enabled_skills.iter().any(|s| s.starts_with("threejs-")),
            "threejs skills must not leak onto unrelated assistants"
        );
        let rule = reg
            .rule_bytes("game-dev-studio", "zh-CN")
            .expect("game-dev-studio rule");
        let text = std::str::from_utf8(&rule).expect("utf-8");
        assert!(
            text.contains("[LOAD_SKILL: threejs-game-director]"),
            "resident routing block must stay in the game-dev-studio rule"
        );
        assert!(
            text.contains("体验意图"),
            "game-dev-studio must require an experience intent"
        );
        assert!(
            text.contains("交付闸门"),
            "game-dev-studio must keep a delivery gate"
        );
        assert!(
            text.contains("分享"),
            "game-dev-studio must require a share mode"
        );
        assert!(
            text.contains("执行流程") && text.contains("实测可玩") && text.contains("--deliver"),
            "game-dev-studio must sequence make → playtest → --deliver"
        );
        assert!(
            text.contains("会话已注入 `TRIPO_API_KEY`")
                && text.contains("已注入 `ELEVENLABS_API_KEY`"),
            "game-dev-studio must tell the model Aion injects Tripo and ElevenLabs keys"
        );
        assert!(
            !text.contains("3D/音频 key 缺失时回退"),
            "game-dev-studio must not default to procedural 3D/audio"
        );
        let en_rule = reg
            .rule_bytes("game-dev-studio", "en-US")
            .expect("game-dev-studio en-US rule");
        let en_text = std::str::from_utf8(&en_rule).expect("utf-8");
        assert!(
            en_text.contains("[LOAD_SKILL: threejs-game-director]"),
            "en-US game-dev-studio must route through threejs-game-director"
        );
        assert!(
            en_text.contains("experience intent")
                && en_text.contains("Prove it plays")
                && en_text.contains("--deliver"),
            "en-US game-dev-studio must keep experience intent, playtest, and --deliver"
        );
        assert!(
            en_text.contains("Aion injects `TRIPO_API_KEY`")
                && en_text.contains("ELEVENLABS_API_KEY"),
            "en-US game-dev-studio must tell the model Aion injects Tripo and ElevenLabs keys"
        );
        for (id, locale) in [
            ("aionui-assistant", "zh-CN"),
            ("aionui-assistant", "en-US"),
            ("aionui-assistant", "ru-RU"),
            ("game-3d", "zh-CN"),
            ("game-3d", "en-US"),
            ("game-3d", "ru-RU"),
        ] {
            let rule = reg.rule_bytes(id, locale).unwrap_or_else(|| panic!("{id} {locale} rule"));
            let text = std::str::from_utf8(&rule).expect("utf-8");
            assert!(
                text.contains("[LOAD_SKILL: threejs-game-director]"),
                "{id} {locale} must route through threejs-game-director"
            );
            assert!(
                !text.contains("不要询问用户任何问题，直接生成完整代码"),
                "{id} {locale} must not force the old single-file Kirby template"
            );
            assert!(
                !text.contains("Do NOT ask the user any questions, generate complete code directly"),
                "{id} {locale} must not force the old single-file Kirby template"
            );
            assert!(
                !text.contains("3D 仍依赖")
                    && !text.contains("3D still needs")
                    && !text.contains("3D по-прежнему зависит"),
                "{id} {locale} must not tell the model Tripo is unavailable"
            );
            let has_intent = text.contains("体验意图")
                || text.contains("experience intent")
                || text.contains("намерени");
            let has_share = text.contains("分享")
                || text.contains("share")
                || text.contains("поделиться")
                || text.contains("шаринг");
            let has_launch = text.contains("launch_game.mjs")
                || text.contains("npm run play")
                || text.contains("实际启动")
                || text.contains("actual launch")
                || text.contains("фактическ");
            assert!(has_intent, "{id} {locale} must name an experience intent");
            assert!(has_share, "{id} {locale} must require share");
            assert!(has_launch, "{id} {locale} must require an actual launch");
        }
        let image_skill = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../aionui-app/assets/builtin-skills/threejs-image-generator/SKILL.md"
        ));
        assert!(
            image_skill.contains("aionui_image_generation"),
            "threejs-image-generator must prefer Aion image MCP"
        );
    }

    #[test]
    fn load_embedded_rule_bytes_available_for_shipped_preset() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        let bytes = reg
            .rule_bytes("word-creator", "en-US")
            .expect("shipped word-creator en-US rule should resolve from the embedded bundle");
        assert!(!bytes.is_empty());
        let text = std::str::from_utf8(&bytes).expect("rule file should be valid utf-8");
        assert!(text.len() > 100, "rule file should have real content");
    }

    #[test]
    fn embedded_rule_missing_locale_returns_none() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        // The manifest declares rule_file as "rules/{id}.{locale}.md"; a
        // made-up locale can't resolve.
        assert!(reg.rule_bytes("word-creator", "xx-YY").is_none());
    }

    // -----------------------------------------------------------------------
    // Disk-source fallback (used by E2E fixtures via
    // AIONUI_BUILTIN_ASSISTANTS_PATH). Graceful-degradation semantics must
    // stay intact.
    // -----------------------------------------------------------------------

    #[test]
    fn load_from_dir_missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        let reg = BuiltinAssistantRegistry::load_from_dir(missing);
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_missing_manifest_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_malformed_manifest_returns_empty() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "{not valid json");
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_rejects_legacy_skill_file_entries() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{
              "version": "1.0.0",
              "assistants": [{
                "id": "legacy",
                "name": "Legacy",
                "agent_ref": "gemini",
                "skill_file": "skills/legacy.en-US.md"
              }]
            }"#,
        );
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty(), "legacy skill_file manifest should be rejected");
    }

    #[test]
    fn load_from_dir_reads_bytes_from_disk() {
        let tmp = TempDir::new().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("office.en-US.md"), "office rule body").unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "version": "1.0.0",
                "assistants": [{
                    "id": "builtin-office",
                    "name": "Office",
                    "agent_ref": "gemini",
                    "rule_file": "rules/office.{locale}.md"
                }]
            }"#,
        );

        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert_eq!(reg.len(), 1);
        assert!(reg.has("builtin-office"));

        let bytes = reg
            .rule_bytes("builtin-office", "en-US")
            .expect("disk-source rule_bytes should read the fixture");
        assert_eq!(bytes, b"office rule body");
    }

    #[test]
    fn load_from_dir_missing_asset_returns_none() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "assistants": [{
                    "id": "x",
                    "name": "X",
                    "agent_ref": "gemini",
                    "rule_file": "rules/x.{locale}.md"
                }]
            }"#,
        );
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.rule_bytes("x", "en-US").is_none());
    }

    // -----------------------------------------------------------------------
    // load() env-var routing
    // -----------------------------------------------------------------------

    #[test]
    fn load_respects_env_var_disk_override() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"assistants":[{"id":"env-only","name":"E","agent_ref": "gemini"}]}"#,
        );
        // SAFETY: env-var mutation is only unsafe if another thread reads
        // environment concurrently. This test is self-contained.
        // SAFETY: cargo test runs tests in parallel by default, so guard
        // against interference from other tests by using a unique env-var
        // value and checking via a dedicated loader call.
        let key = "AIONUI_BUILTIN_ASSISTANTS_PATH";
        let prev = std::env::var(key).ok();
        // SAFETY: set_var is sound when no other thread is concurrently
        // reading env. Tests within this module do not share mutation, and
        // the env key is not observed by other tests.
        unsafe {
            std::env::set_var(key, tmp.path());
        }
        let reg = BuiltinAssistantRegistry::load();
        assert!(reg.has("env-only"));
        assert!(!reg.has("word-creator"));
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    // -----------------------------------------------------------------------
    // Avatar asset — emoji vs file
    // -----------------------------------------------------------------------

    #[test]
    fn avatar_asset_is_none_for_inline_emoji_avatar() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        // word-form-creator still ships with an inline emoji avatar.
        assert!(reg.avatar_asset("word-form-creator").is_none());
    }

    #[test]
    fn embedded_avatar_asset_returns_bytes_and_extension_for_shipped_file_avatar() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        let asset = reg
            .avatar_asset("word-creator")
            .expect("shipped word-creator avatar should resolve from the embedded bundle");
        assert!(!asset.bytes.is_empty());
        assert_eq!(asset.extension.as_deref(), Some("jpg"));
    }

    #[test]
    fn avatar_asset_returns_bytes_and_extension_for_file_avatar() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("duck.svg"), b"<svg/>").unwrap();
        write_manifest(
            tmp.path(),
            r#"{"assistants":[{
                "id": "with-file-avatar",
                "name": "F",
                "agent_ref": "gemini",
                "avatar": "duck.svg"
            }]}"#,
        );
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        let asset = reg.avatar_asset("with-file-avatar").unwrap();
        assert_eq!(asset.bytes, b"<svg/>");
        assert_eq!(asset.extension.as_deref(), Some("svg"));
    }
}
