//! Persistent E2B cloud-control-plane state.
//!
//! This module intentionally keeps the wire resources independent from the
//! Firecracker sandbox model. `team_id` is the authorization boundary for
//! every record; the existing tenant field is only the runtime compatibility
//! bridge.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::auth::{Principal, Scope};

pub const E2B_CONTRACT_COMMIT: &str = "cab27aa6fabd53f759189328c4f74df2df1550ad";

#[derive(Debug, thiserror::Error)]
pub enum ControlPlaneError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("validation: {0}")]
    Validation(String),
    #[error("persistence: {0}")]
    Persistence(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRecord {
    pub team_id: String,
    pub name: String,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMemberRecord {
    pub user_id: String,
    pub email: Option<String>,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: String,
    pub team_id: String,
    pub name: String,
    pub salt: String,
    pub digest: String,
    pub mask: String,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub scope: ScopeRecord,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeRecord {
    Read,
    Full,
    Admin,
}

impl ScopeRecord {
    pub(crate) fn into_scope(self) -> Scope {
        match self {
            Self::Read => Scope::Read,
            Self::Full | Self::Admin => Scope::Full,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessTokenRecord {
    pub id: String,
    pub team_id: String,
    pub name: String,
    pub salt: String,
    pub digest: String,
    pub mask: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeRecord {
    pub volume_id: String,
    pub team_id: String,
    pub name: String,
    pub token_salt: String,
    pub token_digest: String,
    pub token_mask: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub files: HashMap<String, VolumeFileRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeFileRecord {
    pub content: Vec<u8>,
    pub mode: u32,
    pub modified_at: DateTime<Utc>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateRecord {
    pub template_id: String,
    pub team_id: String,
    pub names: Vec<String>,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub public: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub latest_build_id: Option<String>,
    pub build_ids: Vec<String>,
    pub image_reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRecord {
    pub build_id: String,
    pub template_id: String,
    pub team_id: String,
    pub status: String,
    pub image_reference: Option<String>,
    pub request: serde_json::Value,
    pub logs: Vec<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub snapshot_id: String,
    pub team_id: String,
    pub sandbox_id: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub state_path: String,
    pub memory_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatcherRecord {
    pub watcher_id: String,
    pub team_id: String,
    pub sandbox_id: String,
    pub path: String,
    pub recursive: bool,
    pub include_entry: bool,
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub id: String,
    pub team_id: String,
    pub action: String,
    pub resource_type: String,
    pub resource_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub fields: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ControlPlaneData {
    #[serde(default)]
    teams: HashMap<String, TeamRecord>,
    #[serde(default)]
    members: HashMap<String, Vec<TeamMemberRecord>>,
    #[serde(default)]
    api_keys: HashMap<String, ApiKeyRecord>,
    #[serde(default)]
    access_tokens: HashMap<String, AccessTokenRecord>,
    #[serde(default)]
    volumes: HashMap<String, VolumeRecord>,
    #[serde(default)]
    templates: HashMap<String, TemplateRecord>,
    #[serde(default)]
    builds: HashMap<String, BuildRecord>,
    #[serde(default)]
    snapshots: HashMap<String, SnapshotRecord>,
    #[serde(default)]
    watchers: HashMap<String, WatcherRecord>,
    #[serde(default)]
    audit: Vec<AuditRecord>,
}

#[derive(Debug, Clone)]
pub struct E2bControlPlane {
    data: Arc<RwLock<ControlPlaneData>>,
    persistence_path: Option<PathBuf>,
    persist_lock: Arc<Mutex<()>>,
}

impl Default for E2bControlPlane {
    fn default() -> Self {
        Self::new()
    }
}

impl E2bControlPlane {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(ControlPlaneData::default())),
            persistence_path: None,
            persist_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, ControlPlaneError> {
        let path = path.into();
        let data = if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| ControlPlaneError::Persistence(e.to_string()))?;
            serde_json::from_slice(&bytes)
                .map_err(|e| ControlPlaneError::Persistence(e.to_string()))?
        } else {
            ControlPlaneData::default()
        };
        let cp = Self {
            data: Arc::new(RwLock::new(data)),
            persistence_path: Some(path),
            persist_lock: Arc::new(Mutex::new(())),
        };
        cp.ensure_team("dev", Some("Development")).await?;
        Ok(cp)
    }

    pub fn contract_commit(&self) -> &'static str {
        E2B_CONTRACT_COMMIT
    }

    async fn persist(&self) -> Result<(), ControlPlaneError> {
        let Some(path) = &self.persistence_path else {
            return Ok(());
        };
        let _guard = self.persist_lock.lock().await;
        let data = self.data.read().await;
        let bytes = serde_json::to_vec_pretty(&*data)
            .map_err(|e| ControlPlaneError::Persistence(e.to_string()))?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ControlPlaneError::Persistence(e.to_string()))?;
        }
        tokio::fs::write(&tmp, bytes)
            .await
            .map_err(|e| ControlPlaneError::Persistence(e.to_string()))?;
        tokio::fs::rename(&tmp, path)
            .await
            .map_err(|e| ControlPlaneError::Persistence(e.to_string()))?;
        Ok(())
    }

    pub async fn ensure_team(
        &self,
        team_id: &str,
        name: Option<&str>,
    ) -> Result<TeamRecord, ControlPlaneError> {
        if team_id.trim().is_empty() {
            return Err(ControlPlaneError::Validation("team id is empty".into()));
        }
        let mut data = self.data.write().await;
        if let Some(team) = data.teams.get(team_id).cloned() {
            drop(data);
            self.persist().await?;
            return Ok(team);
        }
        let team = TeamRecord {
            team_id: team_id.to_string(),
            name: name.unwrap_or(team_id).to_string(),
            is_default: data.teams.is_empty(),
            created_at: Utc::now(),
        };
        data.teams.insert(team_id.to_string(), team.clone());
        drop(data);
        self.persist().await?;
        Ok(team)
    }

    pub async fn team(&self, team_id: &str) -> Result<TeamRecord, ControlPlaneError> {
        self.data
            .read()
            .await
            .teams
            .get(team_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::NotFound(format!("team {team_id}")))
    }

    pub async fn list_teams(&self, team_id: &str) -> Result<Vec<TeamRecord>, ControlPlaneError> {
        let team = self.ensure_team(team_id, None).await?;
        Ok(vec![team])
    }

    pub async fn add_member(
        &self,
        team_id: &str,
        member: TeamMemberRecord,
    ) -> Result<(), ControlPlaneError> {
        self.ensure_team(team_id, None).await?;
        let mut data = self.data.write().await;
        let members = data.members.entry(team_id.to_string()).or_default();
        if members.iter().any(|m| m.user_id == member.user_id) {
            return Err(ControlPlaneError::Conflict(format!(
                "member {} already exists",
                member.user_id
            )));
        }
        members.push(member);
        drop(data);
        self.persist().await
    }

    pub async fn team_members(
        &self,
        team_id: &str,
    ) -> Result<Vec<TeamMemberRecord>, ControlPlaneError> {
        self.ensure_team(team_id, None).await?;
        Ok(self
            .data
            .read()
            .await
            .members
            .get(team_id)
            .cloned()
            .unwrap_or_default())
    }

    pub async fn authenticate(&self, credential: &str) -> Option<Principal> {
        let mut data = self.data.write().await;
        for key in data.api_keys.values_mut() {
            if verify_digest(&key.salt, credential, &key.digest) {
                key.last_used = Some(Utc::now());
                let principal = Principal {
                    tenant_id: key.team_id.clone(),
                    scope: key.scope.into_scope(),
                    volume_id: None,
                };
                drop(data);
                let _ = self.persist().await;
                return Some(principal);
            }
        }
        for token in data.access_tokens.values() {
            if verify_digest(&token.salt, credential, &token.digest) {
                return Some(Principal {
                    tenant_id: token.team_id.clone(),
                    scope: Scope::Full,
                    volume_id: None,
                });
            }
        }
        for volume in data.volumes.values() {
            if verify_digest(&volume.token_salt, credential, &volume.token_digest) {
                return Some(Principal {
                    tenant_id: volume.team_id.clone(),
                    scope: Scope::Full,
                    volume_id: Some(volume.volume_id.clone()),
                });
            }
        }
        None
    }

    pub async fn create_api_key(
        &self,
        team_id: &str,
        name: &str,
        scope: ScopeRecord,
    ) -> Result<serde_json::Value, ControlPlaneError> {
        validate_name(name, "API key")?;
        self.ensure_team(team_id, None).await?;
        let id = Uuid::now_v7().to_string();
        let raw = format!("e2b_{}", Uuid::now_v7().simple());
        let salt = Uuid::now_v7().to_string();
        let mask = mask_secret(&raw);
        let record = ApiKeyRecord {
            id: id.clone(),
            team_id: team_id.to_string(),
            name: name.to_string(),
            salt: salt.clone(),
            digest: digest_secret(&salt, &raw),
            mask: mask.clone(),
            created_at: Utc::now(),
            last_used: None,
            scope,
        };
        self.data.write().await.api_keys.insert(id.clone(), record);
        self.audit(
            team_id,
            "api_key.create",
            "api_key",
            Some(&id),
            serde_json::json!({"name": name}),
        )
        .await?;
        self.persist().await?;
        Ok(serde_json::json!({
            "id": id,
            "key": raw,
            "mask": {"prefix": &mask[..mask.len().min(8)], "suffix": mask_secret_suffix(&mask)},
            "name": name,
            "createdAt": Utc::now(),
        }))
    }

    pub async fn list_api_keys(
        &self,
        team_id: &str,
    ) -> Result<Vec<serde_json::Value>, ControlPlaneError> {
        self.ensure_team(team_id, None).await?;
        Ok(self
            .data
            .read()
            .await
            .api_keys
            .values()
            .filter(|key| key.team_id == team_id)
            .map(api_key_view)
            .collect())
    }

    pub async fn update_api_key(
        &self,
        team_id: &str,
        id: &str,
        name: &str,
    ) -> Result<(), ControlPlaneError> {
        validate_name(name, "API key")?;
        let mut data = self.data.write().await;
        let key = data
            .api_keys
            .get_mut(id)
            .filter(|key| key.team_id == team_id)
            .ok_or_else(|| ControlPlaneError::NotFound(format!("API key {id}")))?;
        key.name = name.to_string();
        drop(data);
        self.audit(
            team_id,
            "api_key.update",
            "api_key",
            Some(id),
            serde_json::json!({"name": name}),
        )
        .await?;
        self.persist().await
    }

    pub async fn delete_api_key(&self, team_id: &str, id: &str) -> Result<(), ControlPlaneError> {
        let removed = self
            .data
            .write()
            .await
            .api_keys
            .remove(id)
            .filter(|key| key.team_id == team_id);
        if removed.is_none() {
            return Err(ControlPlaneError::NotFound(format!("API key {id}")));
        }
        self.audit(
            team_id,
            "api_key.delete",
            "api_key",
            Some(id),
            serde_json::json!({}),
        )
        .await?;
        self.persist().await
    }

    pub async fn create_access_token(
        &self,
        team_id: &str,
        name: &str,
    ) -> Result<serde_json::Value, ControlPlaneError> {
        validate_name(name, "access token")?;
        self.ensure_team(team_id, None).await?;
        let id = Uuid::now_v7().to_string();
        let raw = format!("e2b_access_{}", Uuid::now_v7().simple());
        let salt = Uuid::now_v7().to_string();
        let mask = mask_secret(&raw);
        self.data.write().await.access_tokens.insert(
            id.clone(),
            AccessTokenRecord {
                id: id.clone(),
                team_id: team_id.to_string(),
                name: name.to_string(),
                salt: salt.clone(),
                digest: digest_secret(&salt, &raw),
                mask: mask.clone(),
                created_at: Utc::now(),
            },
        );
        self.audit(
            team_id,
            "access_token.create",
            "access_token",
            Some(&id),
            serde_json::json!({"name": name}),
        )
        .await?;
        self.persist().await?;
        Ok(serde_json::json!({
            "id": id,
            "name": name,
            "token": raw,
            "mask": {"prefix": &mask[..mask.len().min(8)], "suffix": mask_secret_suffix(&mask)},
            "createdAt": Utc::now(),
        }))
    }

    pub async fn delete_access_token(
        &self,
        team_id: &str,
        id: &str,
    ) -> Result<(), ControlPlaneError> {
        let removed = self
            .data
            .write()
            .await
            .access_tokens
            .remove(id)
            .filter(|token| token.team_id == team_id);
        if removed.is_none() {
            return Err(ControlPlaneError::NotFound(format!("access token {id}")));
        }
        self.audit(
            team_id,
            "access_token.delete",
            "access_token",
            Some(id),
            serde_json::json!({}),
        )
        .await?;
        self.persist().await
    }

    pub async fn create_volume(
        &self,
        team_id: &str,
        name: &str,
    ) -> Result<serde_json::Value, ControlPlaneError> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(ControlPlaneError::Validation(
                "volume name must match ^[a-zA-Z0-9_-]+$".into(),
            ));
        }
        self.ensure_team(team_id, None).await?;
        let mut data = self.data.write().await;
        if data
            .volumes
            .values()
            .any(|volume| volume.team_id == team_id && volume.name == name)
        {
            return Err(ControlPlaneError::Conflict(format!("volume {name} exists")));
        }
        let volume_id = Uuid::now_v7().to_string();
        let token = format!("e2b_volume_{}", Uuid::now_v7().simple());
        let salt = Uuid::now_v7().to_string();
        let mask = mask_secret(&token);
        data.volumes.insert(
            volume_id.clone(),
            VolumeRecord {
                volume_id: volume_id.clone(),
                team_id: team_id.to_string(),
                name: name.to_string(),
                token_salt: salt.clone(),
                token_digest: digest_secret(&salt, &token),
                token_mask: mask,
                created_at: Utc::now(),
                files: HashMap::new(),
            },
        );
        drop(data);
        self.audit(
            team_id,
            "volume.create",
            "volume",
            Some(&volume_id),
            serde_json::json!({"name": name}),
        )
        .await?;
        self.persist().await?;
        Ok(serde_json::json!({
            "volumeID": volume_id,
            "name": name,
            "token": token,
        }))
    }

    pub async fn list_volumes(
        &self,
        team_id: &str,
    ) -> Result<Vec<serde_json::Value>, ControlPlaneError> {
        self.ensure_team(team_id, None).await?;
        Ok(self
            .data
            .read()
            .await
            .volumes
            .values()
            .filter(|volume| volume.team_id == team_id)
            .map(|volume| serde_json::json!({"volumeID": volume.volume_id, "name": volume.name}))
            .collect())
    }

    pub async fn volume_by_name(
        &self,
        team_id: &str,
        name: &str,
    ) -> Result<VolumeRecord, ControlPlaneError> {
        self.data
            .read()
            .await
            .volumes
            .values()
            .find(|volume| volume.team_id == team_id && volume.name == name)
            .cloned()
            .ok_or_else(|| ControlPlaneError::NotFound(format!("volume {name}")))
    }

    pub async fn get_volume(
        &self,
        team_id: &str,
        volume_id: &str,
    ) -> Result<serde_json::Value, ControlPlaneError> {
        let volume = self
            .data
            .read()
            .await
            .volumes
            .get(volume_id)
            .filter(|volume| volume.team_id == team_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::NotFound(format!("volume {volume_id}")))?;
        Ok(
            serde_json::json!({"volumeID": volume.volume_id, "name": volume.name, "token": volume.token_mask}),
        )
    }

    pub async fn delete_volume(
        &self,
        team_id: &str,
        volume_id: &str,
    ) -> Result<(), ControlPlaneError> {
        let removed = self
            .data
            .write()
            .await
            .volumes
            .remove(volume_id)
            .filter(|volume| volume.team_id == team_id);
        if removed.is_none() {
            return Err(ControlPlaneError::NotFound(format!("volume {volume_id}")));
        }
        self.audit(
            team_id,
            "volume.delete",
            "volume",
            Some(volume_id),
            serde_json::json!({}),
        )
        .await?;
        self.persist().await
    }

    pub async fn volume_by_credential(&self, credential: &str) -> Option<VolumeRecord> {
        self.data
            .read()
            .await
            .volumes
            .values()
            .find(|volume| verify_digest(&volume.token_salt, credential, &volume.token_digest))
            .cloned()
    }

    pub async fn put_volume_file(
        &self,
        team_id: &str,
        volume_id: &str,
        path: &str,
        content: Vec<u8>,
        metadata: HashMap<String, String>,
    ) -> Result<(), ControlPlaneError> {
        let path = normalize_volume_path(path)?;
        let mut data = self.data.write().await;
        let volume = data
            .volumes
            .get_mut(volume_id)
            .filter(|volume| volume.team_id == team_id)
            .ok_or_else(|| ControlPlaneError::NotFound(format!("volume {volume_id}")))?;
        volume.files.insert(
            path,
            VolumeFileRecord {
                content,
                mode: 0o644,
                modified_at: Utc::now(),
                metadata,
            },
        );
        drop(data);
        self.persist().await
    }

    pub async fn get_volume_file(
        &self,
        team_id: &str,
        volume_id: &str,
        path: &str,
    ) -> Result<VolumeFileRecord, ControlPlaneError> {
        let path = normalize_volume_path(path)?;
        self.data
            .read()
            .await
            .volumes
            .get(volume_id)
            .filter(|volume| volume.team_id == team_id)
            .and_then(|volume| volume.files.get(&path))
            .cloned()
            .ok_or_else(|| ControlPlaneError::NotFound(format!("volume file {path}")))
    }

    pub async fn list_volume_files(
        &self,
        team_id: &str,
        volume_id: &str,
        path: &str,
    ) -> Result<Vec<String>, ControlPlaneError> {
        let path = normalize_volume_path(path)?;
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let files = self
            .data
            .read()
            .await
            .volumes
            .get(volume_id)
            .filter(|volume| volume.team_id == team_id)
            .ok_or_else(|| ControlPlaneError::NotFound(format!("volume {volume_id}")))?
            .files
            .keys()
            .filter_map(|file| {
                let remainder = file.strip_prefix(&prefix)?;
                let first = remainder
                    .split('/')
                    .next()
                    .filter(|value| !value.is_empty())?;
                let entry = if path == "/" {
                    format!("/{first}")
                } else {
                    format!("{}/{first}", path.trim_end_matches('/'))
                };
                Some(entry)
            })
            .fold(Vec::new(), |mut entries, entry| {
                if !entries.iter().any(|existing| existing == &entry) {
                    entries.push(entry);
                }
                entries
            });
        Ok(files)
    }

    pub async fn create_template(
        &self,
        team_id: &str,
        name: Option<&str>,
        alias: Option<&str>,
        public: bool,
        image_reference: Option<String>,
    ) -> Result<TemplateRecord, ControlPlaneError> {
        let template_id = name
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("template-{}", Uuid::now_v7()));
        validate_template_name(&template_id)?;
        self.ensure_team(team_id, None).await?;
        let now = Utc::now();
        let mut data = self.data.write().await;
        if data
            .templates
            .values()
            .any(|template| template.team_id == team_id && template.template_id == template_id)
        {
            return Err(ControlPlaneError::Conflict(format!(
                "template {template_id} exists"
            )));
        }
        let template = TemplateRecord {
            template_id: template_id.clone(),
            team_id: team_id.to_string(),
            names: vec![template_id.clone()],
            aliases: alias.into_iter().map(str::to_string).collect(),
            tags: Vec::new(),
            public,
            created_at: now,
            updated_at: now,
            latest_build_id: None,
            build_ids: Vec::new(),
            image_reference,
        };
        data.templates.insert(template_id, template.clone());
        drop(data);
        self.persist().await?;
        Ok(template)
    }

    pub async fn list_templates(&self, team_id: &str) -> Vec<TemplateRecord> {
        self.data
            .read()
            .await
            .templates
            .values()
            .filter(|template| template.public || template.team_id == team_id)
            .cloned()
            .collect()
    }

    pub async fn image_templates(&self) -> Vec<String> {
        self.data
            .read()
            .await
            .templates
            .values()
            .filter_map(|template| template.image_reference.clone())
            .collect()
    }

    pub async fn template(
        &self,
        team_id: &str,
        template_id: &str,
    ) -> Result<TemplateRecord, ControlPlaneError> {
        self.data
            .read()
            .await
            .templates
            .get(template_id)
            .filter(|template| template.public || template.team_id == team_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::NotFound(format!("template {template_id}")))
    }

    pub async fn update_template(
        &self,
        team_id: &str,
        template_id: &str,
        request: &serde_json::Value,
    ) -> Result<TemplateRecord, ControlPlaneError> {
        let mut data = self.data.write().await;
        let template = data
            .templates
            .get_mut(template_id)
            .filter(|template| template.team_id == team_id)
            .ok_or_else(|| ControlPlaneError::NotFound(format!("template {template_id}")))?;
        if let Some(public) = request.get("public").and_then(serde_json::Value::as_bool) {
            template.public = public;
        }
        if let Some(alias) = request.get("alias").and_then(serde_json::Value::as_str) {
            validate_template_name(alias)?;
            if !template.aliases.iter().any(|value| value == alias) {
                template.aliases.push(alias.to_string());
            }
        }
        if let Some(tags) = request.get("tags").and_then(serde_json::Value::as_array) {
            template.tags = tags
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect();
        }
        template.updated_at = Utc::now();
        let result = template.clone();
        drop(data);
        self.persist().await?;
        Ok(result)
    }

    pub async fn create_build(
        &self,
        team_id: &str,
        template_id: &str,
        request: serde_json::Value,
        image_reference: Option<String>,
    ) -> Result<BuildRecord, ControlPlaneError> {
        let template = self.template(team_id, template_id).await?;
        let now = Utc::now();
        let build = BuildRecord {
            build_id: Uuid::now_v7().to_string(),
            template_id: template.template_id,
            team_id: team_id.to_string(),
            status: "queued".into(),
            image_reference,
            request,
            logs: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        let mut data = self.data.write().await;
        data.builds.insert(build.build_id.clone(), build.clone());
        if let Some(template) = data.templates.get_mut(template_id) {
            template.build_ids.push(build.build_id.clone());
            template.updated_at = now;
        }
        drop(data);
        self.persist().await?;
        Ok(build)
    }

    pub async fn update_build(
        &self,
        team_id: &str,
        build_id: &str,
        status: &str,
        log: Option<serde_json::Value>,
    ) -> Result<BuildRecord, ControlPlaneError> {
        let mut data = self.data.write().await;
        let build = data
            .builds
            .get_mut(build_id)
            .filter(|build| build.team_id == team_id)
            .ok_or_else(|| ControlPlaneError::NotFound(format!("build {build_id}")))?;
        build.status = status.to_string();
        build.updated_at = Utc::now();
        if let Some(log) = log {
            build.logs.push(log);
        }
        let build = build.clone();
        if status == "succeeded"
            && let Some(template) = data.templates.get_mut(&build.template_id)
        {
            template.latest_build_id = Some(build.build_id.clone());
            if build.image_reference.is_some() {
                template.image_reference = build.image_reference.clone();
            }
        }
        drop(data);
        self.persist().await?;
        Ok(build)
    }

    pub async fn build(
        &self,
        team_id: &str,
        build_id: &str,
    ) -> Result<BuildRecord, ControlPlaneError> {
        self.data
            .read()
            .await
            .builds
            .get(build_id)
            .filter(|build| build.team_id == team_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::NotFound(format!("build {build_id}")))
    }

    pub async fn cancel_builds(&self, team_id: &str) -> Result<(usize, usize), ControlPlaneError> {
        let mut data = self.data.write().await;
        let mut cancelled = 0;
        let mut failed = 0;
        for build in data
            .builds
            .values_mut()
            .filter(|build| build.team_id == team_id)
        {
            if matches!(build.status.as_str(), "queued" | "running") {
                build.status = "cancelled".into();
                build.updated_at = Utc::now();
                build
                    .logs
                    .push(json!({"message": "build cancelled by administrator"}));
                cancelled += 1;
            } else if build.status == "failed" {
                failed += 1;
            }
        }
        drop(data);
        self.persist().await?;
        Ok((cancelled, failed))
    }

    pub async fn add_template_tag(
        &self,
        team_id: &str,
        template_id: &str,
        tag: &str,
    ) -> Result<TemplateRecord, ControlPlaneError> {
        validate_template_name(tag)?;
        let mut data = self.data.write().await;
        let template = data
            .templates
            .get_mut(template_id)
            .filter(|template| template.team_id == team_id)
            .ok_or_else(|| ControlPlaneError::NotFound(format!("template {template_id}")))?;
        if !template.tags.iter().any(|existing| existing == tag) {
            template.tags.push(tag.to_string());
        }
        template.updated_at = Utc::now();
        let template = template.clone();
        drop(data);
        self.persist().await?;
        Ok(template)
    }

    pub async fn create_snapshot(
        &self,
        team_id: &str,
        sandbox_id: &str,
        name: Option<String>,
        state_path: String,
        memory_path: String,
    ) -> Result<SnapshotRecord, ControlPlaneError> {
        let snapshot = SnapshotRecord {
            snapshot_id: Uuid::now_v7().to_string(),
            team_id: team_id.to_string(),
            sandbox_id: sandbox_id.to_string(),
            name,
            created_at: Utc::now(),
            state_path,
            memory_path,
        };
        self.data
            .write()
            .await
            .snapshots
            .insert(snapshot.snapshot_id.clone(), snapshot.clone());
        self.persist().await?;
        Ok(snapshot)
    }

    pub async fn list_snapshots(&self, team_id: &str) -> Vec<SnapshotRecord> {
        self.data
            .read()
            .await
            .snapshots
            .values()
            .filter(|snapshot| snapshot.team_id == team_id)
            .cloned()
            .collect()
    }

    pub async fn snapshot(
        &self,
        team_id: &str,
        snapshot_id: &str,
    ) -> Result<SnapshotRecord, ControlPlaneError> {
        self.data
            .read()
            .await
            .snapshots
            .get(snapshot_id)
            .filter(|snapshot| snapshot.team_id == team_id)
            .cloned()
            .ok_or_else(|| ControlPlaneError::NotFound(format!("snapshot {snapshot_id}")))
    }

    pub async fn create_watcher(
        &self,
        team_id: &str,
        sandbox_id: &str,
        path: &str,
        recursive: bool,
        include_entry: bool,
    ) -> Result<WatcherRecord, ControlPlaneError> {
        let path = normalize_volume_path(path)?;
        let watcher = WatcherRecord {
            watcher_id: Uuid::now_v7().to_string(),
            team_id: team_id.to_string(),
            sandbox_id: sandbox_id.to_string(),
            path,
            recursive,
            include_entry,
            events: Vec::new(),
        };
        self.data
            .write()
            .await
            .watchers
            .insert(watcher.watcher_id.clone(), watcher.clone());
        self.persist().await?;
        Ok(watcher)
    }

    pub async fn watcher_events(
        &self,
        team_id: &str,
        watcher_id: &str,
    ) -> Result<Vec<serde_json::Value>, ControlPlaneError> {
        self.data
            .read()
            .await
            .watchers
            .get(watcher_id)
            .filter(|watcher| watcher.team_id == team_id)
            .map(|watcher| watcher.events.clone())
            .ok_or_else(|| ControlPlaneError::NotFound(format!("watcher {watcher_id}")))
    }

    pub async fn remove_watcher(
        &self,
        team_id: &str,
        watcher_id: &str,
    ) -> Result<(), ControlPlaneError> {
        let removed = self
            .data
            .write()
            .await
            .watchers
            .remove(watcher_id)
            .filter(|watcher| watcher.team_id == team_id);
        if removed.is_none() {
            return Err(ControlPlaneError::NotFound(format!("watcher {watcher_id}")));
        }
        self.persist().await
    }

    pub async fn audit(
        &self,
        team_id: &str,
        action: &str,
        resource_type: &str,
        resource_id: Option<&str>,
        fields: serde_json::Value,
    ) -> Result<(), ControlPlaneError> {
        self.data.write().await.audit.push(AuditRecord {
            id: Uuid::now_v7().to_string(),
            team_id: team_id.to_string(),
            action: action.to_string(),
            resource_type: resource_type.to_string(),
            resource_id: resource_id.map(str::to_string),
            created_at: Utc::now(),
            fields,
        });
        self.persist().await
    }
}

fn validate_name(name: &str, kind: &str) -> Result<(), ControlPlaneError> {
    if name.trim().is_empty() || name.len() > 128 {
        return Err(ControlPlaneError::Validation(format!(
            "{kind} name is invalid"
        )));
    }
    Ok(())
}

fn validate_template_name(name: &str) -> Result<(), ControlPlaneError> {
    if name.trim().is_empty() || name.len() > 128 || name.contains(['/', '\\']) {
        return Err(ControlPlaneError::Validation(
            "template name is invalid".into(),
        ));
    }
    Ok(())
}

fn normalize_volume_path(path: &str) -> Result<String, ControlPlaneError> {
    if path.contains('\\') || path.contains('\0') {
        return Err(ControlPlaneError::Validation(
            "volume path contains an invalid character".into(),
        ));
    }
    if path.is_empty() {
        return Ok("/".into());
    }
    let mut output = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(ControlPlaneError::Validation(
                    "volume path escapes root".into(),
                ));
            }
            part if part.contains('\0') => {
                return Err(ControlPlaneError::Validation(
                    "volume path contains NUL".into(),
                ));
            }
            part => output.push(part),
        }
    }
    Ok(format!("/{}", output.join("/")))
}

fn digest_secret(salt: &str, value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update([0]);
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn verify_digest(salt: &str, value: &str, expected: &str) -> bool {
    digest_secret(salt, value) == expected
}

fn mask_secret(value: &str) -> String {
    if value.len() <= 8 {
        return "********".into();
    }
    format!(
        "{}{}",
        &value[..4],
        "*".repeat(value.len().saturating_sub(8))
    ) + &value[value.len() - 4..]
}

fn mask_secret_suffix(mask: &str) -> String {
    mask.chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn api_key_view(key: &ApiKeyRecord) -> serde_json::Value {
    serde_json::json!({
        "id": key.id,
        "name": key.name,
        "mask": {"prefix": &key.mask[..key.mask.len().min(4)], "suffix": mask_secret_suffix(&key.mask)},
        "createdAt": key.created_at,
        "lastUsed": key.last_used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn api_keys_are_one_way_and_team_scoped() {
        let cp = E2bControlPlane::new();
        let created = cp
            .create_api_key("team-a", "test", ScopeRecord::Full)
            .await
            .unwrap();
        let raw = created["key"].as_str().unwrap().to_string();
        assert!(cp.authenticate(&raw).await.is_some());
        assert!(cp.authenticate("not-a-key").await.is_none());
        assert_eq!(cp.list_api_keys("team-b").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn volume_paths_reject_escape() {
        let cp = E2bControlPlane::new();
        let volume = cp.create_volume("team-a", "data").await.unwrap();
        let id = volume["volumeID"].as_str().unwrap();
        let result = cp
            .put_volume_file("team-a", id, "/../escape", vec![], HashMap::new())
            .await;
        assert!(matches!(result, Err(ControlPlaneError::Validation(_))));
    }

    #[tokio::test]
    async fn credentials_and_volumes_survive_control_plane_restart() {
        let path = std::env::temp_dir().join(format!("clouisle-e2b-{}.json", Uuid::now_v7()));
        let cp = E2bControlPlane::open(&path).await.unwrap();
        let key = cp
            .create_api_key("team-a", "build", ScopeRecord::Read)
            .await
            .unwrap();
        let raw_key = key["key"].as_str().unwrap().to_string();
        let volume = cp.create_volume("team-a", "data").await.unwrap();
        let raw_volume_token = volume["token"].as_str().unwrap().to_string();
        let volume_id = volume["volumeID"].as_str().unwrap();
        cp.put_volume_file(
            "team-a",
            volume_id,
            "/state.txt",
            b"persisted".to_vec(),
            HashMap::new(),
        )
        .await
        .unwrap();
        drop(cp);

        let restored = E2bControlPlane::open(&path).await.unwrap();
        let principal = restored.authenticate(&raw_key).await.unwrap();
        assert_eq!(principal.tenant_id, "team-a");
        assert_eq!(principal.scope, Scope::Read);
        let volume_principal = restored.authenticate(&raw_volume_token).await.unwrap();
        assert_eq!(volume_principal.volume_id.as_deref(), Some(volume_id));
        assert_eq!(
            restored
                .get_volume_file("team-a", volume_id, "/state.txt")
                .await
                .unwrap()
                .content,
            b"persisted".as_slice()
        );
        let _ = tokio::fs::remove_file(path).await;
    }
}
