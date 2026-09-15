//! All requests run on the owning CodexInstance. Codex remains the goal/queue authority.
use super::{CodexOperationContext, CodexTransport, codex_request};
use giskard_core::turn::TurnOverrides;
use giskard_core::{error::HarnessError, goals_queue::*};
use giskard_harness::ThreadHandle;
use serde::Deserialize;
use serde_json::{Value, json};

async fn request(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
    method: &'static str,
    mut params: Value,
) -> Result<Value, HarnessError> {
    params["threadId"] = json!(thread.harness_thread_id);
    codex_request(
        client,
        CodexOperationContext::for_thread("goals_queue", thread),
        method,
        &params,
    )
    .await
}

fn native_status(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::UsageLimited => "usageLimited",
        GoalStatus::BudgetLimited => "budgetLimited",
        GoalStatus::Complete => "complete",
    }
}
fn text_input(text: String) -> Value {
    json!([{ "type": "text", "text": text, "text_elements": [] }])
}

pub(super) async fn execute(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
    command: GoalsQueueCommand,
    settings: Option<TurnOverrides>,
) -> Result<GoalsQueueSnapshot, HarnessError> {
    command.validate().map_err(HarnessError::Protocol)?;
    if command.requires_settings() {
        let settings = settings.ok_or_else(|| {
            HarnessError::Protocol(
                "Launch settings are required before changing native goal or queue work".into(),
            )
        })?;
        let params = settings_params(thread, &settings)?;
        request(client, thread, "thread/settings/update", params).await?;
    }
    let mut cursor = None;
    let mutation = match command {
        GoalsQueueCommand::Read { cursor: value } => {
            cursor = value;
            None
        }
        GoalsQueueCommand::SetGoal {
            objective,
            status,
            token_budget,
        } => {
            let mut params = json!({});
            if let Some(objective) = objective {
                params["objective"] = json!(objective);
            }
            if let Some(status) = status {
                params["status"] = json!(native_status(status));
            }
            if let Some(budget) = token_budget {
                params["tokenBudget"] = json!(budget);
            }
            Some(("thread/goal/set", params))
        }
        GoalsQueueCommand::ClearGoal => Some(("thread/goal/clear", json!({}))),
        GoalsQueueCommand::Add {
            text,
            client_message_id,
        } => Some((
            "thread/queue/add",
            json!({ "input": text_input(text), "clientUserMessageId": client_message_id }),
        )),
        GoalsQueueCommand::Update { id, text } => Some((
            "thread/queue/update",
            json!({ "input": text_input(text), "queuedSubmissionId": id }),
        )),
        GoalsQueueCommand::Delete { id } => {
            Some(("thread/queue/delete", json!({ "queuedSubmissionId": id })))
        }
        GoalsQueueCommand::Reorder { ids } => Some((
            "thread/queue/reorder",
            json!({ "queuedSubmissionIds": ids }),
        )),
        GoalsQueueCommand::Start { id } => {
            Some(("thread/queue/start", json!({ "queuedSubmissionId": id })))
        }
    };
    let mutated = mutation.is_some();
    if let Some((method, params)) = mutation {
        request(client, thread, method, params).await?;
    }
    let snapshot = read(client, thread, cursor).await;
    if mutated {
        snapshot.map_err(|error| HarnessError::Protocol(format!("Change accepted, but refreshing goal/queue state failed: {error}. Refresh before retrying the change.")))
    } else {
        snapshot
    }
}

