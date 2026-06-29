#![warn(clippy::disallowed_types)]

//! OS shell integration: file/folder opener, tool detection, and speech-to-text.
pub mod error;
pub mod opener;
pub mod routes;
pub mod shell;
pub mod state;
pub mod stt;
pub(crate) mod stt_deepgram;
pub(crate) mod stt_openai;
pub mod stt_stream;
pub mod stt_stream_deepgram;
pub mod stt_stream_openai;
pub mod stt_stream_provider;
pub(crate) mod stt_stream_tls;

pub use error::{ShellError, SttError};
pub use opener::{DefaultSystemOpener, ISystemOpener, NoopSystemOpener};
pub use routes::shell_routes;
pub use shell::ShellService;
pub use state::ShellRouterState;
pub use stt::SttService;
pub use stt_stream::{ClientFrame, UpstreamEvent, UpstreamFactory, UpstreamStream, run_stream_session};
pub use stt_stream_deepgram::DeepgramUpstreamFactory;
pub use stt_stream_openai::OpenAIRealtimeUpstreamFactory;
pub use stt_stream_provider::ProviderUpstreamFactory;
