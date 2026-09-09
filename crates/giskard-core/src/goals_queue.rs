//! Harness-owned goal and queued-input controls. Giskard stores no second copy of their state.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueuedInput {
    pub id: String,
    pub client_message_id: String,
    pub text: String,
    /// Non-text input is preserved by the harness, and cannot be replaced by the text editor.
    pub has_other_input: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalsQueueSnapshot {
    pub goal: Option<Goal>,
    pub queue: Vec<QueuedInput>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalsQueueCommand {
    Read {
        #[serde(default)]
        cursor: Option<String>,
    },
    SetGoal {
        objective: Option<String>,
        status: Option<GoalStatus>,
        token_budget: Option<i64>,
    },
    ClearGoal,
    Add {
        text: String,
        client_message_id: String,
    },
    Update {
        id: String,
        text: String,
    },
    Delete {
        id: String,
    },
    Reorder {
        ids: Vec<String>,
    },
    Start {
        id: Option<String>,
    },
}
impl GoalsQueueCommand {
    /// These controls can initiate native work, which must capture the selected settings first.
    pub fn requires_settings(&self) -> bool {
        matches!(
            self,
            Self::SetGoal { .. } | Self::Add { .. } | Self::Start { .. }
        )
    }

    pub fn is_read(&self) -> bool {
        matches!(self, Self::Read { .. })
    }
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::SetGoal {
                objective,
                token_budget,
                ..
            } => {
                if objective
                    .as_ref()
                    .is_some_and(|s| s.trim().is_empty() || s.chars().count() > 4000)
                {
                    return Err("Goal objective must contain 1–4000 characters".into());
                }
                if token_budget.is_some_and(|n| n <= 0) {
                    return Err("Token budget must be positive".into());
                }
            }
            Self::Add {
                text,
                client_message_id,
            } => {
                if text.trim().is_empty() || client_message_id.trim().is_empty() {
                    return Err("Queued text and client message ID are required".into());
                }
            }
            Self::Update { id, text } => {
                if id.trim().is_empty() || text.trim().is_empty() {
                    return Err("Queue ID and text are required".into());
                }
            }
            Self::Delete { id } | Self::Start { id: Some(id) } if id.trim().is_empty() => {
                return Err("Queue ID must not be empty".into());
            }
            Self::Reorder { ids } => {
                let unique: std::collections::HashSet<_> = ids.iter().collect();
                if unique.len() != ids.len() || ids.iter().any(|s| s.trim().is_empty()) {
                    return Err("Queue order must contain unique nonempty IDs".into());
                }
            }
            _ => {}
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_mutations() {
        assert!(
            GoalsQueueCommand::SetGoal {
                objective: Some(" ".into()),
                status: None,
                token_budget: None
            }
            .validate()
            .is_err()
        );
        assert!(
            GoalsQueueCommand::SetGoal {
                objective: None,
                status: None,
                token_budget: Some(-1)
            }
            .validate()
            .is_err()
        );
        assert!(
            GoalsQueueCommand::Reorder {
                ids: vec!["a".into(), "a".into()]
            }
            .validate()
            .is_err()
        );
        assert!(
            GoalsQueueCommand::Add {
                text: "x".into(),
                client_message_id: "".into()
            }
            .validate()
            .is_err()
        );
    }
}
