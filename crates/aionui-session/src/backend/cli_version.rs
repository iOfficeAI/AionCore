//! Version-drift detection for direct-CLI backends.
//!
//! None of claude / codex / agy ship with AionUi: each is whatever the user
//! installed. Every wire contract a backend relies on — stream shapes, control
//! frames, resume flags — was verified against one release, so a drifting
//! install has to be visible rather than failing in some unexplained way
//! mid-turn.
//!
//! claude and codex used to be exempt because the app bundled a version-pinned
//! copy of each. That hid a worse problem: the bundled CLI and the user's own
//! install could differ, so the same prompt behaved differently in AionUi and
//! in the user's terminal with nothing on screen explaining why. Bundling is
//! gone; this module gives all three backends the same treatment.
//!
//! Generic over the backend so the classification rules live in one place —
//! `antigravity::version` keeps its own module only for the agy-specific
//! notice copy.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

use aionui_common::CommandSpec;
use aionui_process::Spawner;

use crate::event::{LocalizedText, NoticeLevel};

/// The release a backend's wire contracts were verified against.
///
/// Bumping one of these means the live e2e suite passed against that exact
/// binary — not that the version number looked safe. `live_ws_http_parity_e2e`
/// is the evidence; a schema diff or a clean changelog is not, because neither
/// can see a behavioural regression.
///
/// codex is deliberately NOT on the latest release: 0.147.0 never completes a
/// turn (its response stream disconnects with `httpStatusCode: null`, observed
/// three times including a back-to-back control after three clean versions), so
/// the verified release stops at 0.146.0 until upstream fixes it.
pub const VERIFIED_CLAUDE_VERSION: &str = "2.1.227";
pub const VERIFIED_CODEX_VERSION: &str = "0.146.0";
pub const VERIFIED_AGY_VERSION: &str = "1.1.11";

/// The verified release for a direct-CLI backend, keyed by the program name the
/// backend spawns. `None` for anything not version-gated here.
pub fn verified_version(cli: &str) -> Option<&'static str> {
    match cli {
        "claude" => Some(VERIFIED_CLAUDE_VERSION),
        "codex" => Some(VERIFIED_CODEX_VERSION),
        "agy" => Some(VERIFIED_AGY_VERSION),
        _ => None,
    }
}

/// How an installed CLI compares to the release its backend was built against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionVerdict {
    /// Exactly the verified release.
    Verified,
    /// Older than verified — features this backend uses may be missing.
    Older,
    /// Newer than verified — the wire contracts may have moved.
    Newer,
    /// Unparseable output; nothing can be concluded, so nothing is claimed.
    Unknown,
}

/// Take the first dotted numeric run out of a `--version` line.
///
/// Deliberately liberal: claude prints `2.1.220 (Claude Code)`, codex prints
/// `codex-cli 0.144.6`, agy prints a bare `1.1.10`. Comparing component-wise
/// (not lexically) keeps `1.1.10 > 1.1.9`.
pub fn parse_version(raw: &str) -> Option<Vec<u32>> {
    let token = raw.split_whitespace().find(|t| {
        let core = t.trim_start_matches('v');
        core.chars().next().is_some_and(|c| c.is_ascii_digit()) && core.contains('.')
    })?;
    let parts: Vec<u32> = token
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.trim_end_matches(|c: char| !c.is_ascii_digit()))
        .map(|p| p.parse::<u32>().ok())
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then_some(parts)
}

/// Compare a reported `--version` string against a verified release.
pub fn classify(reported: &str, verified: &str) -> VersionVerdict {
    let (Some(actual), Some(expected)) = (parse_version(reported), parse_version(verified)) else {
        return VersionVerdict::Unknown;
    };
    match actual.cmp(&expected) {
        std::cmp::Ordering::Equal => VersionVerdict::Verified,
        std::cmp::Ordering::Less => VersionVerdict::Older,
        std::cmp::Ordering::Greater => VersionVerdict::Newer,
    }
}

