//! Trusted local provisioning protocol surface (A0/A1).
//!
//! Conversation-independent discovery, attestation, scoped authority, and
//! conditional resource reconcile skeletons for external provisioners
//! (pc-tools). Does not reuse agent runtime tokens, cookies, CSRF, or
//! `--local` / `system_default_user` fallback.

mod capabilities;
mod endpoint;
mod engine;

pub use capabilities::capability_contract;
pub use endpoint::{
    endpoint_file_path, installation_id_for_data_dir, profile_id_for_data_dir, read_endpoint, remove_endpoint,
    write_endpoint, write_endpoint_for_config,
};
pub use engine::{
    ProvisionEngine, ProvisionEngineError, attestation_from_parts, closed_backend_state, now_ms, running_backend_state,
};
