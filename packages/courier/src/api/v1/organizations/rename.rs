//! Rename organization endpoint.

use aerosol::axum::Dep;
use axum::{Json, extract::Path, http::StatusCode, response::IntoResponse};
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info, warn};

use crate::{
    auth::{ApiError, OrgId, SessionContext},
    db::Postgres,
};

#[derive(Debug, Deserialize)]
pub struct RenameOrganizationRequest {
    /// The new name for the organization.
    pub name: String,
}

/// Rename an organization. Only admins can perform this action.
#[tracing::instrument(skip(db, session))]
pub async fn handle(
    Dep(db): Dep<Postgres>,
    session: SessionContext,
    Path(org_id): Path<i64>,
    Json(request): Json<RenameOrganizationRequest>,
) -> Result<Response, ApiError> {
    let org_id = OrgId::from_i64(org_id);

    // Validate name is not empty
    let name = request.name.trim();
    if name.is_empty() {
        warn!(
            account_id = %session.account_id,
            org_id = %org_id,
            "organizations.rename.empty_name"
        );
        return Ok(Response::EmptyName);
    }

    // Verify admin access using strongly typed role check
    let admin = session.try_admin(&db, org_id).await?;

    match db.rename_organization(org_id, name).await {
        Ok(true) => {
            let _ = db
                .log_audit_event(
                    Some(admin.account_id),
                    Some(org_id),
                    "organization.renamed",
                    Some(json!({
                        "new_name": name,
                    })),
                )
                .await;

            info!(
                org_id = %org_id,
                new_name = %name,
                "organizations.rename.success"
            );
            Ok(Response::Success)
        }
        Ok(false) => Ok(Response::NotFound),
        Err(error) => {
            error!(?error, "organizations.rename.error");
            Ok(Response::Error(error.to_string()))
        }
    }
}

#[derive(Debug)]
pub enum Response {
    Success,
    EmptyName,
    NotFound,
    Error(String),
}

impl IntoResponse for Response {
    fn into_response(self) -> axum::response::Response {
        match self {
            Response::Success => StatusCode::NO_CONTENT.into_response(),
            Response::EmptyName => {
                (StatusCode::BAD_REQUEST, "Organization name cannot be empty").into_response()
            }
            Response::NotFound => (StatusCode::NOT_FOUND, "Organization not found").into_response(),
            Response::Error(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        }
    }
}