/// i18n codes, resolved on the frontend under
/// `conversation.agentTip.codes.<code>.body`. Both interpolate `{{cli}}` /
/// `{{reported}}` / `{{verified}}`, so one pair of strings covers every
/// direct-CLI backend instead of one pair per CLI.
pub const CODE_CLI_VERSION_OLDER: &str = "CLI_VERSION_OLDER";
pub const CODE_CLI_VERSION_NEWER: &str = "CLI_VERSION_NEWER";

/// The user-facing warning for a drifting install, or `None` when there is
/// nothing worth saying.
///
/// A NEWER CLI is not treated as broken: it usually works, and blocking it
/// would strand users on an old release. It is reported once so that, if the
/// session then misbehaves, the cause is already on screen.
///
/// Both directions are `Info`, which is a statement about PRESENTATION, not
/// about how much the two verdicts matter. `Warning` renders as a filled card
/// with the same alarm glyph an error uses, and a drifting install is not an
/// error: nothing has failed, the turn is running, and the user is being told
/// something about their environment. Reported live 2026-08-11 — an `Older`
/// codex read as a failure report. `Info` is the tier the frontend draws as a
/// quiet centred line, and `MessageTips` documents it as the tier a backend
/// picks when it deliberately wants a notice to be unobtrusive.
///
/// The severity difference survives where it is acted on rather than merely
/// read: the two verdicts keep distinct i18n codes, distinct prose ("some
/// features may be missing. Consider upgrading" vs "should still work"), and
/// distinct diagnostic codes on the availability probe — which is the surface
/// that reaches the user BEFORE a conversation exists, when they are still
/// deciding whether to rely on this agent, and which keeps its own weight.
///
/// Returns the English text AND its translation handle: the text is the
/// fallback shown when the locale has no entry for the code, so both travel
/// together rather than the caller having to rebuild one from the other.
pub fn drift_notice(cli: &str, reported: &str, verified: &str) -> Option<(NoticeLevel, String, LocalizedText)> {
    let localized = |code: &str| {
        LocalizedText::new(code)
            .with("cli", cli)
            .with("reported", reported)
            .with("verified", verified)
    };
    match classify(reported, verified) {
        VersionVerdict::Verified | VersionVerdict::Unknown => None,
        VersionVerdict::Older => Some((
            NoticeLevel::Info,
            format!(
                "The installed {cli} is older than the version AionUi verified; \
                 some features may be missing. Consider upgrading {cli}. \
                 (installed {reported} / verified by AionUi: {verified})"
            ),
            localized(CODE_CLI_VERSION_OLDER),
        )),
        VersionVerdict::Newer => Some((
            NoticeLevel::Info,
            format!(
                "The installed {cli} is newer than the version AionUi verified. \
                 It should still work; report anything that behaves oddly. \
                 (installed {reported} / verified by AionUi: {verified})"
            ),
            localized(CODE_CLI_VERSION_NEWER),
        )),
    }
}

/// Diagnostic codes the availability probe persists (it has no notice channel
/// and no structured params column, so the numbers travel in `detail`).
pub const DIAGNOSTIC_VERSION_DRIFT_OLDER: &str = "version_drift_older";
pub const DIAGNOSTIC_VERSION_DRIFT_NEWER: &str = "version_drift_newer";

/// What the availability probe reports for a CLI whose version drifted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDrift {
    /// Diagnostic code the UI translates.
    pub code: &'static str,
    /// The two version numbers, in a form that needs no translation. Carried
    /// separately because the UI appends it to the TRANSLATED sentence: the
    /// prose is what differs by locale, the numbers are what the user acts on,
    /// and a translated string cannot interpolate them.
    pub detail: String,
    /// Full English sentence, used verbatim when the locale has no entry for
    /// `code`.
    pub guidance: String,
}

