//! E2B cloud-control-plane handlers.
//!
//! The runtime handlers in `e2b.rs` own envd-compatible sandbox I/O. This
//! module owns the Team-scoped platform resources exposed by the pinned E2B
//! OpenAPI contract.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde_json::{Value, json};

use clouisle_core::{ClouisleError, SandboxSpec, SandboxStatus};

use crate::auth::{Principal, Scope};
use crate::e2b_cloud::{BuildRecord, ControlPlaneError, ScopeRecord, TemplateRecord};
use crate::error::ApiError;
use crate::state::AppState;

fn handle_for(sandbox: &clouisle_core::Sandbox) -> clouisle_vmm::VmHandle {
    clouisle_vmm::VmHandle {
        id: sandbox
            .vmm_meta
            .vmm_id
            .clone()
            .unwrap_or_else(|| sandbox.id.clone()),
        backend: sandbox.vmm_meta.backend.clone(),
        owner_id: sandbox.vmm_meta.owner_id.clone(),
        pid: sandbox.vmm_meta.pid,
        api_socket: sandbox.vmm_meta.api_socket.clone(),
        vsock_socket: sandbox.vmm_meta.vsock_socket.clone(),
        vsock_cid: sandbox.vmm_meta.vsock_cid,
        subnet: None,
    }
}

fn team_header(headers: &HeaderMap, principal: &Principal) -> Result<String, ApiError> {
    if principal.volume_id.is_some() {
        return Err(ApiError(ClouisleError::new(
            clouisle_core::ErrorKind::Forbidden,
            "volume credential cannot manage team resources",
        )));
    }
    if let Some(value) = headers.get("x-team-id") {
        let requested = value
            .to_str()
            .map_err(|_| ApiError(ClouisleError::validation("invalid X-Team-ID")))?;
        if requested != principal.tenant_id {
            return Err(ApiError(ClouisleError::not_found("team not found")));
        }
    }
    Ok(principal.tenant_id.clone())
}

fn require_volume_access(principal: &Principal, volume_id: &str) -> Result<(), ApiError> {
    if principal
        .volume_id
        .as_deref()
        .is_some_and(|allowed| allowed != volume_id)
    {
        return Err(ApiError(ClouisleError::not_found("volume not found")));
    }
    Ok(())
}

fn require_admin(headers: &HeaderMap) -> Result<(), ApiError> {
    let configured = std::env::var("CLOUISLE_ADMIN_TOKEN").ok();
    let supplied = headers
        .get("x-admin-token")
        .or_else(|| headers.get("authorization"))
        .and_then(|value| value.to_str().ok())
        .map(|value| value.strip_prefix("Bearer ").unwrap_or(value));
    if configured
        .as_deref()
        .is_some_and(|token| Some(token) == supplied)
    {
        return Ok(());
    }
    Err(ApiError(ClouisleError::new(
        clouisle_core::ErrorKind::Forbidden,
        "admin authentication required",
    )))
}

fn team_json(team: &crate::e2b_cloud::TeamRecord) -> Value {
    json!({
        "teamID": team.team_id,
        "name": team.name,
        "apiKey": format!("team_{}", &team.team_id[..team.team_id.len().min(8)]),
        "isDefault": team.is_default,
    })
}

fn response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

fn error_response(error: ControlPlaneError) -> ApiError {
    error.into()
}

fn page<T: Clone>(items: &[T], query: &HashMap<String, String>) -> (Vec<T>, Option<String>) {
    let limit = query
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100)
        .clamp(1, 100);
    let offset = query
        .get("nextToken")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let end = (offset + limit).min(items.len());
    let next = (end < items.len()).then(|| end.to_string());
    (items[offset..end].to_vec(), next)
}

fn with_next_token(mut response: Response, next: Option<String>) -> Response {
    if let Some(next) = next
        && let Ok(value) = HeaderValue::from_str(&next)
    {
        response.headers_mut().insert("x-next-token", value);
    }
    response
}

fn template_json(template: &TemplateRecord, build: Option<&BuildRecord>) -> Value {
    json!({
        "templateID": template.template_id,
        "buildID": template.latest_build_id.clone().or_else(|| build.map(|build| build.build_id.clone())).unwrap_or_default(),
        "cpuCount": 1,
        "memoryMB": 128,
        "diskSizeMB": 1024,
        "public": template.public,
        "aliases": template.aliases,
        "names": template.names,
        "tags": template.tags,
        "createdAt": template.created_at,
        "updatedAt": template.updated_at,
        "createdBy": Value::Null,
        "lastSpawnedAt": Value::Null,
        "spawnCount": 0,
        "buildCount": template.build_ids.len(),
        "envdVersion": env!("CARGO_PKG_VERSION"),
        "buildStatus": build.map(|build| build.status.clone()).unwrap_or_else(|| "unknown".into()),
    })
}

