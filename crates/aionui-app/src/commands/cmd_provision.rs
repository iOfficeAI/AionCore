//! `aioncore provision` — trusted local provisioning CLI (A0/A1 surface).
//!
//! Conversation-independent. Discovers the installation via data-dir endpoint
//! advertisement (no caller-provided port). Does not use runtime tokens,
//! cookies, CSRF, or `--local` identity fallback.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use aionui_api_types::{
    AssistantDeleteRequest, AssistantGetRequest, AssistantReconcileRequest, LocalProvisionEndpoint, McpDeleteRequest,
    McpGetRequest, McpReconcileRequest, PROVISION_PROTOCOL_VERSION, ProvisionAttestation, ProvisionAuthorizeRequest,
    ProvisionBackendAvailability, ProvisionErrorBody, ProvisionErrorCode, ProvisionGrant, ProvisionSubject,
    ProvisionSubjectStatus, SkillDeleteRequest, SkillGetRequest, SkillReconcileRequest, TeamDefinitionDeleteRequest,
    TeamDefinitionGetRequest, TeamDefinitionUpsertRequest,
};

use aionui_app::provisioning::{
    ProvisionEngine, ProvisionEngineError, attestation_from_parts, capability_contract, closed_backend_state,
    endpoint_file_path, installation_id_for_data_dir, profile_id_for_data_dir, read_endpoint, running_backend_state,
};

use crate::cli::{
    ProvisionArgs, ProvisionAssistantsArgs, ProvisionAssistantsCommand, ProvisionCommand, ProvisionMcpArgs,
    ProvisionMcpCommand, ProvisionSkillsArgs, ProvisionSkillsCommand, ProvisionTeamsArgs, ProvisionTeamsCommand,
};

/// Process-local engine for the CLI process (skeleton store).
fn engine() -> &'static Mutex<ProvisionEngine> {
    static ENGINE: OnceLock<Mutex<ProvisionEngine>> = OnceLock::new();
    ENGINE.get_or_init(|| Mutex::new(ProvisionEngine::new()))
}

pub async fn run_provision(args: ProvisionArgs, data_dir: PathBuf) -> ExitCode {
    match run(args, data_dir).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = write_error_envelope(&error);
            eprintln!("{}", error.stderr_line());
            error.exit_code()
        }
    }
}

async fn run(args: ProvisionArgs, data_dir: PathBuf) -> Result<(), ProvisionCliError> {
    match args.command {
        ProvisionCommand::Capabilities => print_success(capability_contract(), meta()),
        ProvisionCommand::Discover => run_discover(&data_dir),
        ProvisionCommand::Attest => run_attest(&data_dir),
        ProvisionCommand::Authorize => run_authorize(&data_dir),
        ProvisionCommand::Revoke => run_revoke(),
        ProvisionCommand::Assistants(args) => run_assistants(args),
        ProvisionCommand::Mcp(args) => run_mcp(args),
        ProvisionCommand::Skills(args) => run_skills(args),
        ProvisionCommand::Teams(args) => run_teams(args),
    }
}

fn run_discover(data_dir: &Path) -> Result<(), ProvisionCliError> {
    let discovery = discover_installation(data_dir)?;
    print_success(
        json!({
            "protocol_version": PROVISION_PROTOCOL_VERSION,
            "data_dir": data_dir.display().to_string(),
            "endpoint_path": endpoint_file_path(data_dir).display().to_string(),
            "installation_id": discovery.installation_id,
            "profile_id": discovery.profile_id,
            "backend": discovery.backend,
            "endpoint": discovery.endpoint,
            "discovery_method": "data_dir_endpoint_file",
            "caller_port_required": false,
        }),
        meta(),
    )
}

fn run_attest(data_dir: &Path) -> Result<(), ProvisionCliError> {
    let attestation = build_attestation(data_dir)?;
    print_success(
        serde_json::to_value(attestation).map_err(|_| {
            ProvisionCliError::new(
                ProvisionErrorCode::InvalidPayload,
                "provision attest",
                "failed to serialize attestation",
            )
        })?,
        meta(),
    )
}

