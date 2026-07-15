use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use dashmap::DashMap;
use getrandom::getrandom;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeTokenScope {
    TeamContext,
    TeamCall,
}

impl RuntimeTokenScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TeamContext => "team:context",
            Self::TeamCall => "team:call",
        }
    }
}

#[derive(Clone)]
pub struct RuntimeTokenIssue {
    pub token: String,
    pub claims: RuntimeTokenClaims,
}

impl std::fmt::Debug for RuntimeTokenIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeTokenIssue")
            .field("token", &"<redacted>")
            .field("claims", &self.claims)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTokenClaims {
    pub user_id: String,
    pub conversation_id: String,
    pub scopes: HashSet<RuntimeTokenScope>,
    pub issued_at: SystemTime,
    pub expires_at: SystemTime,
    pub session_generation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTokenError {
    Missing,
    Unknown,
    Expired,
    UserMismatch,
    ConversationMismatch,
    ScopeMissing,
    GenerationMismatch,
}

#[derive(Default)]
pub struct RuntimeTokenService {
    tokens: DashMap<String, RuntimeTokenClaims>,
}

impl RuntimeTokenService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(
        &self,
        user_id: impl Into<String>,
        conversation_id: impl Into<String>,
        session_generation: impl Into<String>,
        scopes: impl IntoIterator<Item = RuntimeTokenScope>,
        ttl: Duration,
    ) -> RuntimeTokenIssue {
        let token = generate_token();
        let now = SystemTime::now();
        let claims = RuntimeTokenClaims {
            user_id: user_id.into(),
            conversation_id: conversation_id.into(),
            scopes: scopes.into_iter().collect(),
            issued_at: now,
            expires_at: now + ttl,
            session_generation: session_generation.into(),
        };
        self.invalidate_conversation(&claims.user_id, &claims.conversation_id);
        self.tokens.insert(token_hash(&token), claims.clone());
        RuntimeTokenIssue { token, claims }
    }

    pub fn validate(
        &self,
        raw_token: Option<&str>,
        user_id: &str,
        conversation_id: &str,
        scope: RuntimeTokenScope,
        session_generation: &str,
    ) -> Result<RuntimeTokenClaims, RuntimeTokenError> {
        let raw_token = raw_token
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(RuntimeTokenError::Missing)?;
        let Some(entry) = self.tokens.get(&token_hash(raw_token)) else {
            return Err(RuntimeTokenError::Unknown);
        };
        let claims = entry.value().clone();
        drop(entry);
        if SystemTime::now() >= claims.expires_at {
            self.tokens.remove(&token_hash(raw_token));
            return Err(RuntimeTokenError::Expired);
        }
        if claims.user_id != user_id {
            return Err(RuntimeTokenError::UserMismatch);
        }
        if claims.conversation_id != conversation_id {
            return Err(RuntimeTokenError::ConversationMismatch);
        }
        if !claims.scopes.contains(&scope) {
            return Err(RuntimeTokenError::ScopeMissing);
        }
        if claims.session_generation != session_generation {
            return Err(RuntimeTokenError::GenerationMismatch);
        }
        Ok(claims)
    }

    pub fn invalidate_conversation(&self, user_id: &str, conversation_id: &str) {
        self.tokens
            .retain(|_, claims| !(claims.user_id == user_id && claims.conversation_id == conversation_id));
    }

    pub fn invalidate_generation(&self, conversation_id: &str, session_generation: &str) {
        self.tokens.retain(|_, claims| {
            !(claims.conversation_id == conversation_id && claims.session_generation == session_generation)
        });
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom(&mut bytes).expect("OS random source must be available");
    hex::encode(bytes)
}

fn token_hash(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_issued_token_for_matching_runtime_context() {
        let service = RuntimeTokenService::new();
        let issue = service.issue(
            "user-1",
            "conv-1",
            "gen-1",
            [RuntimeTokenScope::TeamContext, RuntimeTokenScope::TeamCall],
            Duration::from_secs(60),
        );

        let claims = service
            .validate(
                Some(&issue.token),
                "user-1",
                "conv-1",
                RuntimeTokenScope::TeamCall,
                "gen-1",
            )
            .unwrap();
        assert_eq!(claims.user_id, "user-1");
        assert!(claims.scopes.contains(&RuntimeTokenScope::TeamCall));
    }

    #[test]
    fn rejects_missing_mismatched_scope_and_generation() {
        let service = RuntimeTokenService::new();
        let issue = service.issue(
            "user-1",
            "conv-1",
            "gen-1",
            [RuntimeTokenScope::TeamContext],
            Duration::from_secs(60),
        );
        assert_eq!(
            service.validate(None, "user-1", "conv-1", RuntimeTokenScope::TeamContext, "gen-1"),
            Err(RuntimeTokenError::Missing)
        );
        assert_eq!(
            service.validate(
                Some(&issue.token),
                "user-1",
                "conv-1",
                RuntimeTokenScope::TeamCall,
                "gen-1"
            ),
            Err(RuntimeTokenError::ScopeMissing)
        );
        assert_eq!(
            service.validate(
                Some(&issue.token),
                "user-1",
                "conv-1",
                RuntimeTokenScope::TeamContext,
                "gen-2"
            ),
            Err(RuntimeTokenError::GenerationMismatch)
        );
    }

    #[test]
    fn invalidation_removes_matching_conversation_tokens() {
        let service = RuntimeTokenService::new();
        let issue = service.issue(
            "user-1",
            "conv-1",
            "gen-1",
            [RuntimeTokenScope::TeamContext],
            Duration::from_secs(60),
        );
        service.invalidate_conversation("user-1", "conv-1");
        assert_eq!(
            service.validate(
                Some(&issue.token),
                "user-1",
                "conv-1",
                RuntimeTokenScope::TeamContext,
                "gen-1"
            ),
            Err(RuntimeTokenError::Unknown)
        );
    }

    #[test]
    fn issuing_replacement_token_invalidates_previous_conversation_token() {
        let service = RuntimeTokenService::new();
        let first = service.issue(
            "user-1",
            "conv-1",
            "gen-1",
            [RuntimeTokenScope::TeamContext, RuntimeTokenScope::TeamCall],
            Duration::from_secs(60),
        );
        let second = service.issue(
            "user-1",
            "conv-1",
            "gen-2",
            [RuntimeTokenScope::TeamContext, RuntimeTokenScope::TeamCall],
            Duration::from_secs(60),
        );

        assert_eq!(
            service.validate(
                Some(&first.token),
                "user-1",
                "conv-1",
                RuntimeTokenScope::TeamCall,
                "gen-1"
            ),
            Err(RuntimeTokenError::Unknown)
        );
        service
            .validate(
                Some(&second.token),
                "user-1",
                "conv-1",
                RuntimeTokenScope::TeamCall,
                "gen-2",
            )
            .unwrap();
    }

    #[test]
    fn debug_redacts_raw_token() {
        let service = RuntimeTokenService::new();
        let issue = service.issue(
            "user-1",
            "conv-1",
            "gen-1",
            [RuntimeTokenScope::TeamContext],
            Duration::from_secs(60),
        );
        let debug = format!("{issue:?}");
        assert!(!debug.contains(&issue.token));
        assert!(debug.contains("<redacted>"));
    }
}
