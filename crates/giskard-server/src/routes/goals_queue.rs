//! Goal and queue projections are read from the harness, never persisted in a parallel store.
use super::*;
use giskard_core::goals_queue::{GoalsQueueCommand, GoalsQueueSnapshot};

#[derive(Default, Deserialize)]
pub(super) struct ReadQuery {
    cursor: Option<String>,
}

pub(super) async fn read(
    State(state): State<AppState>,
    AxumPath((project_id, thread_id)): AxumPath<(ProjectId, ThreadId)>,
    Query(query): Query<ReadQuery>,
) -> Result<Json<GoalsQueueSnapshot>, ApiError> {
    execute(
        &state,
        project_id,
        thread_id,
        GoalsQueueCommand::Read {
            cursor: query.cursor,
        },
    )
    .await
}

pub(super) async fn change(
    State(state): State<AppState>,
    AxumPath((project_id, thread_id)): AxumPath<(ProjectId, ThreadId)>,
    Json(command): Json<GoalsQueueCommand>,
) -> Result<Json<GoalsQueueSnapshot>, ApiError> {
    execute(&state, project_id, thread_id, command).await
}

async fn execute(
    state: &AppState,
    project_id: ProjectId,
    thread_id: ThreadId,
    command: GoalsQueueCommand,
) -> Result<Json<GoalsQueueSnapshot>, ApiError> {
    command.validate().map_err(ApiError::BadRequest)?;
    let config = state
        .store
        .load_project(project_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let thread = state
        .store
        .load_thread(project_id, thread_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !command.is_read() {
        state
            .registry
            .ensure_thread_writable(project_id, thread_id)
            .await
            .map_err(harness_api_error)?;
        if thread.archived {
            return Err(ApiError::Conflict(
                "Unarchive the thread before changing its goal or queue".into(),
            ));
        }
    }
    // Native automatic turns enter the existing event owner's external-turn admission path.
    if matches!(command, GoalsQueueCommand::Start { .. })
        && state.registry.thread_has_active_turn(thread_id).await
    {
        return Err(ApiError::Conflict(
            "Wait for the active turn to finish before starting queued input".into(),
        ));
    }
    let binding = state.registry.loaded_thread_binding(thread_id).await;
    let (handle, harness_name) = match binding {
        Some(binding) if binding.project_id() == project_id => {
            (binding.handle().clone(), binding.harness().to_owned())
        }
        Some(_) => {
            return Err(ApiError::Conflict(
                "Thread binding belongs to a different project".into(),
            ));
        }
        None if command.is_read() => (
            giskard_harness::ThreadHandle::detached(thread_id, thread.harness_thread_id),
            thread.harness.clone(),
        ),
        None => {
            return Err(ApiError::Conflict(
                "Open the thread before changing its goal or queue".into(),
            ));
        }
    };
    let harness = state
        .registry
        .harness(&config, &harness_name)
        .await
        .map_err(harness_api_error)?;
    let result = harness.goals_queue(&handle, command).await;
    if let Err(error) = &result {
        warn!(%project_id, %thread_id, %error, action = "goals_queue", "Goal/queue request failed");
    }
    result.map(Json).map_err(harness_api_error)
}
