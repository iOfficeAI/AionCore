use std::sync::Arc;

use aionui_api_types::{
    EnsureExternalSessionRequest, EnsureExternalSessionResponse, EnsureExternalUserRequest, EnsureExternalUserResponse,
    ExternalUserType, PublicUser, RevokeExternalSessionRequest, RevokeExternalSessionResponse,
};
use aionui_db::{ExternalUserProjection, IUserRepository, UserStatus, UserType, models::User};

use crate::JwtService;

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error("unsupported external user type")]
    UnsupportedUserType,
    #[error("external user is disabled")]
    UserDisabled,
    #[error("external user is not provisioned")]
    UserNotProvisioned,
    #[error("database error: {0}")]
    Db(#[from] aionui_db::DbError),
    #[error("token signing failed: {0}")]
    Token(#[from] crate::AuthError),
}

/// Port for the on-disk side of AionUi → AionPro adoption. When DB adoption
/// re-owns `system_default_user`'s rows to the first external user, this moves
/// the corresponding per-user files (and rewrites stored paths). Implemented
/// in the composition layer over the skill filesystem so `aionui-auth` stays
/// free of filesystem/extension dependencies. Best-effort: never fails
/// provisioning.
#[async_trait::async_trait]
pub trait SystemDefaultFilesystemAdopter: Send + Sync {
    async fn adopt_filesystem(&self, adopter_user_id: &str);
}

#[derive(Clone)]
pub struct AuthProvisionService {
    user_repo: Arc<dyn IUserRepository>,
    jwt_service: Arc<JwtService>,
    fs_adopter: Option<Arc<dyn SystemDefaultFilesystemAdopter>>,
}

pub struct ExternalSessionExchange {
    pub response: EnsureExternalSessionResponse,
    pub token: String,
}

impl AuthProvisionService {
    pub fn new(user_repo: Arc<dyn IUserRepository>, jwt_service: Arc<JwtService>) -> Self {
        Self {
            user_repo,
            jwt_service,
            fs_adopter: None,
        }
    }

    pub fn with_filesystem_adopter(mut self, fs_adopter: Arc<dyn SystemDefaultFilesystemAdopter>) -> Self {
        self.fs_adopter = Some(fs_adopter);
        self
    }

    pub async fn ensure_external_user(
        &self,
        external_user_id: &str,
        request: EnsureExternalUserRequest,
    ) -> Result<EnsureExternalUserResponse, ProvisionError> {
        let user_type = map_external_user_type(request.user_type)?;
        let user = self
            .user_repo
            .ensure_external_user(
                user_type,
                external_user_id,
                ExternalUserProjection {
                    username: request.username,
                    email: request.email,
                    avatar_path: request.avatar_path,
                },
            )
            .await?;

        if user.status == UserStatus::Disabled {
            return Err(ProvisionError::UserDisabled);
        }

        // Upgrade path (AionUi -> AionPro over the same database): while this
        // is the machine's only external user, re-own the local default
        // user's pre-multi-account data to it. Self-idempotent — moves
        // nothing once the source set is empty or a second account exists.
        let adopted = self.user_repo.adopt_system_default_data(&user.id).await?;
        if adopted > 0 {
            tracing::info!(user_id = %user.id, rows = adopted, "adopted system_default_user data into first external user");
        }
        // Move the corresponding on-disk files to the adopter's per-user roots
        // so the upgraded account can see its files (DB rows already point at
        // it). Gated on the recorded one-shot adopter marker, NOT on "rows
        // moved this call": the on-disk move is idempotent and MUST be able to
        // re-run after a partial failure — the first login stamps the marker
        // yet may fail halfway through the file move, so a later login of the
        // SAME account has to pick up the leftover files. Only the stamped
        // adopter matches, so files can never leak to any other account.
        // Best-effort; never fails provisioning.
        if let Some(fs_adopter) = &self.fs_adopter
            && self.user_repo.is_default_data_adopter(&user.id).await?
        {
            fs_adopter.adopt_filesystem(&user.id).await;
        }

        Ok(external_user_response(user, request.user_type))
    }

    pub async fn create_external_session(
        &self,
        request: EnsureExternalSessionRequest,
    ) -> Result<ExternalSessionExchange, ProvisionError> {
        let user_type = map_external_user_type(request.user_type)?;
        let user = self
            .user_repo
            .find_by_external_user_id(user_type, &request.external_user_id)
            .await?
            .ok_or(ProvisionError::UserNotProvisioned)?;

        if user.status == UserStatus::Disabled {
            return Err(ProvisionError::UserDisabled);
        }

        let username = user.username.clone().unwrap_or_else(|| "external_user".to_string());
        let token = self
            .jwt_service
            .sign_with_session_generation(&user.id, &username, user.session_generation)?;
        self.user_repo.update_last_login(&user.id).await?;

        Ok(ExternalSessionExchange {
            response: EnsureExternalSessionResponse {
                user: PublicUser { id: user.id, username },
                session_generation: user.session_generation,
            },
            token,
        })
    }

    pub async fn revoke_external_session(
        &self,
        request: RevokeExternalSessionRequest,
    ) -> Result<RevokeExternalSessionResponse, ProvisionError> {
        let user_type = map_external_user_type(request.user_type)?;
        let user = self
            .user_repo
            .find_by_external_user_id(user_type, &request.external_user_id)
            .await?
            .ok_or(ProvisionError::UserNotProvisioned)?;
        let session_generation = self.user_repo.increment_session_generation(&user.id).await?;
        Ok(RevokeExternalSessionResponse {
            user_id: user.id,
            session_generation,
        })
    }
}

fn map_external_user_type(user_type: ExternalUserType) -> Result<UserType, ProvisionError> {
    match user_type {
        ExternalUserType::Aionpro => Ok(UserType::Aionpro),
    }
}

fn external_user_response(user: User, user_type: ExternalUserType) -> EnsureExternalUserResponse {
    EnsureExternalUserResponse {
        user_id: user.id,
        user_type,
        external_user_id: user.external_user_id.unwrap_or_default(),
        session_generation: user.session_generation,
    }
}