fn settings_params(thread: &ThreadHandle, settings: &TurnOverrides) -> Result<Value, HarnessError> {
    let model = settings
        .model
        .as_ref()
        .ok_or_else(|| HarnessError::Protocol("Launch settings must name a model".into()))?;
    let effort = model
        .reasoning_effort
        .clone()
        .map(super::mapping::map_effort);
    Ok(json!({
        "cwd": thread.workspace_root,
        "model": model.model,
        "effort": effort,
        // Null explicitly clears a previous captured tier. Omission would retain it.
        "serviceTier": model.service_tier,
        "approvalPolicy": super::mapping::map_permission_preset_to_codex_approval(settings.permission_preset),
        "permissions": super::mapping::map_permission_preset_to_codex_permissions(settings.permission_preset),
        "collaborationMode": {
            "mode": super::mapping::map_mode_to_collaboration_mode(settings.mode),
            "settings": { "model": model.model, "reasoning_effort": effort, "developer_instructions": null }
        }
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeGoal {
    thread_id: String,
    objective: String,
    status: String,
    token_budget: Option<i64>,
    tokens_used: i64,
    time_used_seconds: i64,
    created_at: i64,
    updated_at: i64,
}
fn decode_goal(value: Value, thread: &str) -> Result<Option<Goal>, HarnessError> {
    #[derive(Deserialize)]
    struct Response {
        goal: Option<NativeGoal>,
    }
    let response: Response = serde_json::from_value(value)
        .map_err(|e| HarnessError::Protocol(format!("Invalid goal response: {e}")))?;
    let Some(goal) = response.goal else {
        return Ok(None);
    };
    if goal.thread_id != thread {
        return Err(HarnessError::Protocol(
            "Goal response belongs to a different thread".into(),
        ));
    }
    let status = match goal.status.as_str() {
        "active" => GoalStatus::Active,
        "paused" => GoalStatus::Paused,
        "blocked" => GoalStatus::Blocked,
        "usageLimited" => GoalStatus::UsageLimited,
        "budgetLimited" => GoalStatus::BudgetLimited,
        "complete" => GoalStatus::Complete,
        other => {
            return Err(HarnessError::Protocol(format!(
                "Unknown goal status: {other}"
            )));
        }
    };
    Ok(Some(Goal {
        objective: goal.objective,
        status,
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_seconds: goal.time_used_seconds,
        created_at: goal.created_at,
        updated_at: goal.updated_at,
    }))
}
fn decode_queue(value: Value) -> Result<(Vec<QueuedInput>, Option<String>), HarnessError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Entry {
        id: String,
        client_user_message_id: String,
        input: Vec<Value>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Response {
        data: Vec<Entry>,
        next_cursor: Option<String>,
    }
    let response: Response = serde_json::from_value(value)
        .map_err(|e| HarnessError::Protocol(format!("Invalid queue response: {e}")))?;
    let entries = response
        .data
        .into_iter()
        .map(|entry| QueuedInput {
            id: entry.id,
            client_message_id: entry.client_user_message_id,
            text: entry
                .input
                .iter()
                .filter_map(|input| {
                    if input["type"] == "text" {
                        input["text"].as_str()
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            has_other_input: entry.input.iter().any(|input| input["type"] != "text"),
        })
        .collect();
    Ok((entries, response.next_cursor))
}
async fn read(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
    cursor: Option<String>,
) -> Result<GoalsQueueSnapshot, HarnessError> {
    let goal = decode_goal(
        request(client, thread, "thread/goal/get", json!({})).await?,
        &thread.harness_thread_id,
    )?;
    let (queue, next_cursor) = decode_queue(
        request(
            client,
            thread,
            "thread/queue/list",
            json!({ "cursor": cursor }),
        )
        .await?,
    )?;
    Ok(GoalsQueueSnapshot {
        goal,
        queue,
        next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_keeps_non_text_input_visible() {
        let (entries, cursor) = decode_queue(json!({"data":[{"id":"q", "clientUserMessageId":"c", "input":[{"type":"text","text":"hello"},{"type":"localAudio","path":"a"}]}], "nextCursor":"next"})).unwrap();
        assert_eq!(entries[0].text, "hello");
        assert!(entries[0].has_other_input);
        assert_eq!(cursor.as_deref(), Some("next"));
    }
    #[test]
    fn malformed_queue_is_an_error() {
        assert!(decode_queue(json!({"data":[{"id":"q"}]})).is_err());
    }
    #[test]
    fn rejects_foreign_goal() {
        assert!(decode_goal(json!({"goal":{"threadId":"other","objective":"test","status":"active","tokensUsed":0,"timeUsedSeconds":0,"createdAt":0,"updatedAt":0}}), "ours").is_err());
    }
    #[test]
    fn no_goal_is_valid() {
        assert!(decode_goal(json!({"goal":null}), "ours").unwrap().is_none());
    }
}
