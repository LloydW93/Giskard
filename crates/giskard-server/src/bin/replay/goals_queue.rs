use giskard_core::{error::HarnessError, goals_queue::*, ids::ItemId};

/// Mutate state owned by the scripted thread; return text only when Start consumes an entry.
pub(super) fn apply(
    state: &mut GoalsQueueSnapshot,
    command: GoalsQueueCommand,
) -> Result<Option<String>, HarnessError> {
    command.validate().map_err(HarnessError::Protocol)?;
    let error = || HarnessError::Protocol("Queued input no longer exists".into());
    match command {
        GoalsQueueCommand::Read { cursor } => {
            if cursor.is_some() {
                return Err(HarnessError::Protocol("Unknown queue cursor".into()));
            }
        }
        GoalsQueueCommand::SetGoal {
            objective,
            status,
            token_budget,
        } => {
            let now = chrono::Utc::now().timestamp();
            match &mut state.goal {
                Some(goal) => {
                    if let Some(objective) = objective {
                        if objective != goal.objective || goal.status == GoalStatus::Complete {
                            goal.tokens_used = 0;
                            goal.time_used_seconds = 0;
                            goal.created_at = now;
                        }
                        goal.objective = objective;
                    }
                    if let Some(status) = status {
                        goal.status = status;
                    }
                    if let Some(budget) = token_budget {
                        goal.token_budget = Some(budget);
                    }
                    goal.updated_at = now;
                }
                None => {
                    state.goal = Some(Goal {
                        objective: objective.ok_or_else(|| {
                            HarnessError::Protocol("A new goal needs an objective".into())
                        })?,
                        status: status.unwrap_or(GoalStatus::Active),
                        token_budget,
                        tokens_used: 0,
                        time_used_seconds: 0,
                        created_at: now,
                        updated_at: now,
                    })
                }
            }
        }
        GoalsQueueCommand::ClearGoal => state.goal = None,
        GoalsQueueCommand::Add {
            text,
            client_message_id,
        } => {
            if state
                .queue
                .iter()
                .any(|q| q.client_message_id == client_message_id)
            {
                return Err(HarnessError::Protocol(
                    "Client message ID already queued".into(),
                ));
            }
            state.queue.push(QueuedInput {
                id: ItemId::new().to_string(),
                client_message_id,
                text,
                has_other_input: false,
            });
        }
        GoalsQueueCommand::Update { id, text } => {
            state
                .queue
                .iter_mut()
                .find(|q| q.id == id)
                .ok_or_else(error)?
                .text = text
        }
        GoalsQueueCommand::Delete { id } => {
            let index = state
                .queue
                .iter()
                .position(|q| q.id == id)
                .ok_or_else(error)?;
            state.queue.remove(index);
        }
        GoalsQueueCommand::Reorder { ids } => {
            if ids.len() != state.queue.len()
                || ids
                    .iter()
                    .any(|id| !state.queue.iter().any(|q| &q.id == id))
            {
                return Err(HarnessError::Protocol(
                    "Order must include every queued input exactly once".into(),
                ));
            }
            state
                .queue
                .sort_by_key(|q| ids.iter().position(|id| id == &q.id));
        }
        GoalsQueueCommand::Start { id } => {
            let index = match id {
                Some(id) => state
                    .queue
                    .iter()
                    .position(|q| q.id == id)
                    .ok_or_else(error)?,
                None if !state.queue.is_empty() => 0,
                None => return Err(error()),
            };
            return Ok(Some(state.queue.remove(index).text));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordering_is_exact_and_failure_preserves_queue() {
        let mut state = GoalsQueueSnapshot::default();
        apply(
            &mut state,
            GoalsQueueCommand::Add {
                text: "one".into(),
                client_message_id: "a".into(),
            },
        )
        .unwrap();
        apply(
            &mut state,
            GoalsQueueCommand::Add {
                text: "two".into(),
                client_message_id: "b".into(),
            },
        )
        .unwrap();
        let before = state.clone();
        assert!(
            apply(
                &mut state,
                GoalsQueueCommand::Reorder {
                    ids: vec![state_id(&before, 0)]
                }
            )
            .is_err()
        );
        assert_eq!(state, before);
        apply(
            &mut state,
            GoalsQueueCommand::Reorder {
                ids: vec![state_id(&before, 1), state_id(&before, 0)],
            },
        )
        .unwrap();
        assert_eq!(
            apply(&mut state, GoalsQueueCommand::Start { id: None })
                .unwrap()
                .as_deref(),
            Some("two")
        );
        assert_eq!(state.queue.len(), 1);
    }
    fn state_id(s: &GoalsQueueSnapshot, index: usize) -> String {
        s.queue[index].id.clone()
    }
}