/// Drift verdict for the availability probe.
///
/// That probe already runs `<cli> --version` for its integrity check and
/// reaches the user BEFORE any conversation exists — which is where a version
/// warning actually helps, since that is when the user decides whether to rely
/// on this agent. It has no notice channel, so the same verdict is exposed here
/// in the shape that path can persist.
pub fn version_drift(cli: &str, reported: &str) -> Option<VersionDrift> {
    let verified = verified_version(cli)?;
    let code = match classify(reported, verified) {
        VersionVerdict::Older => DIAGNOSTIC_VERSION_DRIFT_OLDER,
        VersionVerdict::Newer => DIAGNOSTIC_VERSION_DRIFT_NEWER,
        VersionVerdict::Verified | VersionVerdict::Unknown => return None,
    };
    let (_, guidance, _) = drift_notice(cli, reported, verified)?;
    Some(VersionDrift {
        code,
        detail: format!("{cli} {reported} / verified {verified}"),
        guidance,
    })
}

/// Budget for the one-shot `--version` probe. Generous: a cold Node CLI can
/// take seconds, and a timeout only costs us the drift claim, never the
/// session.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn read_first_line(stdout: aionui_process::BoxedStdout) -> Option<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// Read `<cli> --version`, or `None` when it cannot be determined.
///
/// Best-effort: a version we cannot read must not stop a session, it only
/// means no drift claim can be made. `stdin` is dropped immediately because
/// agy waits on it; the others do not care.
pub async fn probe_cli_version(
    spawner: &Arc<dyn Spawner>,
    program: &std::path::Path,
    owner_tag: &str,
) -> Option<String> {
    let spec = CommandSpec {
        command: program.to_path_buf(),
        args: vec!["--version".to_owned()],
        env: Vec::new(),
        cwd: None,
    };
    let proc = spawner.spawn(spec, &[], owner_tag).await.ok()?;
    let (stdin, stdout) = proc.take_stdio().await?;
    drop(stdin);
    tokio::time::timeout(PROBE_TIMEOUT, read_first_line(stdout))
        .await
        .ok()?
}

/// Sessions that have already carried the version notice, keyed by
/// `(cli, session_id)`.
///
/// Deduped per SESSION, not per process. The notice is a card in a
/// conversation's own history, so a process-wide latch meant only the first
/// conversation after a restart ever showed it — a user who hit odd behaviour
/// in their fifth conversation had no way to see that their CLI had drifted.
/// Keyed by CLI too so a session that somehow drives two backends reports each.
static REPORTED: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Whether this (cli, session) pair has already reported, marking it reported.
pub fn already_reported(cli: &str, session_id: &str) -> bool {
    !REPORTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((cli.to_owned(), session_id.to_owned()))
}

/// Probe `<cli> --version` and return the drift notice to emit, if any.
///
/// Returns `None` when this session already reported, when the CLI is not
/// version-gated, when the probe failed, or when the install matches — i.e.
/// the caller emits whatever comes back and needs no further conditions.
pub async fn session_drift_notice(
    spawner: &Arc<dyn Spawner>,
    cli: &str,
    program: &std::path::Path,
    session_id: &str,
) -> Option<(NoticeLevel, String, LocalizedText)> {
    let Some(verified) = verified_version(cli) else {
        tracing::debug!(cli = %cli, "version check skipped: CLI is not version-gated");
        return None;
    };
    if already_reported(cli, session_id) {
        tracing::debug!(session_id = %session_id, cli = %cli, "version check skipped: already reported for this session");
        return None;
    }
    let Some(reported) = probe_cli_version(spawner, program, session_id).await else {
        tracing::warn!(session_id = %session_id, cli = %cli, program = %program.display(), "version probe produced no output");
        return None;
    };
    tracing::info!(session_id = %session_id, cli = %cli, version = %reported, "direct CLI version detected");
    let notice = drift_notice(cli, &reported, verified)?;
    tracing::warn!(session_id = %session_id, cli = %cli, version = %reported, "direct CLI version drift");
    Some(notice)
}

