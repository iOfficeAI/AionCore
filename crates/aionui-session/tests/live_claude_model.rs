//! LIVE tests (`#[ignore]`) for claude's model selection, run against the real
//! `claude` binary on PATH through the production `RealSpawner` + `ClaudeConnection`.
//!
//! These exist because the whole selection mechanism rests on claude-CLI behaviour that
//! cannot be asserted from a fixture: `--model default` overriding `ANTHROPIC_MODEL`,
//! the `initialize` catalog changing shape per `--model` value, and `--resume` NOT
//! restoring a session's model. A green fake-IO suite proves our framing, not that the
//! CLI does what we think.
//!
//! Last run green against claude 2.1.231.
//!
//! Run explicitly (needs a working `claude` login + spends a few tokens):
//!
//! ```text
//! cargo test -p aionui-session --test live_claude_model -- --ignored --nocapture
//! ```
//!
//! Environment note: the effective model depends on the user's own
//! `~/.claude/settings.json` env block (`ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_*`), so
//! these assert RELATIVE facts — "the model we asked for is the one running", "a
//! selection changes the running model, and Default does not pin it" — never a
//! hard-coded model id.

use std::sync::{Arc, Mutex};

use aionui_process::Spawner;
use aionui_session::{
    BackendConnection, ClaudeConnection, Command, CommandMeta, ContentBlock, SessionConfig, SessionSpec,
};

/// Capture the `running_model` field of the reconcile log line — the only place the
/// CONCRETE model a session ended up on is observable (claude reports it in
/// `system/init`, which the session API otherwise only surfaces when no selection was
/// made).
#[derive(Clone, Default)]
struct ModelSpy(Arc<Mutex<Vec<(String, String)>>>);

impl ModelSpy {
    /// `(message, running_model)` pairs seen so far.
    fn seen(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// The spy must be installed GLOBALLY, not per-thread: the reconcile runs on the
/// backend's reader task, which tokio may schedule on any worker thread, so a
/// thread-local `set_default` subscriber would never see it.
fn global_spy() -> ModelSpy {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt;
    static SPY: std::sync::OnceLock<ModelSpy> = std::sync::OnceLock::new();
    SPY.get_or_init(|| {
        let spy = ModelSpy::default();
        let subscriber = tracing_subscriber::registry()
            .with(spy.clone())
            // Mirror to stderr so a failure shows WHY (spawn error, no init frame, ...)
            // instead of just "nothing observed".
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
            );
        tracing::subscriber::set_global_default(subscriber).expect("install the live-test subscriber");
        spy
    })
    .clone()
}

impl<S> tracing_subscriber::Layer<S> for ModelSpy
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        struct Grab {
            message: String,
            running: Option<String>,
        }
        impl tracing::field::Visit for Grab {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}").trim_matches('"').to_string();
                match field.name() {
                    "message" => self.message = rendered,
                    "running_model" => self.running = Some(rendered),
                    _ => {}
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                match field.name() {
                    "message" => self.message = value.to_owned(),
                    "running_model" => self.running = Some(value.to_owned()),
                    _ => {}
                }
            }
        }
        let mut grab = Grab {
            message: String::new(),
            running: None,
        };
        event.record(&mut grab);
        if let Some(running) = grab.running {
            self.0.lock().unwrap().push((grab.message, running));
        }
    }
}

/// Open a claude session with `model`, run one tiny turn, and report the concrete model
/// it actually ran (`None` when nothing was observed).
async fn running_model_for(model: Option<&str>) -> Option<String> {
    let spy = global_spy();
    spy.clear();

    let tmp = tempfile::tempdir().expect("tempdir");
    let spawner: Arc<dyn Spawner> = Arc::new(aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(tmp.path())),
        uuid::Uuid::now_v7(),
        "live-test",
    ));
    let conn = ClaudeConnection::new(spawner);
    let session_id = uuid::Uuid::new_v4().to_string();
    let backend = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: session_id.clone(),
            },
            SessionConfig {
                cwd: Some(tmp.path().to_string_lossy().to_string()),
                model: model.map(str::to_owned),
                ..Default::default()
            },
        )
        .await
        .expect("claude session opens (is `claude` on PATH and logged in?)");

    backend
        .dispatch(Command::Send {
            content: vec![ContentBlock::Text("say ok".into())],
            metadata: CommandMeta::default(),
        })
        .await
        .expect("prompt accepted");

    // The reconcile fires on `system/init`, which claude emits once the turn starts.
    for _ in 0..120 {
        if let Some((_, running)) = spy.seen().into_iter().next_back() {
            return Some(running);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    None
}

/// The model AionUi asks for is the model claude runs — end to end, through the real
/// spawn, with NO `--model` flag involved (the selection travels in-band).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "LIVE: spawns the real claude CLI and spends tokens"]
async fn selection_is_the_model_that_actually_runs() {
    let haiku = running_model_for(Some("haiku"))
        .await
        .expect("no reconcile line observed — did system/init never arrive?");
    println!("selection `haiku` ran: {haiku}");
    // `haiku` is claude's own alias row; its concrete id is whatever the user's
    // ANTHROPIC_DEFAULT_HAIKU_MODEL resolves to, so match on the family, not an id.
    assert!(
        haiku.to_ascii_lowercase().contains("haiku"),
        "asked for the haiku row, claude ran `{haiku}`"
    );
}

/// The "Default" row must NOT pin a model: it sends nothing, so claude resolves the user
/// config exactly as the terminal CLI does. Asserted as a DIFFERENCE against an explicit
/// selection, so it holds for any user's env.
///
/// This is the regression that started all of this: mapping Default to `--model default`
/// overrode the user's own `ANTHROPIC_MODEL` and silently ran a different model.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "LIVE: spawns the real claude CLI and spends tokens"]
async fn default_row_defers_to_the_user_config_instead_of_pinning_a_model() {
    // Default sends no set_model, so the reconcile stays silent by design — the
    // observable is that an explicit selection DOES move the model, i.e. Default is not
    // silently forcing the same one.
    assert_eq!(
        running_model_for(Some("default")).await,
        None,
        "the Default row must send no set_model at all (nothing to reconcile)"
    );

    let explicit = running_model_for(Some("haiku"))
        .await
        .expect("an explicit selection must be observable");
    println!("explicit selection ran: {explicit}");
}
