use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use clouisle_core::{ClouisleError, ImageRef, SandboxSpec};

use crate::error::ApiError;
use crate::state::{AppState, ImagePrefetchJob};

#[derive(Debug, Deserialize)]
pub struct ImagePrefetchRequest {
    #[serde(default)]
    pub image: Option<ImageRef>,
    #[serde(default)]
    pub images: Vec<ImageRef>,
}

#[derive(Debug, Serialize)]
pub struct ImagePrefetchResponse {
    pub job_id: String,
    pub jobs: Vec<String>,
}

pub async fn prefetch_images(
    State(state): State<AppState>,
    Json(request): Json<ImagePrefetchRequest>,
) -> Result<(StatusCode, Json<ImagePrefetchResponse>), ApiError> {
    let mut images = request.images;
    if let Some(image) = request.image {
        images.push(image);
    }
    if images.is_empty() {
        return Err(ApiError(ClouisleError::validation(
            "image or images is required",
        )));
    }

    let mut job_ids = Vec::with_capacity(images.len());
    for image in images {
        if image.reference.trim().is_empty() {
            return Err(ApiError(ClouisleError::validation(
                "image reference is required",
            )));
        }
        let job_id = uuid::Uuid::now_v7().to_string();
        state
            .image_jobs
            .insert(ImagePrefetchJob {
                job_id: job_id.clone(),
                image: image.clone(),
                status: "queued".into(),
                error: None,
            })
            .await;
        let jobs = state.image_jobs.clone();
        let vmm = state.vmm.clone();
        let job_id_for_task = job_id.clone();
        tokio::spawn(async move {
            jobs.update(&job_id_for_task, "running", None).await;
            let spec = SandboxSpec {
                image,
                ..SandboxSpec::default()
            };
            match vmm.prefetch_image(&spec).await {
                Ok(()) => jobs.update(&job_id_for_task, "succeeded", None).await,
                Err(error) => {
                    jobs.update(&job_id_for_task, "failed", Some(error.message)).await
                }
            }
        });
        job_ids.push(job_id);
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(ImagePrefetchResponse {
            job_id: job_ids[0].clone(),
            jobs: job_ids,
        }),
    ))
}

pub async fn get_prefetch_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<ImagePrefetchJob>, ApiError> {
    state
        .image_jobs
        .get(&job_id)
        .await
        .map(Json)
        .ok_or_else(|| ApiError(ClouisleError::not_found(format!("image job {job_id} not found"))))
}