/// How long to keep trying to hand the notice to a subscriber.
///
/// Sized against the gap that produced the bug — codex broadcast at
/// `02:18:02.863` and the session pump attached at `02:18:05.036`, 2.2s later —
/// with room for a slower machine. A conversation that has attracted no
/// subscriber in 30s has no one left to tell.
const NOTICE_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const NOTICE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Broadcast a session notice, retrying until someone is subscribed.
///
/// `event_tx` is a `broadcast` channel, so a send with zero receivers is
/// DISCARDED, not queued — and the version notice is one-shot: unlike a turn
/// frame there is no later event carrying the same information, and
/// `already_reported` guarantees the check will not run again for this session.
///
/// The three backends all spawn their version probe as a detached task, so
/// whether the notice wins the race against the orchestration layer's
/// `events()` subscription is pure timing. claude happens to win (its check is
/// the last statement in `open_session`); codex reliably loses, because its
/// check is deliberately placed early — before model discovery, so the
/// reconcile path cannot skip it — and that discovery keeps `open_session`
/// busy for seconds while `codex --version` returns in ~60ms.
///
/// Measured on 2026-08-11 dev logs: 13 codex drifts detected, 13 notices
/// broadcast, 0 delivered — every one dropped by an empty channel, and every
/// one logged as if it had been sent, because the send result was discarded
/// with `let _ =`.
///
/// Retrying is free here: the caller is already a detached task whose only job
/// is this notice.
pub async fn broadcast_notice(
    event_tx: &tokio::sync::broadcast::Sender<crate::backend::types::SessionEnvelope>,
    envelope: crate::backend::types::SessionEnvelope,
    cli: &str,
) {
    broadcast_notice_within(event_tx, envelope, cli, NOTICE_DELIVERY_TIMEOUT).await
}

