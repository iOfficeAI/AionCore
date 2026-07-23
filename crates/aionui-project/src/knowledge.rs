use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::time::Duration;

use aionui_api_types::{ProjectKnowledgeFact, ProjectTaskContext};
use aionui_runtime::Builder;
use serde_json::Value;

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_FACTS: usize = 500;
const MAX_CONTEXT_ITEMS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeProviderError {
    Unavailable,
    Timeout,
    MalformedOutput,
    Rejected,
}

impl KnowledgeProviderError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::MalformedOutput => "malformed_output",
            Self::Rejected => "rejected",
        }
    }
}

impl std::fmt::Display for KnowledgeProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "knowledge provider {}", self.category())
    }
}

impl std::error::Error for KnowledgeProviderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectKnowledgeProviderHealth {
    pub provider: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectKnowledgeProviderRequest {
    pub project_path: String,
    pub provider_project_name: String,
    pub source_commit: Option<String>,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectKnowledgeProviderResult {
    pub provider_project_name: String,
    pub source_commit: Option<String>,
    pub changed_paths: Vec<String>,
    pub facts: Vec<ProjectKnowledgeFact>,
}

#[async_trait::async_trait]
pub trait ProjectKnowledgeProvider: Send + Sync {
    async fn health(&self) -> Result<ProjectKnowledgeProviderHealth, KnowledgeProviderError>;
    async fn index(
        &self,
        request: &ProjectKnowledgeProviderRequest,
    ) -> Result<ProjectKnowledgeProviderResult, KnowledgeProviderError>;
    async fn update(
        &self,
        request: &ProjectKnowledgeProviderRequest,
    ) -> Result<ProjectKnowledgeProviderResult, KnowledgeProviderError>;
    async fn architecture(
        &self,
        provider_project_name: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError>;
    async fn search(
        &self,
        provider_project_name: &str,
        query: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError>;
    async fn trace(
        &self,
        provider_project_name: &str,
        function_name: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError>;
    async fn task_context(
        &self,
        provider_project_name: &str,
        query: &str,
        generation: i64,
    ) -> Result<ProjectTaskContext, KnowledgeProviderError>;
}

#[derive(Clone, Debug)]
pub struct CodebaseMemoryCliProvider {
    program: String,
}

impl Default for CodebaseMemoryCliProvider {
    fn default() -> Self {
        Self::new("codebase-memory-mcp")
    }
}

impl CodebaseMemoryCliProvider {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }

    async fn run_json(&self, arguments: &[String]) -> Result<Value, KnowledgeProviderError> {
        let mut command = Builder::clean_cli(&self.program);
        command.args(arguments);
        let output = tokio::time::timeout(PROVIDER_TIMEOUT, command.output())
            .await
            .map_err(|_| KnowledgeProviderError::Timeout)?
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    KnowledgeProviderError::Unavailable
                } else {
                    KnowledgeProviderError::Rejected
                }
            })?;
        if !output.status.success() {
            return Err(KnowledgeProviderError::Rejected);
        }
        serde_json::from_slice(&output.stdout).map_err(|_| KnowledgeProviderError::MalformedOutput)
    }

    async fn index_with_mode(
        &self,
        request: &ProjectKnowledgeProviderRequest,
        mode: &str,
    ) -> Result<ProjectKnowledgeProviderResult, KnowledgeProviderError> {
        let value = self
            .run_json(&[
                "cli".into(),
                "index_repository".into(),
                "--repo-path".into(),
                request.project_path.clone(),
                "--name".into(),
                request.provider_project_name.clone(),
                "--mode".into(),
                mode.into(),
                "--persistence".into(),
                "false".into(),
            ])
            .await?;
        let provider_project_name = value
            .get("project")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(KnowledgeProviderError::MalformedOutput)?;
        if provider_project_name != request.provider_project_name {
            return Err(KnowledgeProviderError::MalformedOutput);
        }
        Ok(ProjectKnowledgeProviderResult {
            provider_project_name: provider_project_name.into(),
            source_commit: request.source_commit.clone(),
            changed_paths: request.changed_paths.clone(),
            facts: Vec::new(),
        })
    }

    async fn graph_facts(
        &self,
        tool: &str,
        arguments: Vec<String>,
        default_kind: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError> {
        let mut command_arguments = vec!["cli".into(), tool.into()];
        command_arguments.extend(arguments);
        let value = self.run_json(&command_arguments).await?;
        let mut facts = Vec::new();
        collect_facts(&value, default_kind, &mut facts);
        facts.truncate(MAX_FACTS);
        Ok(facts)
    }
}

#[async_trait::async_trait]
impl ProjectKnowledgeProvider for CodebaseMemoryCliProvider {
    async fn health(&self) -> Result<ProjectKnowledgeProviderHealth, KnowledgeProviderError> {
        let mut command = Builder::clean_cli(&self.program);
        command.arg("--version");
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .map_err(|_| KnowledgeProviderError::Timeout)?
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    KnowledgeProviderError::Unavailable
                } else {
                    KnowledgeProviderError::Rejected
                }
            })?;
        if !output.status.success() {
            return Err(KnowledgeProviderError::Unavailable);
        }
        let version = String::from_utf8(output.stdout)
            .ok()
            .and_then(|value| value.lines().next().map(str::trim).map(str::to_owned))
            .filter(|value| !value.is_empty());
        Ok(ProjectKnowledgeProviderHealth {
            provider: "codebase-memory".into(),
            version,
        })
    }

    async fn index(
        &self,
        request: &ProjectKnowledgeProviderRequest,
    ) -> Result<ProjectKnowledgeProviderResult, KnowledgeProviderError> {
        self.index_with_mode(request, "moderate").await
    }

    async fn update(
        &self,
        request: &ProjectKnowledgeProviderRequest,
    ) -> Result<ProjectKnowledgeProviderResult, KnowledgeProviderError> {
        self.index_with_mode(request, "fast").await
    }

    async fn architecture(
        &self,
        provider_project_name: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError> {
        let mut facts = self
            .graph_facts(
                "get_architecture",
                vec![
                    "--project".into(),
                    provider_project_name.into(),
                    "--aspects".into(),
                    "overview".into(),
                    "--aspects".into(),
                    "routes".into(),
                    "--aspects".into(),
                    "entry_points".into(),
                    "--aspects".into(),
                    "packages".into(),
                ],
                "architecture",
            )
            .await?;
        let mut located = self
            .graph_facts(
                "search_graph",
                vec![
                    "--project".into(),
                    provider_project_name.into(),
                    "--name-pattern".into(),
                    ".*".into(),
                    "--min-degree".into(),
                    "1".into(),
                    "--limit".into(),
                    "200".into(),
                ],
                "symbol",
            )
            .await?;
        facts.append(&mut located);
        facts.truncate(MAX_FACTS);
        Ok(facts)
    }

    async fn search(
        &self,
        provider_project_name: &str,
        query: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError> {
        self.graph_facts(
            "search_graph",
            vec![
                "--project".into(),
                provider_project_name.into(),
                "--query".into(),
                query.into(),
                "--include-connected".into(),
                "true".into(),
                "--limit".into(),
                MAX_CONTEXT_ITEMS.to_string(),
            ],
            "symbol",
        )
        .await
    }

    async fn trace(
        &self,
        provider_project_name: &str,
        function_name: &str,
    ) -> Result<Vec<ProjectKnowledgeFact>, KnowledgeProviderError> {
        let value = self
            .run_json(&[
                "cli".into(),
                "trace_path".into(),
                "--project".into(),
                provider_project_name.into(),
                "--function-name".into(),
                function_name.into(),
                "--direction".into(),
                "both".into(),
                "--depth".into(),
                "2".into(),
                "--include-tests".into(),
                "true".into(),
            ])
            .await?;
        let mut qualified_names = BTreeSet::new();
        collect_qualified_names(&value, &mut qualified_names);
        let mut facts = Vec::new();
        for qualified_name in qualified_names.into_iter().take(MAX_CONTEXT_ITEMS) {
            let mut located = self
                .graph_facts(
                    "search_graph",
                    vec![
                        "--project".into(),
                        provider_project_name.into(),
                        "--qn-pattern".into(),
                        format!("^{}$", escape_regex(&qualified_name)),
                        "--limit".into(),
                        "1".into(),
                    ],
                    "caller",
                )
                .await?;
            facts.append(&mut located);
        }
        facts.truncate(MAX_CONTEXT_ITEMS);
        Ok(facts)
    }

    async fn task_context(
        &self,
        provider_project_name: &str,
        query: &str,
        generation: i64,
    ) -> Result<ProjectTaskContext, KnowledgeProviderError> {
        let search = self.search(provider_project_name, query).await?;
        let mut traced = Vec::new();
        if let Some(function_name) = search.iter().find_map(|fact| fact.qualified_name.as_deref()) {
            traced = self.trace(provider_project_name, function_name).await?;
        }
        let mut symbols = BTreeSet::new();
        let mut callers = BTreeSet::new();
        let mut tests = BTreeSet::new();
        let mut routes = BTreeSet::new();
        let mut data_entities = BTreeSet::new();
        for fact in search.iter().chain(traced.iter()) {
            let value = fact.qualified_name.as_ref().unwrap_or(&fact.name).clone();
            match fact.kind.as_str() {
                "test" => {
                    tests.insert(fact.source_path.clone());
                }
                "route" => {
                    routes.insert(value);
                }
                "data_entity" => {
                    data_entities.insert(value);
                }
                "caller" => {
                    callers.insert(value);
                }
                _ => {
                    symbols.insert(value);
                }
            }
        }
        Ok(ProjectTaskContext {
            id: String::new(),
            project_id: String::new(),
            provider_project_name: provider_project_name.into(),
            generation,
            query: query.into(),
            symbols: bounded(symbols),
            callers: bounded(callers),
            tests: bounded(tests),
            routes: bounded(routes),
            data_entities: bounded(data_entities),
            created_at: 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepositoryKnowledgeState {
    pub source_commit: Option<String>,
    pub changed_paths: Vec<String>,
}

pub(crate) async fn inspect_repository(path: &str) -> Result<RepositoryKnowledgeState, KnowledgeProviderError> {
    let source_commit = match git_output(path, &["rev-parse", "HEAD"]).await {
        Ok(output) => output
            .lines()
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        Err(KnowledgeProviderError::Rejected) => None,
        Err(error) => return Err(error),
    };
    let status = match git_output_bytes(path, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await {
        Ok(output) => output,
        Err(KnowledgeProviderError::Rejected) => Vec::new(),
        Err(error) => return Err(error),
    };
    let mut changed_paths = Vec::new();
    let mut records = status.split(|byte| *byte == 0).filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if record.len() < 4 {
            return Err(KnowledgeProviderError::MalformedOutput);
        }
        let path_bytes = &record[3..];
        let changed_path = std::str::from_utf8(path_bytes).map_err(|_| KnowledgeProviderError::MalformedOutput)?;
        validate_provider_path(changed_path)?;
        changed_paths.push(changed_path.to_owned());
        if record
            .get(..2)
            .is_some_and(|status| status.contains(&b'R') || status.contains(&b'C'))
        {
            let original = records.next().ok_or(KnowledgeProviderError::MalformedOutput)?;
            let original = std::str::from_utf8(original).map_err(|_| KnowledgeProviderError::MalformedOutput)?;
            validate_provider_path(original)?;
            changed_paths.push(original.to_owned());
        }
    }
    changed_paths.sort();
    changed_paths.dedup();
    Ok(RepositoryKnowledgeState {
        source_commit,
        changed_paths,
    })
}

async fn git_output(path: &str, arguments: &[&str]) -> Result<String, KnowledgeProviderError> {
    let output = git_output_bytes(path, arguments).await?;
    String::from_utf8(output).map_err(|_| KnowledgeProviderError::MalformedOutput)
}

async fn git_output_bytes(path: &str, arguments: &[&str]) -> Result<Vec<u8>, KnowledgeProviderError> {
    let mut command = Builder::clean_cli("git");
    command.args(arguments);
    command.current_dir(path);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| KnowledgeProviderError::Timeout)?
        .map_err(|_| KnowledgeProviderError::Rejected)?;
    if !output.status.success() {
        return Err(KnowledgeProviderError::Rejected);
    }
    Ok(output.stdout)
}

fn validate_provider_path(value: &str) -> Result<(), KnowledgeProviderError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(KnowledgeProviderError::MalformedOutput);
    }
    Ok(())
}

fn collect_facts(value: &Value, default_kind: &str, facts: &mut Vec<ProjectKnowledgeFact>) {
    if facts.len() >= MAX_FACTS {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_facts(item, default_kind, facts);
            }
        }
        Value::Object(object) => {
            let name = object.get("name").and_then(Value::as_str);
            let source_path = object
                .get("file_path")
                .or_else(|| object.get("source_path"))
                .and_then(Value::as_str);
            if let (Some(name), Some(source_path)) = (name, source_path)
                && validate_provider_path(source_path).is_ok()
            {
                let label = object.get("label").and_then(Value::as_str).unwrap_or(default_kind);
                let kind = normalize_kind(label, source_path, default_kind);
                facts.push(ProjectKnowledgeFact {
                    kind,
                    name: name.into(),
                    qualified_name: object.get("qualified_name").and_then(Value::as_str).map(str::to_owned),
                    source_path: source_path.into(),
                    source_line: object
                        .get("start_line")
                        .or_else(|| object.get("source_line"))
                        .and_then(Value::as_i64)
                        .filter(|line| *line > 0),
                    indexed_at: 0,
                });
            }
            for child in object.values() {
                if child.is_array() || child.is_object() {
                    collect_facts(child, default_kind, facts);
                }
            }
        }
        _ => {}
    }
}

fn normalize_kind(label: &str, source_path: &str, default_kind: &str) -> String {
    if source_path.contains("test") || label.eq_ignore_ascii_case("test") {
        "test".into()
    } else {
        match label.to_ascii_lowercase().as_str() {
            "route" => "route".into(),
            "table" | "entity" | "data_entity" => "data_entity".into(),
            "caller" => "caller".into(),
            "architecture" => "architecture".into(),
            _ => default_kind.into(),
        }
    }
}

fn bounded(values: BTreeSet<String>) -> Vec<String> {
    values.into_iter().take(MAX_CONTEXT_ITEMS).collect()
}

fn collect_qualified_names(value: &Value, names: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_qualified_names(item, names);
            }
        }
        Value::Object(object) => {
            if let Some(name) = object.get("qualified_name").and_then(Value::as_str)
                && !name.is_empty()
            {
                names.insert(name.into());
            }
            for child in object.values() {
                if child.is_array() || child.is_object() {
                    collect_qualified_names(child, names);
                }
            }
        }
        _ => {}
    }
}

fn escape_regex(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}
