//! Session preferences are durable; native limits are applied at the existing launch boundary.
use super::*;
use giskard_core::model::{ModelDescriptor, ModelRef, non_premium_context_window};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct Settings {
    model: ModelRef,
    advertised_maximum: Option<u32>,
    default_window: u32,
    selected_window: u32,
    override_window: Option<u32>,
    non_premium_window: Option<u32>,
    effective_window: u32,
    can_configure: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    model: ModelRef,
    context_window: Option<u32>,
}

fn same_model(left: &ModelRef, right: &ModelRef) -> bool {
    left.provider == right.provider && left.model == right.model
}

fn project_settings(
    thread: &ThreadFile,
    model: ModelRef,
    descriptor: &ModelDescriptor,
    supported: bool,
) -> Settings {
    Settings {
        advertised_maximum: descriptor.advertised_context_window.filter(|v| *v > 0),
        default_window: descriptor.default_session_context_window(),
        selected_window: crate::models::selected_session_context_window(
            descriptor,
            thread.context_window_override,
        ),
        override_window: thread.context_window_override,
        non_premium_window: non_premium_context_window(&model.model),
        effective_window: thread.context_window,
        can_configure: supported && thread.kind == ThreadKind::Primary && !thread.archived,
        model,
    }
}

async fn load(
    state: &AppState,
    project_id: ProjectId,
    thread_id: ThreadId,
) -> Result<(ThreadFile, ModelRef, ModelDescriptor, bool), ApiError> {
    let project = state
        .store
        .load_project(project_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let thread = state
        .store
        .load_thread(project_id, thread_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let model = thread.current_model.as_known().cloned().ok_or_else(|| {
        ApiError::Conflict("Select a model before configuring its context limit".into())
    })?;
    let config = state.store.load_config().await?;
    let catalog = project_model_catalog(state, &project, &config).await;
    let descriptor = crate::models::resolve_catalog_descriptor(&catalog, &config, &model);
    let harness = state
        .registry
        .harness(&project)
        .await
        .map_err(harness_api_error)?;
    Ok((
        thread,
        model,
        descriptor,
        harness.capabilities().context_window_configuration,
    ))
}

pub(super) async fn read(
    State(state): State<AppState>,
    AxumPath((project_id, thread_id)): AxumPath<(ProjectId, ThreadId)>,
) -> Result<Json<Settings>, ApiError> {
    let (thread, model, descriptor, supported) = load(&state, project_id, thread_id).await?;
    Ok(Json(project_settings(
        &thread,
        model,
        &descriptor,
        supported,
    )))
}

pub(super) async fn update(
    State(state): State<AppState>,
    AxumPath((project_id, thread_id)): AxumPath<(ProjectId, ThreadId)>,
    Json(request): Json<Update>,
) -> Result<Json<Settings>, ApiError> {
    state
        .registry
        .ensure_thread_writable(project_id, thread_id)
        .await
        .map_err(harness_api_error)?;
    let (thread, model, descriptor, supported) = load(&state, project_id, thread_id).await?;
    if !supported || thread.archived {
        return Err(ApiError::Conflict(
            "Context settings require a writable, unarchived thread and a supporting harness"
                .into(),
        ));
    }
    if !same_model(&model, &request.model) {
        return Err(ApiError::Conflict(
            "The selected model changed; reopen Context and try again".into(),
        ));
    }
    if let Some(value) = request.context_window {
        let maximum = descriptor
            .advertised_context_window
            .filter(|v| *v > 0)
            .ok_or_else(|| {
                ApiError::BadRequest(
                    "The model has not advertised its maximum context window".into(),
                )
            })?;
        if value < descriptor.default_session_context_window() || value > maximum {
            return Err(ApiError::BadRequest(format!(
                "Context limit must be between {} and {} tokens",
                descriptor.default_session_context_window(),
                maximum
            )));
        }
    }
    let mut stale = false;
    let current = state
        .thread_metadata
        .mutate(project_id, thread_id, |current| {
            if current.archived
                || current.kind != ThreadKind::Primary
                || !current
                    .current_model
                    .as_known()
                    .is_some_and(|m| same_model(m, &request.model))
            {
                stale = true;
                return;
            }
            current.context_window_override = request.context_window;
        })
        .await?
        .into_current()
        .ok_or(ApiError::NotFound)?;
    if stale {
        return Err(ApiError::Conflict(
            "The thread changed; reopen Context and try again".into(),
        ));
    }
    info!(%project_id, %thread_id, context_window = ?request.context_window, action = "set_context_window", "saved session context preference for subsequent turns");
    Ok(Json(project_settings(
        &current,
        model,
        &descriptor,
        supported,
    )))
}
