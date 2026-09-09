//! Verified per-session native configuration. Ordinary warm resume ignores overrides.
use super::{CodexMapper, CodexOperationContext, CodexTransport, codex_request};
use giskard_core::{error::HarnessError, model::ModelRef, turn::TurnOverrides};
use giskard_harness::ThreadHandle;
use serde_json::{Value, json};
use std::time::Duration;

pub(super) fn config(window: u32) -> Result<Value, HarnessError> {
    if window == 0 {
        return Err(HarnessError::Protocol(
            "Context window must be positive".into(),
        ));
    }
    Ok(
        json!({"model_context_window": window, "model_auto_compact_token_limit": window,
        "model_auto_compact_token_limit_scope": "total"}),
    )
}

async fn rpc(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
    method: &'static str,
    mut params: Value,
) -> Result<Value, HarnessError> {
    if method.starts_with("thread/") {
        params["threadId"] = json!(thread.harness_thread_id);
    }
    tokio::time::timeout(
        Duration::from_secs(20),
        codex_request(
            client,
            CodexOperationContext::for_thread("context_configure", thread),
            method,
            &params,
        ),
    )
    .await
    .map_err(|_| {
        HarnessError::Timeout(format!(
            "Context configuration {method} timed out; delivery is uncertain"
        ))
    })?
}

fn identity(value: &Value, thread: &ThreadHandle) -> Result<(), HarnessError> {
    if value["thread"]["id"] != thread.harness_thread_id {
        return Err(HarnessError::Protocol(
            "Context configuration returned a different native thread".into(),
        ));
    }
    Ok(())
}

async fn idle(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
) -> Result<Value, HarnessError> {
    let read = rpc(client, thread, "thread/read", json!({"includeTurns":false})).await?;
    identity(&read, thread)?;
    if read["thread"]["status"]["type"] != "idle" {
        return Err(HarnessError::ThreadBusy {
            thread: thread.thread,
        });
    }
    Ok(read)
}