fn run_authorize(data_dir: &Path) -> Result<(), ProvisionCliError> {
    let command = "provision authorize";
    let request: ProvisionAuthorizeRequest = read_stdin_json(command)?;
    let attestation = build_attestation(data_dir)?;
    let mut engine = engine().lock().expect("provision engine lock");
    let grant = engine
        .authorize(request, &attestation)
        .map_err(|err| engine_error(command, err))?;
    print_success(serde_json::to_value(grant).unwrap(), meta())
}

fn run_revoke() -> Result<(), ProvisionCliError> {
    let command = "provision revoke";
    let payload: Value = read_stdin_json(command)?;
    let grant_id = payload
        .get("grant_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ProvisionCliError::new(ProvisionErrorCode::InvalidPayload, command, "grant_id is required")
                .field("field", "grant_id")
        })?;
    let mut engine = engine().lock().expect("provision engine lock");
    engine.revoke(grant_id).map_err(|err| engine_error(command, err))?;
    print_success(json!({ "revoked": true, "grant_id": grant_id }), meta())
}

fn run_assistants(args: ProvisionAssistantsArgs) -> Result<(), ProvisionCliError> {
    match args.command {
        ProvisionAssistantsCommand::Reconcile => {
            let command = "provision assistants reconcile";
            let request: AssistantReconcileRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            let readback = engine
                .reconcile_assistant(request)
                .map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionAssistantsCommand::Get => {
            let command = "provision assistants get";
            let request: AssistantGetRequest = read_stdin_json(command)?;
            let engine = engine().lock().expect("provision engine lock");
            let readback = engine
                .get_assistant(request)
                .map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionAssistantsCommand::Delete => {
            let command = "provision assistants delete";
            let request: AssistantDeleteRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            engine
                .delete_assistant(request)
                .map_err(|err| engine_error(command, err))?;
            print_success(json!({ "deleted": true }), meta())
        }
    }
}

fn run_mcp(args: ProvisionMcpArgs) -> Result<(), ProvisionCliError> {
    match args.command {
        ProvisionMcpCommand::Reconcile => {
            let command = "provision mcp reconcile";
            let request: McpReconcileRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            let readback = engine
                .reconcile_mcp(request)
                .map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionMcpCommand::Get => {
            let command = "provision mcp get";
            let request: McpGetRequest = read_stdin_json(command)?;
            let engine = engine().lock().expect("provision engine lock");
            let readback = engine.get_mcp(request).map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionMcpCommand::Delete => {
            let command = "provision mcp delete";
            let request: McpDeleteRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            engine.delete_mcp(request).map_err(|err| engine_error(command, err))?;
            print_success(json!({ "deleted": true }), meta())
        }
    }
}

fn run_skills(args: ProvisionSkillsArgs) -> Result<(), ProvisionCliError> {
    match args.command {
        ProvisionSkillsCommand::Reconcile => {
            let command = "provision skills reconcile";
            let request: SkillReconcileRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            let readback = engine
                .reconcile_skill(request)
                .map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionSkillsCommand::Get => {
            let command = "provision skills get";
            let request: SkillGetRequest = read_stdin_json(command)?;
            let engine = engine().lock().expect("provision engine lock");
            let readback = engine.get_skill(request).map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionSkillsCommand::Delete => {
            let command = "provision skills delete";
            let request: SkillDeleteRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            engine.delete_skill(request).map_err(|err| engine_error(command, err))?;
            print_success(json!({ "deleted": true }), meta())
        }
    }
}

fn run_teams(args: ProvisionTeamsArgs) -> Result<(), ProvisionCliError> {
    match args.command {
        ProvisionTeamsCommand::Create => {
            let command = "provision teams create";
            let request: TeamDefinitionUpsertRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            let readback = engine
                .upsert_team(request, true)
                .map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionTeamsCommand::Update => {
            let command = "provision teams update";
            let request: TeamDefinitionUpsertRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            let readback = engine
                .upsert_team(request, false)
                .map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionTeamsCommand::Get => {
            let command = "provision teams get";
            let request: TeamDefinitionGetRequest = read_stdin_json(command)?;
            let engine = engine().lock().expect("provision engine lock");
            let readback = engine.get_team(request).map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(readback).unwrap(), meta())
        }
        ProvisionTeamsCommand::Delete => {
            let command = "provision teams delete";
            let request: TeamDefinitionDeleteRequest = read_stdin_json(command)?;
            let mut engine = engine().lock().expect("provision engine lock");
            let disposition = engine.delete_team(request).map_err(|err| engine_error(command, err))?;
            print_success(serde_json::to_value(disposition).unwrap(), meta())
        }
    }
}

#[derive(Debug)]
struct DiscoverySnapshot {
    installation_id: String,
    profile_id: String,
    backend: aionui_api_types::ProvisionBackendState,
    endpoint: Option<LocalProvisionEndpoint>,
}

fn discover_installation(data_dir: &Path) -> Result<DiscoverySnapshot, ProvisionCliError> {
    let installation_id = installation_id_for_data_dir(data_dir);
    let profile_id = profile_id_for_data_dir(data_dir);
    let endpoint = read_endpoint(data_dir).map_err(|err| {
        ProvisionCliError::new(
            ProvisionErrorCode::InstallationNotFound,
            "provision discover",
            "failed to read local provision endpoint file",
        )
        .field("error", err.to_string())
    })?;

    let Some(endpoint) = endpoint else {
        return Ok(DiscoverySnapshot {
            installation_id,
            profile_id,
            backend: closed_backend_state(),
            endpoint: None,
        });
    };

    // Stale file: PID not alive → treat as closed-app (never fall back to local identity).
    if !process_seems_alive(endpoint.pid) {
        return Ok(DiscoverySnapshot {
            installation_id: endpoint.installation_id.clone(),
            profile_id: endpoint.profile_id.clone(),
            backend: closed_backend_state(),
            endpoint: Some(endpoint),
        });
    }

    // Installation/profile must match data-dir derivation when both are known.
    if endpoint.installation_id != installation_id || endpoint.profile_id != profile_id {
        // Prefer advertised ids (backend is source of truth) but surface mismatch for diagnostics.
        tracing::warn!(
            advertised_installation = %endpoint.installation_id,
            derived_installation = %installation_id,
            "provision endpoint installation id differs from data-dir derivation"
        );
    }

    Ok(DiscoverySnapshot {
        installation_id: endpoint.installation_id.clone(),
        profile_id: endpoint.profile_id.clone(),
        backend: running_backend_state(endpoint.pid, endpoint.base_url.clone()),
        endpoint: Some(endpoint),
    })
}

fn build_attestation(data_dir: &Path) -> Result<ProvisionAttestation, ProvisionCliError> {
    let discovery = discover_installation(data_dir)?;
    let (identity_mode, aioncore_version, aionui_version) = match &discovery.endpoint {
        Some(endpoint) => (
            endpoint.identity_mode.clone(),
            endpoint.aioncore_version.clone(),
            endpoint.aionui_version.clone(),
        ),
        None => ("unknown".to_owned(), env!("CARGO_PKG_VERSION").to_owned(), None),
    };

    // Subject: skeleton does not extract browser/session credentials. When the
    // backend is running under aionpro, subject remains Unknown until a future
    // principal-attestation channel is wired. Unknown never becomes
    // system_default_user authority.
    let subject = ProvisionSubject {
        subject_id: None,
        user_type: None,
        session_generation: None,
        status: if discovery.backend.state == ProvisionBackendAvailability::Running {
            ProvisionSubjectStatus::Unknown
        } else {
            ProvisionSubjectStatus::Absent
        },
    };

    // For offline protocol testing, accept AIONCORE_PROVISION_TEST_SUBJECT
    // only when explicitly provided — never invent local-default identity.
    let subject = if let Ok(raw) = std::env::var("AIONCORE_PROVISION_TEST_SUBJECT") {
        parse_test_subject(&raw).unwrap_or(subject)
    } else {
        subject
    };

    Ok(attestation_from_parts(
        discovery.installation_id,
        discovery.profile_id,
        identity_mode,
        aioncore_version,
        aionui_version,
        subject,
        discovery.backend,
    ))
}

fn parse_test_subject(raw: &str) -> Option<ProvisionSubject> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let subject_id = value.get("subject_id")?.as_str()?.to_owned();
    if subject_id.trim().is_empty() || subject_id == "system_default_user" {
        return None;
    }
    Some(ProvisionSubject {
        subject_id: Some(subject_id),
        user_type: value.get("user_type").and_then(Value::as_str).map(str::to_owned),
        session_generation: value.get("session_generation").and_then(Value::as_i64),
        status: ProvisionSubjectStatus::Attested,
    })
}

fn process_seems_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // signal 0: existence check without delivery.
        // SAFETY: kill(pid, 0) only probes liveness.
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == 0 {
            return true;
        }
        // EPERM means the process exists but we cannot signal it — still alive.
        let errno = std::io::Error::last_os_error().raw_os_error();
        errno == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        // Best-effort: OpenProcess. If unavailable, treat as alive so we do not
        // falsely claim closed-app and invite unsafe local fallback.
        let _ = pid;
        true
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

fn read_stdin_json<T: DeserializeOwned>(command: &str) -> Result<T, ProvisionCliError> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw).map_err(|_| {
        ProvisionCliError::new(
            ProvisionErrorCode::InvalidPayload,
            command,
            "failed to read JSON payload from stdin",
        )
        .field("field", "stdin")
    })?;
    if raw.trim().is_empty() {
        return Err(ProvisionCliError::new(
            ProvisionErrorCode::InvalidPayload,
            command,
            "JSON payload is required on stdin",
        )
        .field("field", "stdin"));
    }
    serde_json::from_str(&raw).map_err(|err| {
        ProvisionCliError::new(ProvisionErrorCode::InvalidPayload, command, "JSON payload is invalid")
            .field("field", "stdin")
            .field("error", err.to_string())
    })
}

fn engine_error(command: &str, err: ProvisionEngineError) -> ProvisionCliError {
    let mut cli = ProvisionCliError::new(err.code, command, err.message);
    if let Some(field) = err.field {
        cli = cli.field("field", field);
    }
    cli.zero_mutation = err.zero_mutation;
    cli
}

fn meta() -> Value {
    json!({
        "schema_version": 1,
        "protocol_version": PROVISION_PROTOCOL_VERSION,
        "contract": aionui_api_types::PROVISION_CONTRACT,
    })
}

fn print_success(data: Value, meta: Value) -> Result<(), ProvisionCliError> {
    let envelope = json!({
        "success": true,
        "data": data,
        "meta": meta,
    });
    let rendered = serde_json::to_string_pretty(&envelope).map_err(|_| {
        ProvisionCliError::new(
            ProvisionErrorCode::InvalidPayload,
            "provision",
            "failed to serialize success envelope",
        )
    })?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(rendered.as_bytes())
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| {
            ProvisionCliError::new(
                ProvisionErrorCode::InvalidPayload,
                "provision",
                "failed to write JSON output",
            )
        })
}

fn write_error_envelope(error: &ProvisionCliError) -> io::Result<()> {
    let body = ProvisionErrorBody {
        code: error.code,
        message: error.message.to_owned(),
        field: error.fields.get("field").cloned(),
        zero_mutation: error.zero_mutation,
    };
    let envelope = json!({
        "success": false,
        "error": body,
        "meta": meta(),
    });
    let rendered = serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| {
        r#"{"success":false,"error":{"code":"PROVISION_INVALID_PAYLOAD","message":"serialize failed","zero_mutation":true}}"#.to_owned()
    });
    let mut stdout = io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    stdout.write_all(b"\n")
}

#[derive(Debug)]
struct ProvisionCliError {
    code: ProvisionErrorCode,
    command: String,
    message: &'static str,
    fields: BTreeMap<&'static str, String>,
    zero_mutation: bool,
}

impl ProvisionCliError {
    fn new(code: ProvisionErrorCode, command: &str, message: &'static str) -> Self {
        Self {
            code,
            command: command.to_owned(),
            message,
            fields: BTreeMap::new(),
            zero_mutation: true,
        }
    }

    fn field(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.fields.insert(key, value.into());
        self
    }

    fn exit_code(&self) -> ExitCode {
        match self.code {
            ProvisionErrorCode::InvalidPayload
            | ProvisionErrorCode::InvalidDesiredState
            | ProvisionErrorCode::InvalidLeader
            | ProvisionErrorCode::InvalidMemberKey
            | ProvisionErrorCode::UnsupportedVersion
            | ProvisionErrorCode::WrongProfile
            | ProvisionErrorCode::ScopeMissing
            | ProvisionErrorCode::AuthorityMissing
            | ProvisionErrorCode::AuthorityUnknown
            | ProvisionErrorCode::AuthorityExpired
            | ProvisionErrorCode::AuthorityRevoked
            | ProvisionErrorCode::SubjectMismatch
            | ProvisionErrorCode::ConcurrentConflict
            | ProvisionErrorCode::TeamReferencedAssistant
            | ProvisionErrorCode::ResourceNotFound
            | ProvisionErrorCode::DispositionUnknown => ExitCode::from(2),
            ProvisionErrorCode::BackendClosed
            | ProvisionErrorCode::BackendUnavailable
            | ProvisionErrorCode::InstallationNotFound
            | ProvisionErrorCode::RuntimeBusy => ExitCode::from(3),
        }
    }

    fn stderr_line(&self) -> String {
        let mut line = format!("{} command=\"{}\"", self.code.as_str(), escape_field(&self.command));
        for (key, value) in &self.fields {
            line.push_str(&format!(" {key}=\"{}\"", escape_field(value)));
        }
        if self.zero_mutation {
            line.push_str(" zero_mutation=true");
        }
        line.push_str(": ");
        line.push_str(self.message);
        line
    }
}

fn escape_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

// Silence unused import of ProvisionGrant in some builds.
#[allow(dead_code)]
fn _grant_type_anchor(_: &ProvisionGrant) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use aionui_api_types::{PROVISION_SCHEMA_VERSION, ProvisionScope};
    use aionui_app::provisioning::{now_ms, write_endpoint};

    #[test]
    fn discover_reports_closed_when_endpoint_missing() {
        let dir = std::env::temp_dir().join(format!("aion-prov-cli-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let snapshot = discover_installation(&dir).unwrap();
        assert_eq!(snapshot.backend.state, ProvisionBackendAvailability::Closed);
        assert!(snapshot.endpoint.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_reads_endpoint_without_caller_port() {
        let dir = std::env::temp_dir().join(format!("aion-prov-cli2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let endpoint = LocalProvisionEndpoint {
            schema_version: PROVISION_SCHEMA_VERSION,
            protocol_version: PROVISION_PROTOCOL_VERSION,
            installation_id: installation_id_for_data_dir(&dir),
            profile_id: profile_id_for_data_dir(&dir),
            pid: std::process::id(),
            host: "127.0.0.1".into(),
            port: 25808,
            base_url: "http://127.0.0.1:25808".into(),
            identity_mode: "aionpro".into(),
            aioncore_version: "0.1.62".into(),
            aionui_version: Some("2.1.52".into()),
            started_at_ms: now_ms(),
            capabilities: ProvisionScope::ALL.to_vec(),
        };
        write_endpoint(&dir, &endpoint).unwrap();
        let snapshot = discover_installation(&dir).unwrap();
        assert_eq!(snapshot.backend.state, ProvisionBackendAvailability::Running);
        assert_eq!(snapshot.backend.base_url.as_deref(), Some("http://127.0.0.1:25808"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_subject_rejects_system_default_user() {
        assert!(parse_test_subject(r#"{"subject_id":"system_default_user"}"#).is_none());
        let subject =
            parse_test_subject(r#"{"subject_id":"user_1","user_type":"aionpro","session_generation":2}"#).unwrap();
        assert_eq!(subject.subject_id.as_deref(), Some("user_1"));
        assert_eq!(subject.status, ProvisionSubjectStatus::Attested);
    }
}
