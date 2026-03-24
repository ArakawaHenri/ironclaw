//! User management API handlers (admin).

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};

use crate::channels::web::auth::AuthenticatedUser;
use crate::channels::web::server::GatewayState;
use crate::db::UserRecord;

/// POST /api/admin/users — create a new user.
pub async fn users_create_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(user): AuthenticatedUser,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    let display_name = body
        .get("display_name")
        .and_then(|v| v.as_str())
        .ok_or((
            StatusCode::BAD_REQUEST,
            "Missing required field 'display_name'".to_string(),
        ))?
        .to_string();

    let email = body.get("email").and_then(|v| v.as_str()).map(String::from);

    // Generate user id: prefer email if provided, otherwise derive from display_name.
    let user_id = if let Some(ref e) = email {
        e.clone()
    } else {
        display_name
            .to_ascii_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
    };

    let now = chrono::Utc::now();
    let user_record = UserRecord {
        id: user_id.clone(),
        email,
        display_name: display_name.clone(),
        status: "active".to_string(),
        created_at: now,
        updated_at: now,
        last_login_at: None,
        created_by: Some(user.user_id.clone()),
        metadata: serde_json::json!({}),
    };

    store
        .create_user(&user_record)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "id": user_record.id,
        "email": user_record.email,
        "display_name": user_record.display_name,
        "status": user_record.status,
        "created_at": user_record.created_at.to_rfc3339(),
        "created_by": user_record.created_by,
    })))
}

/// GET /api/admin/users — list all users.
pub async fn users_list_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(_user): AuthenticatedUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    let users = store
        .list_users(None)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let users_json: Vec<serde_json::Value> = users
        .into_iter()
        .map(|u| {
            serde_json::json!({
                "id": u.id,
                "email": u.email,
                "display_name": u.display_name,
                "status": u.status,
                "created_at": u.created_at.to_rfc3339(),
                "updated_at": u.updated_at.to_rfc3339(),
                "last_login_at": u.last_login_at.map(|dt| dt.to_rfc3339()),
                "created_by": u.created_by,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "users": users_json })))
}

/// GET /api/admin/users/{id} — get a single user.
pub async fn users_detail_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    let user_record = store
        .get_user(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "User not found".to_string()))?;

    Ok(Json(serde_json::json!({
        "id": user_record.id,
        "email": user_record.email,
        "display_name": user_record.display_name,
        "status": user_record.status,
        "created_at": user_record.created_at.to_rfc3339(),
        "updated_at": user_record.updated_at.to_rfc3339(),
        "last_login_at": user_record.last_login_at.map(|dt| dt.to_rfc3339()),
        "created_by": user_record.created_by,
        "metadata": user_record.metadata,
    })))
}

/// PATCH /api/admin/users/{id} — update a user's profile.
pub async fn users_update_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    // Verify the user exists.
    let existing = store
        .get_user(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "User not found".to_string()))?;

    let display_name = body
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(&existing.display_name);

    let metadata = body.get("metadata").unwrap_or(&existing.metadata);

    store
        .update_user_profile(&id, display_name, metadata)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Re-fetch the updated record to return consistent data.
    let updated = store
        .get_user(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "User not found".to_string()))?;

    Ok(Json(serde_json::json!({
        "id": updated.id,
        "email": updated.email,
        "display_name": updated.display_name,
        "status": updated.status,
        "created_at": updated.created_at.to_rfc3339(),
        "updated_at": updated.updated_at.to_rfc3339(),
        "metadata": updated.metadata,
    })))
}

/// POST /api/admin/users/{id}/suspend — suspend a user.
pub async fn users_suspend_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    // Verify the user exists.
    store
        .get_user(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "User not found".to_string()))?;

    store
        .update_user_status(&id, "suspended")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "id": id,
        "status": "suspended",
    })))
}

/// POST /api/admin/users/{id}/activate — activate a user.
pub async fn users_activate_handler(
    State(state): State<Arc<GatewayState>>,
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = state.store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Database not available".to_string(),
    ))?;

    // Verify the user exists.
    store
        .get_user(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "User not found".to_string()))?;

    store
        .update_user_status(&id, "active")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({
        "id": id,
        "status": "active",
    })))
}