/// [`broadcast_notice`] with an injectable deadline, so a test does not wait 30s
/// to observe the give-up path.
async fn broadcast_notice_within(
    event_tx: &tokio::sync::broadcast::Sender<crate::backend::types::SessionEnvelope>,
    envelope: crate::backend::types::SessionEnvelope,
    cli: &str,
    timeout: std::time::Duration,
) {
    let session_id = envelope.session_id.clone();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut waited = false;
    loop {
        // `send` fails ONLY when there is no receiver, which is precisely the
        // condition worth retrying — a subscriber that is merely slow still
        // takes the value into the ring buffer.
        if event_tx.send(envelope.clone()).is_ok() {
            if waited {
                tracing::info!(session_id = %session_id, cli = %cli, "session notice delivered to a late subscriber");
            }
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                session_id = %session_id,
                cli = %cli,
                timeout_ms = timeout.as_millis(),
                "session notice dropped: nothing ever subscribed to this session's events"
            );
            return;
        }
        waited = true;
        tokio::time::sleep(NOTICE_RETRY_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice_envelope(session_id: &str) -> crate::backend::types::SessionEnvelope {
        crate::backend::types::SessionEnvelope {
            session_id: session_id.to_string(),
            turn_gen: 0,
            event: crate::event::SessionEvent::Notice {
                level: NoticeLevel::Warning,
                message: "drift".into(),
                localized: None,
                supersedes_key: None,
            },
        }
    }

    /// The bug this function exists for: the notice is produced BEFORE the
    /// orchestration layer subscribes, which is what codex does every time — its
    /// version check is placed early (so the reconcile path cannot skip it) and
    /// `open_session` then spends seconds on model discovery. A plain
    /// `send` drops the value on an empty channel, and 13 of 13 codex drift
    /// notices were lost that way.
    #[tokio::test]
    async fn a_notice_broadcast_before_anyone_subscribes_still_arrives() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        // Proof the send would have been LOST as written before: no receiver
        // exists, so the one-shot send fails outright.
        assert!(
            tx.send(notice_envelope("s1")).is_err(),
            "precondition: an empty channel drops the value"
        );

        let sender = tx.clone();
        let task = tokio::spawn(async move {
            broadcast_notice(&sender, notice_envelope("s1"), "codex").await;
        });

        // Subscribe late, the way the session pump does (measured: 2.2s after
        // the codex notice was broadcast).
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let mut rx = tx.subscribe();

        task.await.expect("delivery task");
        let env = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("the notice must still be waiting for a late subscriber")
            .expect("channel open");
        assert_eq!(env.session_id, "s1");
        assert!(matches!(env.event, crate::event::SessionEvent::Notice { .. }));
    }

    /// The claude timing — a subscriber is already attached — must not regress
    /// into waiting for anything.
    #[tokio::test]
    async fn an_already_subscribed_session_gets_the_notice_immediately() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let started = tokio::time::Instant::now();
        broadcast_notice(&tx, notice_envelope("s2"), "claude").await;
        assert!(
            started.elapsed() < NOTICE_RETRY_INTERVAL,
            "a live subscriber must be served without a retry sleep"
        );
        assert_eq!(rx.recv().await.expect("delivered").session_id, "s2");
    }

    /// A session nobody ever listens to must not keep a task alive forever.
    #[tokio::test]
    async fn delivery_gives_up_once_the_deadline_passes() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        drop(rx);
        let started = tokio::time::Instant::now();
        broadcast_notice_within(&tx, notice_envelope("s3"), "agy", std::time::Duration::from_millis(120)).await;
        let waited = started.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(120),
            "must honour the deadline: {waited:?}"
        );
        assert!(
            waited < std::time::Duration::from_secs(2),
            "must not hang past it: {waited:?}"
        );
    }

    #[test]
    fn the_verified_release_says_nothing() {
        // Literal on purpose: this is the exact string a user on the verified
        // release reports, so the test breaks if a bump forgets to re-verify.
        assert_eq!(classify("2.1.227", VERIFIED_CLAUDE_VERSION), VersionVerdict::Verified);
        assert!(drift_notice("claude", "2.1.227", VERIFIED_CLAUDE_VERSION).is_none());
    }

    #[test]
    fn components_compare_numerically_not_lexically() {
        // The bug a string compare would introduce: "0.144.6" < "0.99.0"
        // lexically, but 144 > 99.
        assert_eq!(classify("0.99.0", VERIFIED_CODEX_VERSION), VersionVerdict::Older);
        assert_eq!(classify("0.146.1", VERIFIED_CODEX_VERSION), VersionVerdict::Newer);
    }

    #[test]
    fn real_version_lines_parse() {
        // claude prints a suffix, codex prints a name prefix, agy prints bare.
        assert_eq!(parse_version("2.1.220 (Claude Code)"), Some(vec![2, 1, 220]));
        assert_eq!(parse_version("codex-cli 0.144.6"), Some(vec![0, 144, 6]));
        assert_eq!(parse_version("1.1.10"), Some(vec![1, 1, 10]));
    }

    /// Both drift directions are `Info` — the tier the frontend draws as a quiet
    /// centred line. `Warning` renders with the same alarm glyph as an error,
    /// and a drifting install is not a failure. What must stay distinguishable
    /// is what the user acts on: the code and the prose.
    #[test]
    fn an_older_install_is_told_apart_by_its_words_not_by_an_alarm() {
        let (level, text, localized) =
            drift_notice("claude", "2.1.100", VERIFIED_CLAUDE_VERSION).expect("older drifts");
        assert_eq!(level, NoticeLevel::Info, "a drifting install must not read as an error");
        assert!(
            text.contains("older") && text.contains("Consider upgrading"),
            "the older case must still say what to do: {text}"
        );
        assert!(text.contains("2.1.100") && text.contains(VERIFIED_CLAUDE_VERSION));
        assert_eq!(localized.code, CODE_CLI_VERSION_OLDER);
        assert_eq!(localized.params.get("cli").and_then(|v| v.as_str()), Some("claude"));

        let (level, _, localized) = drift_notice("codex", "0.200.0", VERIFIED_CODEX_VERSION).expect("newer drifts");
        assert_eq!(level, NoticeLevel::Info, "a newer CLI must not be treated as broken");
        assert_eq!(localized.code, CODE_CLI_VERSION_NEWER);
    }

    #[test]
    fn every_direct_cli_backend_is_version_gated() {
        // The three direct-CLI backends must behave identically here — agy used
        // to own a private copy of this logic while claude/codex had none.
        for cli in ["claude", "codex", "agy"] {
            let verified = verified_version(cli).unwrap_or_else(|| panic!("{cli} must declare a verified release"));
            assert_eq!(classify(verified, verified), VersionVerdict::Verified);
            assert_eq!(
                version_drift(cli, verified),
                None,
                "{cli}: a matching install must stay silent"
            );
        }
        assert_eq!(verified_version("kimi"), None, "ACP agents are not version-gated here");
    }

    #[test]
    fn a_drifting_install_gets_actionable_text() {
        let older = version_drift("agy", "1.1.8").expect("older must be reported");
        assert_eq!(older.code, DIAGNOSTIC_VERSION_DRIFT_OLDER);
        // The detail is what survives translation, so both numbers must be in it.
        assert!(older.detail.contains("1.1.8") && older.detail.contains(VERIFIED_AGY_VERSION));
        assert!(older.guidance.contains("1.1.8"));

        let newer = version_drift("claude", "9.9.9").expect("newer must be reported");
        assert_eq!(newer.code, DIAGNOSTIC_VERSION_DRIFT_NEWER);
        assert!(newer.detail.contains("claude") && newer.detail.contains("9.9.9"));
    }

    #[test]
    fn unreadable_version_output_says_nothing() {
        // The probe hands over whatever the CLI printed. Claiming drift from a
        // line we could not parse would invent a problem.
        for raw in ["", "error: not signed in", "???"] {
            assert_eq!(version_drift("agy", raw), None, "raw={raw:?}");
        }
    }

    /// The drift notice is a card in a conversation's own history, so every
    /// conversation has to be able to carry it. A process-wide latch meant only
    /// the first conversation after a restart ever showed it.
    #[test]
    fn the_notice_is_deduped_per_session_not_per_process() {
        let a = format!("conv-a-{}", std::process::id());
        let b = format!("conv-b-{}", std::process::id());

        assert!(!already_reported("claude", &a), "a session's first check must report");
        assert!(already_reported("claude", &a), "the same session must not repeat it");
        assert!(
            !already_reported("claude", &b),
            "a different conversation must still report"
        );
        assert!(
            !already_reported("codex", &a),
            "a different CLI in the same session reports on its own"
        );
    }

    #[test]
    fn local_codex_output_is_classified_as_newer() {
        // Real `codex --version` output on this machine (2026-08-07).
        assert_eq!(parse_version("codex-cli 0.147.0"), Some(vec![0, 147, 0]));
        let (level, _, localized) =
            drift_notice("codex", "codex-cli 0.147.0", VERIFIED_CODEX_VERSION).expect("0.147.0 drifts from 0.144.6");
        assert_eq!(level, NoticeLevel::Info);
        assert_eq!(localized.code, CODE_CLI_VERSION_NEWER);
    }

    #[test]
    fn unparseable_output_claims_nothing() {
        assert_eq!(
            classify("not a version", VERIFIED_CLAUDE_VERSION),
            VersionVerdict::Unknown
        );
        assert!(drift_notice("claude", "not a version", VERIFIED_CLAUDE_VERSION).is_none());
    }
}