pub async fn list_teams(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> Result<Json<Vec<Value>>, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    let teams = state
        .e2b
        .list_teams(&team_id)
        .await
        .map_err(error_response)?
        .iter()
        .map(team_json)
        .collect();
    Ok(Json(teams))
}

pub async fn create_team(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError(ClouisleError::validation("name is required")))?;
    let team = state
        .e2b
        .ensure_team(&principal.tenant_id, Some(name))
        .await
        .map_err(error_response)?;
    Ok(response(StatusCode::CREATED, team_json(&team)))
}

pub async fn team_members(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(team_id): Path<String>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let current = team_header(&headers, &principal)?;
    if current != team_id {
        return Err(ApiError(ClouisleError::not_found("team not found")));
    }
    let members = state
        .e2b
        .team_members(&team_id)
        .await
        .map_err(error_response)?;
    Ok(Json(
        members
            .into_iter()
            .map(|member| json!({"id": member.user_id, "email": member.email, "role": member.role}))
            .collect(),
    ))
}

pub async fn team_metrics(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(team_id): Path<String>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let current = team_header(&headers, &principal)?;
    if current != team_id {
        return Err(ApiError(ClouisleError::not_found("team not found")));
    }
    let sandboxes = state
        .store
        .list_sandboxes(None)
        .await?
        .into_iter()
        .filter(|sandbox| sandbox.spec.tenant_id.as_deref() == Some(team_id.as_str()))
        .collect::<Vec<_>>();
    Ok(Json(vec![json!({
        "timestamp": Utc::now(),
        "timestampUnix": Utc::now().timestamp(),
        "concurrentSandboxes": sandboxes.iter().filter(|sandbox| sandbox.status.is_active()).count(),
        "sandboxStartRate": 0.0,
    })]))
}

pub async fn team_metrics_max(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(team_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let current = team_header(&headers, &principal)?;
    if current != team_id {
        return Err(ApiError(ClouisleError::not_found("team not found")));
    }
    let metric = query
        .get("metric")
        .ok_or_else(|| ApiError(ClouisleError::validation("metric is required")))?;
    if metric != "concurrent_sandboxes" && metric != "sandbox_start_rate" {
        return Err(ApiError(ClouisleError::validation("unknown metric")));
    }
    let value = if metric == "concurrent_sandboxes" {
        state
            .store
            .list_sandboxes(None)
            .await?
            .into_iter()
            .filter(|sandbox| {
                sandbox.spec.tenant_id.as_deref() == Some(team_id.as_str())
                    && sandbox.status.is_active()
            })
            .count() as f64
    } else {
        0.0
    };
    Ok(Json(vec![
        json!({"timestamp": Utc::now(), "timestampUnix": Utc::now().timestamp(), "value": value}),
    ]))
}

pub async fn list_api_keys(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> Result<Json<Vec<Value>>, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    Ok(Json(
        state
            .e2b
            .list_api_keys(&team_id)
            .await
            .map_err(error_response)?,
    ))
}

fn requested_scope(body: &Value) -> ScopeRecord {
    match body.get("scope").and_then(Value::as_str) {
        Some("read") => ScopeRecord::Read,
        Some("admin") => ScopeRecord::Admin,
        _ => ScopeRecord::Full,
    }
}

pub async fn create_api_key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError(ClouisleError::validation("name is required")))?;
    let scope = requested_scope(&body);
    let created = state
        .e2b
        .create_api_key(&team_id, name, scope)
        .await
        .map_err(error_response)?;
    // Newly created keys must be accepted immediately by the middleware.
    if let (Some(id), Some(raw)) = (
        created.get("id").and_then(Value::as_str),
        created.get("key").and_then(Value::as_str),
    ) {
        state
            .auth
            .register_with_id(id, raw, &team_id, scope.into_scope())
            .await;
    }
    Ok(response(StatusCode::CREATED, created))
}

pub async fn update_api_key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<StatusCode, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError(ClouisleError::validation("name is required")))?;
    state
        .e2b
        .update_api_key(&team_id, &id, name)
        .await
        .map_err(error_response)?;
    Ok(StatusCode::OK)
}

