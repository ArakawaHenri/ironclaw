//! Invitation management API handlers.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::channels::web::auth::AuthenticatedUser;
use crate::channels::web::server::GatewayState;
use crate::db::{InvitationRecord, UserRecord};

/// POST /api/invitations — create an invitation.
pub async fn invitations_create_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(user): AuthenticatedUser,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    let email = body.get("email").and_then(|v| v.as_str()).map(String::from);

    let expires_in_days = body
        .get("expires_in_days")
        .and_then(|v| v.as_u64())
        .unwrap_or(7);

    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::days(expires_in_days as i64);

    // Generate 32 random bytes for the invite token.
    let mut token_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut token_bytes);
    let plaintext_token = hex::encode(token_bytes);

    // SHA-256 hash for storage — plaintext is never persisted.
    let mut hasher = Sha256::new();
    hasher.update(token_bytes);
    let hash: [u8; 32] = hasher.finalize().into();

    let invitation_id = Uuid::new_v4();
    let invitation = InvitationRecord {
        id: invitation_id,
        email: email.clone(),
        invited_by: user.user_id.clone(),
        status: "pending".to_string(),
        expires_at,
        accepted_at: None,
        accepted_by: None,
        created_at: now,
    };

    store
        .create_invitation(&invitation, &hash)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Return the plaintext token — this is the ONLY time it is shown.
    Ok(Json(serde_json::json!({
        "invite_token": plaintext_token,
        "id": invitation_id.to_string(),
        "email": email,
        "expires_at": expires_at.to_rfc3339(),
        "created_at": now.to_rfc3339(),
    })))
}

/// GET /api/invitations — list invitations created by the current user.
pub async fn invitations_list_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(user): AuthenticatedUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    let invitations = store
        .list_invitations(Some(&user.user_id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let invitations_json: Vec<serde_json::Value> = invitations
        .into_iter()
        .map(|inv| {
            serde_json::json!({
                "id": inv.id.to_string(),
                "email": inv.email,
                "invited_by": inv.invited_by,
                "status": inv.status,
                "expires_at": inv.expires_at.to_rfc3339(),
                "accepted_at": inv.accepted_at.map(|dt| dt.to_rfc3339()),
                "accepted_by": inv.accepted_by,
                "created_at": inv.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "invitations": invitations_json })))
}

/// POST /api/invitations/accept — accept an invitation and create a user account.
pub async fn invitations_accept_handler(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    let invite_token = body.get("invite_token").and_then(|v| v.as_str()).ok_or((
        StatusCode::BAD_REQUEST,
        "Missing required field 'invite_token'".to_string(),
    ))?;

    let display_name = body
        .get("display_name")
        .and_then(|v| v.as_str())
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Missing required field 'display_name'".to_string(),
        ))?
        .to_string();

    // Hash the provided token to look up the invitation.
    let token_bytes = hex::decode(invite_token).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "Invalid invite token format".to_string(),
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&token_bytes);
    let hash: [u8; 32] = hasher.finalize().into();

    // Look up the invitation by hash.
    let invitation = store
        .get_invitation_by_hash(&hash)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "Invitation not found or already used".to_string(),
        ))?;

    // Verify the invitation is still pending.
    if invitation.status != "pending" {
        return Err((
            StatusCode::CONFLICT,
            format!("Invitation is already '{}'", invitation.status),
        ));
    }

    // Verify the invitation has not expired.
    if invitation.expires_at < chrono::Utc::now() {
        return Err((StatusCode::GONE, "Invitation has expired".to_string()));
    }

    let new_user_id = Uuid::new_v4().to_string();

    let now = chrono::Utc::now();
    let user_record = UserRecord {
        id: new_user_id.clone(),
        email: invitation.email.clone(),
        display_name: display_name.clone(),
        status: "active".to_string(),
        created_at: now,
        updated_at: now,
        last_login_at: None,
        created_by: Some(invitation.invited_by.clone()),
        metadata: serde_json::json!({}),
    };

    store
        .create_user(&user_record)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Create a first API token for the new user.
    let mut api_token_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut api_token_bytes);
    let plaintext_api_token = hex::encode(api_token_bytes);

    let mut api_hasher = Sha256::new();
    api_hasher.update(api_token_bytes);
    let api_hash: [u8; 32] = api_hasher.finalize().into();

    let api_prefix = &plaintext_api_token[..8];

    let api_token_record = store
        .create_api_token(&new_user_id, "default", &api_hash, api_prefix, None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Mark the invitation as accepted.
    store
        .accept_invitation(invitation.id, &new_user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "user": {
            "id": new_user_id,
            "email": user_record.email,
            "display_name": user_record.display_name,
            "status": "active",
            "created_at": now.to_rfc3339(),
        },
        "api_token": {
            "token": plaintext_api_token,
            "id": api_token_record.id.to_string(),
            "name": api_token_record.name,
            "token_prefix": api_token_record.token_prefix,
            "created_at": api_token_record.created_at.to_rfc3339(),
        },
        "invitation_id": invitation.id.to_string(),
    })))
}