async fn effort_seen(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
    expected: &str,
) -> Result<(), HarnessError> {
    // Settings acknowledgement only queues the update. Read live snapshots until the core
    // has applied it; leave every notification in the transport inbox for normal reduction.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let read = idle(client, thread).await?;
        if read["thread"]["reasoningEffort"] == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(HarnessError::Timeout(
                "Native reasoning setting was not confirmed before context reconfiguration".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn efforts(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
    model: &ModelRef,
) -> Result<(String, String), HarnessError> {
    let provider = tokio::time::timeout(
        Duration::from_secs(20),
        super::default_model_provider(client, thread.workspace_root.to_string_lossy().into_owned()),
    )
    .await
    .map_err(|_| {
        HarnessError::Timeout("Native model catalog provider lookup timed out".into())
    })??;
    if provider != model.provider {
        return Err(HarnessError::Protocol("The native model catalog belongs to another provider; cannot attest reasoning settings for context reconfiguration".into()));
    }
    let mut cursor = Value::Null;
    let mut seen = std::collections::HashSet::new();
    loop {
        let page = rpc(
            client,
            thread,
            "model/list",
            json!({"cursor":cursor,"includeHidden":true}),
        )
        .await?;
        let entries = page["data"].as_array().ok_or_else(|| {
            HarnessError::Protocol("Native model catalog is missing entries".into())
        })?;
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry["model"] == model.model || entry["id"] == model.model)
        {
            let desired = model.reasoning_effort.as_ref().map(|e| e.as_str())
                .or_else(|| entry["defaultReasoningEffort"].as_str())
                .ok_or_else(|| HarnessError::Protocol("Native catalog has no default reasoning effort for context reconfiguration".into()))?;
            let advertised = entry["supportedReasoningEfforts"].as_array()
                .ok_or_else(|| HarnessError::Protocol("Native model does not advertise reasoning settings for verified context reconfiguration".into()))?;
            if !advertised.iter().any(|e| e["reasoningEffort"] == desired) {
                return Err(HarnessError::Protocol(
                    "Selected reasoning effort is not advertised by the native model".into(),
                ));
            }
            let marker = advertised.iter().filter_map(|e| e["reasoningEffort"].as_str()).find(|e| *e != desired)
                .ok_or_else(|| HarnessError::Protocol("This model has no alternate reasoning effort to verify a context change; start a new session with the desired limit".into()))?;
            return Ok((desired.to_owned(), marker.to_owned()));
        }
        cursor = page["nextCursor"].clone();
        if cursor.is_null() {
            break;
        }
        if !seen.insert(cursor.to_string()) {
            return Err(HarnessError::Protocol(
                "Native model catalog repeated a cursor".into(),
            ));
        }
    }
    Err(HarnessError::Protocol(
        "Selected model is absent from the native catalog; cannot verify context configuration"
            .into(),
    ))
}

async fn resume(
    client: &mut dyn CodexTransport,
    thread: &ThreadHandle,
    model: &ModelRef,
    config: Option<Value>,
) -> Result<Value, HarnessError> {
    let mut params = json!({"model":model.model,"modelProvider":model.provider,"cwd":thread.workspace_root,"excludeTurns":true});
    if let Some(config) = config {
        params["config"] = config;
    }
    let result = rpc(client, thread, "thread/resume", params).await?;
    identity(&result, thread)?;
    Ok(result)
}

pub(super) async fn ensure(
    client: &mut dyn CodexTransport,
    mapper: &mut CodexMapper,
    thread: &ThreadHandle,
    overrides: &TurnOverrides,
) -> Result<(), HarnessError> {
    let Some(window) = overrides.context_window else {
        return Ok(());
    };
    let mut configuration = config(window)?;
    if !mapper.has_thread_route(thread) {
        return Err(HarnessError::Protocol(
            "Reopen the changed native thread binding before configuring context".into(),
        ));
    }
    if mapper.applied_context_window(thread) == Some(window) {
        return Ok(());
    }
    if mapper
        .active_giskard_turn_for_thread(thread.thread)
        .is_some()
    {
        return Err(HarnessError::ThreadBusy {
            thread: thread.thread,
        });
    }
    let model = overrides.model.as_ref().ok_or_else(|| {
        HarnessError::Protocol("Context configuration requires a selected model".into())
    })?;
    let state = idle(client, thread).await?;
    if state["thread"]["canAcceptDirectInput"] == false {
        return Err(HarnessError::Protocol(
            "Provider-owned child sessions cannot change context through public resume".into(),
        ));
    }

    if state["thread"]["modelProvider"] != model.provider {
        return Err(HarnessError::Protocol(
            "Context configuration cannot change the native provider".into(),
        ));
    }
    let goal = rpc(client, thread, "thread/goal/get", json!({})).await?;
    let queue = rpc(client, thread, "thread/queue/list", json!({"limit":1})).await?;
    if goal.get("goal").is_none()
        || (!goal["goal"].is_null() && goal["goal"]["threadId"] != thread.harness_thread_id)
    {
        return Err(HarnessError::Protocol(
            "Native goal state could not be verified for context configuration".into(),
        ));
    }
    if goal["goal"]["status"] == "active"
        || queue["data"].as_array().is_none_or(|q| !q.is_empty())
        || !queue["nextCursor"].is_null()
    {
        return Err(HarnessError::Protocol("Pause the native goal and empty the native queue before applying a changed context limit".into()));
    }
    let (desired, marker) = efforts(client, thread, model).await?;
    configuration["model_reasoning_effort"] = json!(desired);
    tracing::info!(thread_id = %thread.thread, context_window = window, "applying verified native session context configuration");
    mapper.set_context_window(thread, None)?;
    let apply = async {
        rpc(client, thread, "thread/settings/update", json!({"model":model.model,"effort":marker})).await?;
        effort_seen(client, thread, &marker).await?;
        rpc(client, thread, "thread/unsubscribe", json!({})).await?;
        let result = resume(client, thread, model, Some(configuration)).await?;
        if result["reasoningEffort"] != desired || result["model"] != model.model || result["modelProvider"] != model.provider {
            return Err(HarnessError::Protocol("Codex ignored context overrides for the loaded thread; the context change was not applied".into()));
        }
        Ok(())
    }.await;
    if let Err(error) = apply {
        tracing::warn!(thread_id = %thread.thread, %error, "native context configuration failed; restoring thread subscription and effort");
        let recover = async {
            resume(client, thread, model, None).await?;
            rpc(
                client,
                thread,
                "thread/settings/update",
                json!({"model":model.model,"effort":desired}),
            )
            .await?;
            effort_seen(client, thread, &desired).await
        }
        .await;
        return match recover {
            Ok(()) => Err(error),
            Err(recovery) => Err(HarnessError::Protocol(format!(
                "Context change failed: {error}. Restoring the native session also failed: {recovery}. Reopen this session before starting more work."
            ))),
        };
    }
    mapper.set_context_window(thread, Some(window))?;
    tracing::info!(thread_id = %thread.thread, context_window = window, "verified native session context configuration applied");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use giskard_core::{
        ids::ThreadId,
        model::Effort,
        turn::{Mode, PermissionPreset},
    };
    use std::path::PathBuf;

    struct Native {
        calls: Vec<(String, Value)>,
        effort: String,
        context: Option<u32>,
        ignored: bool,
        active: bool,
        goal: bool,
        queued: bool,
        single_effort: bool,
        fail_resume: bool,
    }
    impl Default for Native {
        fn default() -> Self {
            Self {
                calls: vec![],
                effort: "high".into(),
                context: None,
                ignored: false,
                active: false,
                goal: false,
                queued: false,
                single_effort: false,
                fail_resume: false,
            }
        }
    }
    #[async_trait::async_trait]
    impl CodexTransport for Native {
        async fn request_json(
            &mut self,
            method: &str,
            params: Value,
        ) -> Result<Value, HarnessError> {
            self.calls.push((method.to_owned(), params.clone()));
            Ok(match method {
                "thread/read" => {
                    json!({"thread":{"id":"native","modelProvider":"openai","reasoningEffort":self.effort,
                    "status":{"type":if self.active {"active"} else {"idle"}}}})
                }
                "thread/goal/get" => {
                    json!({"goal":if self.goal {json!({"threadId":"native","status":"active"})} else {Value::Null}})
                }
                "thread/queue/list" => {
                    json!({"data":if self.queued {vec![json!({"id":"queued"})]} else {vec![]},"nextCursor":null})
                }
                "config/read" => json!({"config":{"model_provider":"openai"},"origins":{}}),
                "model/list" => {
                    json!({"data":[{"id":"astra","model":"gpt-6-astra","defaultReasoningEffort":"high",
                    "supportedReasoningEfforts":if self.single_effort {vec![json!({"reasoningEffort":"high"})]}
                    else {vec![json!({"reasoningEffort":"high"}),json!({"reasoningEffort":"low"})]}}],"nextCursor":null})
                }
                "thread/settings/update" => {
                    self.effort = params["effort"].as_str().unwrap().into();
                    json!({})
                }
                "thread/unsubscribe" => json!({"status":"unsubscribed"}),
                "thread/resume" => {
                    if params.get("config").is_some() {
                        if self.fail_resume {
                            self.fail_resume = false;
                            return Err(HarnessError::Transport("resume failed".into()));
                        }
                        if !self.ignored {
                            self.effort = params["config"]["model_reasoning_effort"]
                                .as_str()
                                .unwrap()
                                .into();
                            self.context = params["config"]["model_context_window"]
                                .as_u64()
                                .map(|v| v as u32);
                        }
                    }
                    json!({"thread":{"id":"native"},"model":"gpt-6-astra","modelProvider":"openai","reasoningEffort":self.effort})
                }
                _ => panic!("unexpected method: {method}"),
            })
        }
        async fn next_message(
            &mut self,
        ) -> Result<Option<codex_codes::ServerMessage>, crate::rpc::CodexStreamError> {
            panic!("context reload must preserve the ordinary inbox")
        }
        async fn respond_json(
            &mut self,
            _: codex_codes::jsonrpc::RequestId,
            _: Value,
        ) -> Result<(), HarnessError> {
            panic!("unexpected response")
        }
        async fn respond_error_json(
            &mut self,
            _: codex_codes::jsonrpc::RequestId,
            _: i64,
            _: &str,
        ) -> Result<(), HarnessError> {
            panic!("unexpected response")
        }
        async fn shutdown_transport(self) -> Result<(), HarnessError> {
            panic!("context reload must not stop the process")
        }
    }
    fn fixture() -> (CodexMapper, ThreadHandle, TurnOverrides) {
        let thread = ThreadHandle::opened(
            ThreadId::new(),
            "native".into(),
            PathBuf::from("/workspace"),
        );
        let mut mapper = CodexMapper::new(PathBuf::from("/workspace"));
        mapper.claim_thread("native".into(), thread.thread).unwrap();
        let settings = TurnOverrides {
            context_window: Some(272000),
            model: Some(ModelRef {
                provider: "openai".into(),
                model: "gpt-6-astra".into(),
                reasoning_effort: Some(Effort::new("high")),
                service_tier: None,
            }),
            mode: Mode::Build,
            permission_preset: PermissionPreset::AskFirst,
        };
        (mapper, thread, settings)
    }
    #[test]
    fn context_configuration_is_total_and_positive() {
        assert!(config(0).is_err());
        assert_eq!(
            config(272000).unwrap(),
            json!({"model_context_window":272000,"model_auto_compact_token_limit":272000,"model_auto_compact_token_limit_scope":"total"})
        );
    }
    #[tokio::test]
    async fn verified_reload_preserves_identity_and_confirms_marker_before_unsubscribe() {
        let (mut mapper, thread, settings) = fixture();
        let mut native = Native::default();
        ensure(&mut native, &mut mapper, &thread, &settings)
            .await
            .unwrap();
        assert_eq!(native.context, Some(272000));
        assert_eq!(native.effort, "high");
        assert_eq!(mapper.applied_context_window(&thread), Some(272000));
        assert!(mapper.has_thread_route(&thread));
        let methods: Vec<_> = native.calls.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(
            methods,
            vec![
                "thread/read",
                "thread/goal/get",
                "thread/queue/list",
                "config/read",
                "model/list",
                "thread/settings/update",
                "thread/read",
                "thread/unsubscribe",
                "thread/resume"
            ]
        );
        let config = &native.calls.last().unwrap().1["config"];
        assert_eq!(config["model_reasoning_effort"], "high");
        assert_eq!(config["model_auto_compact_token_limit_scope"], "total");
        let count = native.calls.len();
        ensure(&mut native, &mut mapper, &thread, &settings)
            .await
            .unwrap();
        assert_eq!(
            native.calls.len(),
            count,
            "verified unchanged cap needs no reload"
        );
    }
    #[tokio::test]
    async fn successful_ignored_resume_is_failure_and_recovers_subscription_and_effort() {
        let (mut mapper, thread, settings) = fixture();
        let mut native = Native {
            ignored: true,
            ..Default::default()
        };
        let error = ensure(&mut native, &mut mapper, &thread, &settings)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ignored context overrides"));
        assert_eq!(native.effort, "high");
        assert_eq!(mapper.applied_context_window(&thread), None);
        let suffix: Vec<_> = native
            .calls
            .iter()
            .rev()
            .take(3)
            .map(|c| c.0.as_str())
            .collect();
        assert_eq!(
            suffix,
            vec!["thread/read", "thread/settings/update", "thread/resume"]
        );
    }
    #[tokio::test]
    async fn failed_resume_recovers_without_false_confirmation() {
        let (mut mapper, thread, settings) = fixture();
        let mut native = Native {
            fail_resume: true,
            ..Default::default()
        };
        assert!(
            ensure(&mut native, &mut mapper, &thread, &settings)
                .await
                .is_err()
        );
        assert_eq!(native.effort, "high");
        assert_eq!(mapper.applied_context_window(&thread), None);
    }
    #[tokio::test]
    async fn running_native_work_is_refused_before_mutation() {
        let (mut mapper, thread, settings) = fixture();
        let mut native = Native {
            active: true,
            ..Default::default()
        };
        assert!(matches!(
            ensure(&mut native, &mut mapper, &thread, &settings).await,
            Err(HarnessError::ThreadBusy { .. })
        ));
        assert_eq!(native.calls.len(), 1);
    }
    #[tokio::test]
    async fn autonomous_goal_or_queued_input_prevents_reload() {
        for (goal, queued) in [(true, false), (false, true)] {
            let (mut mapper, thread, settings) = fixture();
            let mut native = Native {
                goal,
                queued,
                ..Default::default()
            };
            assert!(
                ensure(&mut native, &mut mapper, &thread, &settings)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("Pause the native goal")
            );
            assert!(native.calls.iter().all(|c| c.0 != "thread/settings/update"));
        }
    }
    #[tokio::test]
    async fn missing_alternate_effort_is_explicit_and_nonmutating() {
        let (mut mapper, thread, settings) = fixture();
        let mut native = Native {
            single_effort: true,
            ..Default::default()
        };
        assert!(
            ensure(&mut native, &mut mapper, &thread, &settings)
                .await
                .unwrap_err()
                .to_string()
                .contains("no alternate reasoning effort")
        );
        assert!(native.calls.iter().all(|c| c.0 != "thread/settings/update"));
    }
    #[tokio::test]
    async fn model_default_effort_is_resolved_from_catalog() {
        let (mut mapper, thread, mut settings) = fixture();
        settings.model.as_mut().unwrap().reasoning_effort = None;
        let mut native = Native::default();
        ensure(&mut native, &mut mapper, &thread, &settings)
            .await
            .unwrap();
        assert_eq!(native.effort, "high");
    }
    #[test]
    fn native_unload_invalidates_configuration_without_losing_identity() {
        let (mut mapper, thread, _) = fixture();
        mapper.set_context_window(&thread, Some(272000)).unwrap();
        mapper.map_notification(
            &codex_codes::Notification::ThreadClosed(codex_codes::ThreadClosedNotification {
                thread_id: "native".into(),
            }),
            thread.thread,
        );
        assert_eq!(mapper.applied_context_window(&thread), None);
        assert!(mapper.has_thread_route(&thread));
    }
}