pub async fn delete_api_key(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    state
        .e2b
        .delete_api_key(&team_id, &id)
        .await
        .map_err(error_response)?;
    state.auth.revoke_id(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn create_access_token(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError(ClouisleError::validation("name is required")))?;
    let created = state
        .e2b
        .create_access_token(&principal.tenant_id, name)
        .await
        .map_err(error_response)?;
    if let (Some(id), Some(raw)) = (
        created.get("id").and_then(Value::as_str),
        created.get("token").and_then(Value::as_str),
    ) {
        state
            .auth
            .register_with_id(id, raw, &principal.tenant_id, Scope::Full)
            .await;
    }
    Ok(response(StatusCode::CREATED, created))
}

pub async fn delete_access_token(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .e2b
        .delete_access_token(&principal.tenant_id, &id)
        .await
        .map_err(error_response)?;
    state.auth.revoke_id(&id).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_volumes(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
) -> Result<Json<Vec<Value>>, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    Ok(Json(
        state
            .e2b
            .list_volumes(&team_id)
            .await
            .map_err(error_response)?,
    ))
}

pub async fn create_volume(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError(ClouisleError::validation("name is required")))?;
    let created = state
        .e2b
        .create_volume(&team_id, name)
        .await
        .map_err(error_response)?;
    Ok(response(StatusCode::CREATED, created))
}

pub async fn get_volume(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(volume_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    Ok(Json(
        state
            .e2b
            .get_volume(&team_id, &volume_id)
            .await
            .map_err(error_response)?,
    ))
}

pub async fn delete_volume(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(volume_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let team_id = team_header(&headers, &principal)?;
    state
        .e2b
        .delete_volume(&team_id, &volume_id)
        .await
        .map_err(error_response)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn volume_path(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(volume_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    require_volume_access(&principal, &volume_id)?;
    let path = query.get("path").map(String::as_str).unwrap_or("/");
    let file = state
        .e2b
        .get_volume_file(&principal.tenant_id, &volume_id, path)
        .await;
    match file {
        Ok(file) => Ok(response(
            StatusCode::OK,
            json!({
                "name": path.rsplit('/').next().unwrap_or_default(),
                "path": path,
                "type": "file",
                "size": file.content.len(),
                "mode": file.mode,
                "modifiedTime": file.modified_at,
                "metadata": file.metadata,
            }),
        )),
        Err(ControlPlaneError::NotFound(_)) => Ok(response(
            StatusCode::OK,
            json!({
                "name": path.trim_matches('/').rsplit('/').next().unwrap_or_default(),
                "path": path,
                "type": "directory",
                "size": 0,
                "mode": 0o755,
            }),
        )),
        Err(error) => Err(error_response(error)),
    }
}

pub async fn volume_dir(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(volume_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Vec<Value>>, ApiError> {
    require_volume_access(&principal, &volume_id)?;
    let path = query.get("path").map(String::as_str).unwrap_or("/");
    let files = state
        .e2b
        .list_volume_files(&principal.tenant_id, &volume_id, path)
        .await
        .map_err(error_response)?;
    Ok(Json(
        files
            .into_iter()
            .map(|path| json!({"name": path.rsplit('/').next().unwrap_or_default(), "path": path, "type": "file"}))
            .collect(),
    ))
}

pub async fn volume_file(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(volume_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    method: Method,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    require_volume_access(&principal, &volume_id)?;
    let path = query.get("path").map(String::as_str).unwrap_or("/");
    if method == Method::GET {
        let file = state
            .e2b
            .get_volume_file(&principal.tenant_id, &volume_id, path)
            .await
            .map_err(error_response)?;
        return Ok((
            [(header::CONTENT_TYPE, "application/octet-stream")],
            file.content,
        )
            .into_response());
    }
    let metadata = headers
        .iter()
        .filter_map(|(key, value)| {
            key.as_str().strip_prefix("x-metadata-").and_then(|name| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.to_string()))
            })
        })
        .collect();
    state
        .e2b
        .put_volume_file(
            &principal.tenant_id,
            &volume_id,
            path,
            body.to_vec(),
            metadata,
        )
        .await
        .map_err(error_response)?;
    Ok(response(
        StatusCode::OK,
        json!([{"path": path, "name": path.rsplit('/').next().unwrap_or_default(), "type": "file"}]),
    ))
}

pub async fn list_templates(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let templates = state.e2b.list_templates(&principal.tenant_id).await;
    let values = templates
        .iter()
        .map(|template| template_json(template, None))
        .collect::<Vec<_>>();
    let (items, next) = page(&values, &query);
    Ok(with_next_token(
        response(StatusCode::OK, Value::Array(items)),
        next,
    ))
}

pub async fn create_template_v3(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| body.get("alias").and_then(Value::as_str));
    let mut template = state
        .e2b
        .create_template(
            &principal.tenant_id,
            name,
            body.get("alias").and_then(Value::as_str),
            body.get("public").and_then(Value::as_bool).unwrap_or(false),
            body.get("image")
                .or_else(|| body.get("fromImage"))
                .and_then(Value::as_str)
                .map(str::to_string),
        )
        .await
        .map_err(error_response)?;
    if body.get("tags").is_some() {
        template = state
            .e2b
            .update_template(&principal.tenant_id, &template.template_id, &body)
            .await
            .map_err(error_response)?;
    }
    let build = state
        .e2b
        .create_build(
            &principal.tenant_id,
            &template.template_id,
            body.clone(),
            template.image_reference.clone(),
        )
        .await
        .map_err(error_response)?;
    let team_id = principal.tenant_id.clone();
    let build_for_task = build.clone();
    let state_for_task = state.clone();
    tokio::spawn(async move {
        if let Err(error) =
            build_image_if_possible(&state_for_task, &team_id, &build_for_task).await
        {
            tracing::error!(build_id = %build_for_task.build_id, error = %error.0.message, "template image preparation failed");
        }
    });
    Ok(response(
        StatusCode::ACCEPTED,
        json!({
            "templateID": template.template_id,
            "buildID": build.build_id,
            "public": template.public,
            "aliases": template.aliases,
            "names": template.names,
            "tags": template.tags,
        }),
    ))
}

pub async fn create_template_v2(
    state: State<AppState>,
    principal: Extension<Principal>,
    body: Json<Value>,
) -> Result<Response, ApiError> {
    create_template_v3(state, principal, body).await
}

async fn build_image_if_possible(
    state: &AppState,
    team_id: &str,
    build: &BuildRecord,
) -> Result<BuildRecord, ApiError> {
    if let Ok(current) = state.e2b.build(team_id, &build.build_id).await
        && current.status == "cancelled"
    {
        return Ok(current);
    }
    let image = build
        .image_reference
        .clone()
        .or_else(|| {
            build
                .request
                .get("fromImage")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            build
                .request
                .get("image")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let Some(image) = image else {
        return state
            .e2b
            .update_build(
                team_id,
                &build.build_id,
                "failed",
                Some(json!({"message": "template build requires an OCI image reference; Dockerfile builder is not configured"})),
            )
            .await
            .map_err(error_response);
    };
    let mut spec = SandboxSpec::default();
    spec.image.reference = image;
    state
        .e2b
        .update_build(team_id, &build.build_id, "running", None)
        .await
        .map_err(error_response)?;
    match state.vmm.prefetch_image(&spec).await {
        Ok(()) => {
            let warm_slot_requested = if state.vmm.supports_detached_warm_pool() {
                state.warm_pool.warm(&spec).await.is_some()
            } else {
                false
            };
            state
                .e2b
                .update_build(
                    team_id,
                    &build.build_id,
                    "succeeded",
                    Some(json!({
                        "message": "OCI image prepared",
                        "warmSlotRequested": warm_slot_requested,
                    })),
                )
                .await
                .map_err(error_response)
        }
        Err(error) => state
            .e2b
            .update_build(
                team_id,
                &build.build_id,
                "failed",
                Some(json!({"message": error.message})),
            )
            .await
            .map_err(error_response),
    }
}

pub async fn get_template(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(template_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let template = state
        .e2b
        .template(&principal.tenant_id, &template_id)
        .await
        .map_err(error_response)?;
    let build = match template.latest_build_id.as_deref() {
        Some(id) => state.e2b.build(&principal.tenant_id, id).await.ok(),
        None => None,
    };
    Ok(Json(template_json(&template, build.as_ref())))
}

pub async fn list_template_builds(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(template_id): Path<String>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let template = state
        .e2b
        .template(&principal.tenant_id, &template_id)
        .await
        .map_err(error_response)?;
    let mut builds = Vec::new();
    for id in template.build_ids {
        if let Ok(build) = state.e2b.build(&principal.tenant_id, &id).await {
            builds.push(json!({"buildID": build.build_id, "templateID": build.template_id, "status": build.status, "createdAt": build.created_at, "updatedAt": build.updated_at}));
        }
    }
    Ok(Json(builds))
}

pub async fn start_template_build(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path((template_id, build_id)): Path<(String, String)>,
    _body: Option<Json<Value>>,
) -> Result<StatusCode, ApiError> {
    let build = state
        .e2b
        .build(&principal.tenant_id, &build_id)
        .await
        .map_err(error_response)?;
    if build.template_id != template_id {
        return Err(ApiError(ClouisleError::not_found("build not found")));
    }
    let state_for_task = state.clone();
    let team_id = principal.tenant_id.clone();
    tokio::spawn(async move {
        if let Err(error) = build_image_if_possible(&state_for_task, &team_id, &build).await {
            tracing::error!(build_id = %build.build_id, error = %error.0.message, "template build failed");
        }
    });
    Ok(StatusCode::ACCEPTED)
}

pub async fn build_template_v2(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path((template_id, build_id)): Path<(String, String)>,
    _body: Option<Json<Value>>,
) -> Result<StatusCode, ApiError> {
    start_template_build(
        State(state),
        Extension(principal),
        Path((template_id, build_id)),
        None,
    )
    .await
}

pub async fn get_build_status(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path((_template_id, build_id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let build = state
        .e2b
        .build(&principal.tenant_id, &build_id)
        .await
        .map_err(error_response)?;
    Ok(Json(
        json!({"buildID": build.build_id, "templateID": build.template_id, "status": build.status, "createdAt": build.created_at, "updatedAt": build.updated_at}),
    ))
}

pub async fn get_build_logs(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path((_template_id, build_id)): Path<(String, String)>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let build = state
        .e2b
        .build(&principal.tenant_id, &build_id)
        .await
        .map_err(error_response)?;
    Ok(Json(build.logs))
}

pub async fn cancel_builds(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(team_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&headers)?;
    let (cancelled, failed) = state
        .e2b
        .cancel_builds(&team_id)
        .await
        .map_err(error_response)?;
    Ok(Json(
        json!({"cancelledCount": cancelled, "failedCount": failed}),
    ))
}

pub async fn assign_template_tags(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let template_id = body
        .get("templateID")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError(ClouisleError::validation("templateID is required")))?;
    let tags = body
        .get("tags")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError(ClouisleError::validation("tags is required")))?;
    let mut current = state
        .e2b
        .template(&principal.tenant_id, template_id)
        .await
        .map_err(error_response)?;
    for tag in tags.iter().filter_map(Value::as_str) {
        current = state
            .e2b
            .add_template_tag(&principal.tenant_id, template_id, tag)
            .await
            .map_err(error_response)?;
    }
    Ok(Json(json!({"tags": current.tags})))
}

pub async fn list_template_tags(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(template_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let template = state
        .e2b
        .template(&principal.tenant_id, &template_id)
        .await
        .map_err(error_response)?;
    Ok(Json(json!({"tags": template.tags})))
}

pub async fn resolve_template_alias(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(alias): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let template = state
        .e2b
        .list_templates(&principal.tenant_id)
        .await
        .into_iter()
        .find(|template| {
            template.aliases.iter().any(|value| value == &alias)
                || template.names.iter().any(|value| value == &alias)
        })
        .ok_or_else(|| ApiError(ClouisleError::not_found("template alias not found")))?;
    Ok(Json(
        json!({"templateID": template.template_id, "public": template.public}),
    ))
}

pub async fn sandbox_logs(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sandbox_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sandbox = state.store.get_sandbox(&sandbox_id).await?;
    if sandbox.spec.tenant_id.as_deref() != Some(principal.tenant_id.as_str()) {
        return Err(ApiError(ClouisleError::not_found("sandbox not found")));
    }
    let records = state.store.list_executions(&sandbox_id).await?;
    let mut logs = Vec::new();
    for record in records {
        let fields = json!({"executionID": record.id, "exitCode": record.exit_code.to_string()});
        for (level, bytes) in [("info", record.stdout), ("error", record.stderr)] {
            if !bytes.is_empty() {
                let message = String::from_utf8_lossy(&bytes).into_owned();
                logs.push(json!({"timestamp": record.finished_at, "level": level, "message": message, "line": message, "fields": fields}));
            }
        }
    }
    Ok(Json(json!({"logs": logs.clone(), "logEntries": logs})))
}

pub async fn sandbox_metrics(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sandbox_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sandbox = state.store.get_sandbox(&sandbox_id).await?;
    if sandbox.spec.tenant_id.as_deref() != Some(principal.tenant_id.as_str()) {
        return Err(ApiError(ClouisleError::not_found("sandbox not found")));
    }
    let stats = if sandbox.vmm_meta.vmm_id.is_some() {
        state
            .vmm
            .stats(&handle_for(&sandbox))
            .await
            .unwrap_or_default()
    } else {
        Default::default()
    };
    Ok(Json(
        json!({"timestamp": Utc::now(), "timestampUnix": Utc::now().timestamp(), "cpuCount": sandbox.spec.resources.vcpu, "cpuUsedPct": stats.vcpu_usage.unwrap_or(0.0), "memUsed": stats.mem_used_mb.unwrap_or(0) * 1024 * 1024, "memTotal": sandbox.spec.resources.memory_mb as u64 * 1024 * 1024, "memCache": 0, "diskUsed": 0, "diskTotal": sandbox.spec.resources.disk_mb as u64 * 1024 * 1024}),
    ))
}

pub async fn list_sandbox_metrics(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let ids = query
        .get("sandboxIDs")
        .or_else(|| query.get("sandboxIds"))
        .map(String::as_str)
        .unwrap_or_default();
    let mut sandboxes = HashMap::new();
    for id in ids.split(',').filter(|id| !id.is_empty()) {
        if let Ok(sandbox) = state.store.get_sandbox(id).await
            && sandbox.spec.tenant_id.as_deref() == Some(principal.tenant_id.as_str())
        {
            sandboxes.insert(id.to_string(), json!({"timestamp": Utc::now(), "timestampUnix": Utc::now().timestamp(), "cpuCount": sandbox.spec.resources.vcpu, "cpuUsedPct": 0.0, "memUsed": 0, "memTotal": sandbox.spec.resources.memory_mb as u64 * 1024 * 1024, "memCache": 0, "diskUsed": 0, "diskTotal": sandbox.spec.resources.disk_mb as u64 * 1024 * 1024}));
        }
    }
    Ok(Json(json!({"sandboxes": sandboxes})))
}

pub async fn create_snapshot(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sandbox_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let sandbox = state.store.get_sandbox(&sandbox_id).await?;
    if sandbox.spec.tenant_id.as_deref() != Some(principal.tenant_id.as_str()) {
        return Err(ApiError(ClouisleError::not_found("sandbox not found")));
    }
    let handle = handle_for(&sandbox);
    if !state.vmm.capabilities().snapshot || sandbox.vmm_meta.vmm_id.is_none() {
        return Err(ApiError(ClouisleError::invalid_state(
            "snapshot is unavailable for this runtime",
        )));
    }
    let snapshot_id = uuid::Uuid::now_v7().to_string();
    let root_dir = "/data/e2b/snapshots";
    tokio::fs::create_dir_all(root_dir).await.map_err(|error| {
        ApiError(ClouisleError::internal(format!(
            "create snapshot directory: {error}"
        )))
    })?;
    let root = format!("{root_dir}/{snapshot_id}");
    let paths = clouisle_vmm::SnapshotPaths {
        state_path: format!("{root}.state"),
        mem_path: format!("{root}.mem"),
    };
    // FC snapshot 要求 VM 处于 Paused 状态；完成后恢复运行。
    state.vmm.pause(&handle).await?;
    // 快照内固化 vsock UDS 路径；fork restore 时若源 socket 文件仍在会导致
    // EADDRINUSE。移除源 socket（其 inode 仍被源进程持有，源 agent 后续走 TCP）。
    if let Some(vsock) = handle.vsock_socket.as_deref() {
        let _ = tokio::fs::remove_file(vsock).await;
    }
    let snapshot_result = state
        .vmm
        .snapshot(&handle, clouisle_vmm::SnapshotKind::Full, &paths)
        .await;
    let _ = state.vmm.resume(&handle).await;
    snapshot_result?;
    let snapshot = state
        .e2b
        .create_snapshot(
            &principal.tenant_id,
            &sandbox_id,
            body.get("name").and_then(Value::as_str).map(str::to_string),
            paths.state_path,
            paths.mem_path,
        )
        .await
        .map_err(error_response)?;
    Ok(response(
        StatusCode::CREATED,
        json!({"snapshotID": snapshot.snapshot_id, "sandboxID": snapshot.sandbox_id, "names": snapshot.name.into_iter().collect::<Vec<_>>(), "createdAt": snapshot.created_at}),
    ))
}

pub async fn list_snapshots(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let mut values = state
        .e2b
        .list_snapshots(&principal.tenant_id)
        .await
        .into_iter()
        .map(|snapshot| json!({"snapshotID": snapshot.snapshot_id, "sandboxID": snapshot.sandbox_id, "names": snapshot.name.into_iter().collect::<Vec<_>>(), "createdAt": snapshot.created_at}))
        .collect::<Vec<_>>();
    if let Some(source) = query.get("sandboxID") {
        values.retain(|value| value.get("sandboxID").and_then(Value::as_str) == Some(source));
    }
    let (items, next) = page(&values, &query);
    Ok(with_next_token(
        response(StatusCode::OK, Value::Array(items)),
        next,
    ))
}

pub async fn fork_sandbox(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(sandbox_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let source = state.store.get_sandbox(&sandbox_id).await?;
    if source.spec.tenant_id.as_deref() != Some(principal.tenant_id.as_str()) {
        return Err(ApiError(ClouisleError::not_found("sandbox not found")));
    }
    if !state.vmm.capabilities().snapshot || source.vmm_meta.vmm_id.is_none() {
        return Err(ApiError(ClouisleError::invalid_state(
            "fork requires an active snapshot-capable runtime",
        )));
    }
    let snapshot_paths = if let Some(snapshot_id) = body.get("snapshotID").and_then(Value::as_str) {
        let snapshot = state
            .e2b
            .snapshot(&principal.tenant_id, snapshot_id)
            .await
            .map_err(error_response)?;
        clouisle_vmm::SnapshotPaths {
            state_path: snapshot.state_path,
            mem_path: snapshot.memory_path,
        }
    } else {
        let handle = handle_for(&source);
        let file_id = uuid::Uuid::now_v7().to_string();
        let root_dir = "/data/e2b/snapshots";
        tokio::fs::create_dir_all(root_dir).await.map_err(|error| {
            ApiError(ClouisleError::internal(format!(
                "create snapshot directory: {error}"
            )))
        })?;
        let paths = clouisle_vmm::SnapshotPaths {
            state_path: format!("{root_dir}/{file_id}.state"),
            mem_path: format!("{root_dir}/{file_id}.mem"),
        };
        // FC snapshot 要求 VM 处于 Paused 状态；完成后恢复运行。
        state.vmm.pause(&handle).await?;
        if let Some(vsock) = handle.vsock_socket.as_deref() {
            let _ = tokio::fs::remove_file(vsock).await;
        }
        let snapshot_result = state
            .vmm
            .snapshot(&handle, clouisle_vmm::SnapshotKind::Full, &paths)
            .await;
        let _ = state.vmm.resume(&handle).await;
        snapshot_result?;
        state
            .e2b
            .create_snapshot(
                &principal.tenant_id,
                &source.id,
                Some(format!("fork-{file_id}")),
                paths.state_path.clone(),
                paths.mem_path.clone(),
            )
            .await
            .map_err(error_response)?;
        paths
    };
    // fork 的 guest IP 固化在快照内（源沙盒网段）；fork 沙盒的 netns
    // 必须继承源网段，否则 restore 后 agent 网络不可达。
    // subnet 格式 "10.{a}.{b}.0/30" → 取第 2/3 段。
    let source_subnet = {
        let net = clouisle_net::netns::subnet(&source.id);
        let mut parts = net.split('.');
        parts.next();
        let a = parts.next().and_then(|v| v.parse::<u16>().ok());
        let b = parts.next().and_then(|v| v.parse::<u16>().ok());
        a.zip(b)
    };
    let count = body
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 100);
    let timeout = body.get("timeout").and_then(Value::as_u64).unwrap_or(15);
    let mut result = Vec::new();
    for _ in 0..count {
        let mut spec = source.spec.clone();
        spec.ttl_secs = Some(timeout);
        let id = uuid::Uuid::now_v7().to_string();
        let mut fork = clouisle_core::Sandbox::new(id.clone(), spec);
        fork.status = SandboxStatus::Starting;
        let reservation = if state.manage_resources {
            Some(state.pool.admit(&fork.spec).await?)
        } else {
            None
        };
        match state.store.create_sandbox(&fork).await {
            Ok(()) => {
                let response_sandbox = fork.clone();
                let state_for_task = state.clone();
                let paths = snapshot_paths.clone();
                tokio::spawn(async move {
                    if let Err(error) = crate::handlers::sandbox::run_provision_from_snapshot(
                        state_for_task,
                        fork,
                        reservation,
                        paths,
                        source_subnet,
                    )
                    .await
                    {
                        tracing::error!(sandbox_id = %id, %error, "fork restore failed");
                    }
                });
                result.push(json!({"sandbox": crate::e2b::from_sandbox(&response_sandbox)}));
            }
            Err(error) => {
                result.push(json!({"error": {"code": "INTERNAL", "message": error.to_string()}}))
            }
        }
    }
    Ok(Json(result))
}

pub async fn admin_kill_team(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(team_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&headers)?;
    let mut killed = 0;
    let mut failed = 0;
    for sandbox in state.store.list_sandboxes(None).await? {
        if sandbox.spec.tenant_id.as_deref() == Some(team_id.as_str()) {
            if let Some(slot) = state.warm_slots.lock().await.remove(&sandbox.id) {
                let _ = state.warm_pool.discard(slot).await;
            } else if sandbox.vmm_meta.vmm_id.is_some() {
                let _ = state
                    .vmm
                    .stop(&handle_for(&sandbox), clouisle_vmm::StopMode::Force)
                    .await;
            }
            #[cfg(target_os = "linux")]
            if state.manage_network {
                let _ = state
                    .firewall
                    .teardown_sandbox_network(&sandbox.id, sandbox.vmm_meta.inherited_subnet())
                    .await;
            }
            match state.store.delete_sandbox(&sandbox.id).await {
                Ok(()) => {
                    state.reservations.lock().await.remove(&sandbox.id);
                    killed += 1;
                }
                Err(_) => failed += 1,
            }
        }
    }
    Ok(Json(json!({"killedCount": killed, "failedCount": failed})))
}

pub async fn admin_create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(team_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    require_admin(&headers)?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("admin-key");
    let created = state
        .e2b
        .create_api_key(&team_id, name, ScopeRecord::Full)
        .await
        .map_err(error_response)?;
    if let (Some(id), Some(raw)) = (
        created.get("id").and_then(Value::as_str),
        created.get("key").and_then(Value::as_str),
    ) {
        state
            .auth
            .register_with_id(id, raw, &team_id, Scope::Full)
            .await;
    }
    Ok(response(StatusCode::CREATED, created))
}

pub async fn admin_delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((team_id, key_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    require_admin(&headers)?;
    state
        .e2b
        .delete_api_key(&team_id, &key_id)
        .await
        .map_err(error_response)?;
    state.auth.revoke_id(&key_id).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn admin_cancel_builds(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(team_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&headers)?;
    let (cancelled, failed) = state
        .e2b
        .cancel_builds(&team_id)
        .await
        .map_err(error_response)?;
    Ok(Json(
        json!({"cancelledCount": cancelled, "failedCount": failed}),
    ))
}

pub async fn list_nodes(
    State(state): State<AppState>,
    Extension(_principal): Extension<Principal>,
) -> Result<Json<Vec<Value>>, ApiError> {
    Ok(Json(
        state
            .store
            .list_ready_nodes(Utc::now().timestamp_millis())
            .await?
            .into_iter()
            .map(|node| json!({"nodeID": node.info.node_id, "status": node.status, "endpoint": node.endpoint, "info": node.info}))
            .collect(),
    ))
}

pub async fn get_node(
    State(state): State<AppState>,
    Extension(_principal): Extension<Principal>,
    Path(node_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let node = state
        .store
        .list_ready_nodes(Utc::now().timestamp_millis())
        .await?
        .into_iter()
        .find(|node| node.info.node_id == node_id)
        .ok_or_else(|| ApiError(ClouisleError::not_found("node not found")))?;
    Ok(Json(
        json!({"nodeID": node.info.node_id, "status": node.status, "endpoint": node.endpoint, "info": node.info}),
    ))
}

pub async fn upload_template_file(
    State(_state): State<AppState>,
    Extension(_principal): Extension<Principal>,
    Path((_template_id, _hash)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    Ok(response(
        StatusCode::OK,
        json!({"url": "local://template-build-upload"}),
    ))
}

pub async fn update_template(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(template_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let template = state
        .e2b
        .update_template(&principal.tenant_id, &template_id, &body)
        .await
        .map_err(error_response)?;
    Ok(Json(template_json(&template, None)))
}

pub async fn template_tag_response(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path((_template_id, _tag)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let _ = state
        .e2b
        .ensure_team(&principal.tenant_id, None)
        .await
        .map_err(error_response)?;
    Ok(Json(json!({"templateID": "", "public": false})))
}

pub async fn template_build_upload(
    State(_state): State<AppState>,
    Extension(_principal): Extension<Principal>,
    Path((_template_id, _hash)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<StatusCode, ApiError> {
    if body.len() > 1024 * 1024 * 1024 {
        return Err(ApiError(ClouisleError::validation(
            "template upload too large",
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn volume_content_health() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn volume_content_init() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn volume_content_metrics() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

pub async fn health_204() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub async fn unsupported_cloud_endpoint() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
