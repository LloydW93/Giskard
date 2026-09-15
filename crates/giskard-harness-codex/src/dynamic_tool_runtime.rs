use super::*;
use codex_codes::{jsonrpc::RequestId, messages::ServerRequest};
use dynamic_tools::Output;
use tokio::task::{AbortHandle, JoinError, JoinSet};

pub(super) struct Running {
    thread: ThreadId,
    turn: Option<TurnId>,
    abort: AbortHandle,
}

struct Finished {
    thread: ThreadId,
    turn: Option<TurnId>,
    output: Output,
}

#[derive(Default)]
pub(super) struct Calls {
    pub(super) tasks: JoinSet<(ServerRequestId, Output)>,
    // Per-request execution lifetime. Removed on response completion, interrupt, thread/turn
    // termination; dropping the instance aborts all remaining process I/O through JoinSet.
    pub(super) running: HashMap<ServerRequestId, Running>,
    // Completed execution ownership survives a failed response write. Only a confirmed write
    // or connection teardown removes it; duplicate calls resend this exact cached output.
    finished: HashMap<ServerRequestId, Finished>,
}

impl Calls {
    pub(super) fn owns(&self, id: &ServerRequestId) -> bool {
        self.running.contains_key(id) || self.finished.contains_key(id)
    }

    pub(super) fn undelivered_ids(&self) -> Vec<ServerRequestId> {
        self.finished.keys().cloned().collect()
    }

    pub(super) fn abort_all(&mut self, reason: &str) {
        for (id, finished) in self.finished.drain() {
            warn!(request_id = %id, thread_id = %finished.thread, reason, action = "discard_dynamic_result", "discarding undelivered client tool result as connection closes");
        }
        for (id, running) in self.running.drain() {
            warn!(request_id = %id, thread_id = %running.thread, reason, action = "cancel_dynamic_tool", "cancelling client executor as connection closes");
            running.abort.abort();
        }
    }
}

impl<C: CodexTransport> CodexInstance<C> {
    pub(super) async fn dynamic_call_is_duplicate(
        &mut self,
        id: &RequestId,
        request: &ServerRequest,
    ) -> bool {
        let request_id = ServerRequestId(id.to_string());
        if !matches!(request, ServerRequest::ItemToolCall(_))
            || !self.dynamic_calls.owns(&request_id)
        {
            return false;
        }
        if self.dynamic_calls.finished.contains_key(&request_id) {
            info!(request_id = %id, action = "respond_dynamic_tool", "retrying cached result for duplicate dynamic tool request without reexecuting");
            self.send_cached_dynamic_output(&request_id).await;
        } else {
            warn!(request_id = %id, action = "execute_dynamic_tool", "ignoring duplicate in-flight dynamic tool request");
        }
        true
    }

    pub(super) async fn begin_dynamic_call(
        &mut self,
        request: &ServerRequest,
        event: &AgentEvent,
    ) -> bool {
        let ServerRequest::ItemToolCall(params) = request else {
            return false;
        };
        let AgentEvent::ServerRequestReceived {
            request,
            thread,
            turn,
        } = event
        else {
            return false;
        };
        let id = request.id.clone();
        let executor = if self.dynamic_calls.running.len() + self.dynamic_calls.finished.len()
            >= dynamic_tools::MAX_CONCURRENT
        {
            Err(
                "client tool executor capacity limit reached (16 running or undelivered results)"
                    .into(),
            )
        } else {
            self.dynamic_tools.executor(params)
        };
        match executor {
            Ok(executor) => {
                info!(thread_id = %thread, turn_id = ?turn, request_id = %id, tool_call_id = %params.call_id, namespace = ?params.namespace, tool = %params.tool, action = "execute_dynamic_tool", "starting configured client tool executor");
                let params = params.clone();
                let task_id = id.clone();
                let abort = self
                    .dynamic_calls
                    .tasks
                    .spawn(async move { (task_id, executor.execute(params).await) });
                self.dynamic_calls.running.insert(
                    id,
                    Running {
                        thread: *thread,
                        turn: *turn,
                        abort,
                    },
                );
            }
            Err(error) => {
                warn!(thread_id = %thread, turn_id = ?turn, request_id = %id, tool = %params.tool, action = "execute_dynamic_tool", "rejecting unavailable or invalid client tool invocation");
                self.deliver_dynamic_output(&id, *thread, *turn, Output::failure(error))
                    .await;
            }
        }
        true
    }

    pub(super) async fn finish_dynamic_call(
        &mut self,
        result: Option<Result<(ServerRequestId, Output), JoinError>>,
    ) {
        match result {
            Some(Ok((id, output))) => {
                if let Some(running) = self.dynamic_calls.running.remove(&id) {
                    self.deliver_dynamic_output(&id, running.thread, running.turn, output)
                        .await;
                } else {
                    debug!(request_id = %id, action = "execute_dynamic_tool", "discarding completion after client tool cancellation");
                }
            }
            Some(Err(error)) if error.is_cancelled() => {
                debug!(
                    action = "execute_dynamic_tool",
                    "client tool process I/O cancelled"
                );
            }
            Some(Err(error)) => {
                error!(%error, action = "execute_dynamic_tool", "client tool process task failed");
                // A panic cannot leave a permanently pending request. Match the Tokio task id,
                // without allowing process workers to access protocol correlation or mapping.
                let id = self
                    .dynamic_calls
                    .running
                    .iter()
                    .find(|(_, running)| running.abort.id() == error.id())
                    .map(|(id, _)| id.clone());
                if let Some(id) = id
                    && let Some(running) = self.dynamic_calls.running.remove(&id)
                {
                    self.deliver_dynamic_output(
                        &id,
                        running.thread,
                        running.turn,
                        Output::failure("client tool process task failed"),
                    )
                    .await;
                }
            }
            None => {}
        }
    }

    async fn deliver_dynamic_output(
        &mut self,
        id: &ServerRequestId,
        thread: ThreadId,
        turn: Option<TurnId>,
        output: Output,
    ) {
        info!(request_id = %id, thread_id = %thread, turn_id = ?turn, success = output.success, action = "execute_dynamic_tool", "client tool execution finished");
        if !output.success {
            let _ = broadcast_event(&self.senders, thread, || AgentEvent::Error {
                thread,
                turn,
                error: HarnessError::Protocol(format!(
                    "Client tool request {id} failed: {}",
                    output
                        .diagnostic
                        .as_deref()
                        .unwrap_or("executor reported an unsuccessful result; see the tool output")
                )),
            })
            .await;
        }
        self.dynamic_calls.finished.insert(
            id.clone(),
            Finished {
                thread,
                turn,
                output,
            },
        );
        self.send_cached_dynamic_output(id).await;
    }

    async fn send_cached_dynamic_output(&mut self, id: &ServerRequestId) {
        let Some(finished) = self.dynamic_calls.finished.get(id) else {
            return;
        };
        let thread = finished.thread;
        let turn = finished.turn;
        let value = finished.output.value();
        if let Err(error) = handle_respond_server_request(
            &mut self.client,
            &mut self.mapper,
            &self.senders,
            id,
            ServerRequestResponse::result(value),
        )
        .await
        {
            warn!(request_id = %id, thread_id = %thread, turn_id = ?turn, %error, action = "respond_dynamic_tool", "client tool result could not be delivered; execution will not be retried");
            let _ = broadcast_event(&self.senders, thread, || AgentEvent::Error {
                thread,
                turn,
                error,
            })
            .await;
        } else {
            self.dynamic_calls.finished.remove(id);
        }
    }

    pub(super) async fn cancel_dynamic_calls(
        &mut self,
        thread: ThreadId,
        turn: Option<TurnId>,
        reason: &str,
    ) {
        let ids: Vec<_> = self
            .dynamic_calls
            .running
            .iter()
            .filter(|(_, running)| {
                running.thread == thread && turn.is_none_or(|turn| running.turn == Some(turn))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(running) = self.dynamic_calls.running.remove(&id) {
                running.abort.abort();
                warn!(thread_id = %thread, request_id = %id, reason, action = "cancel_dynamic_tool", "cancelling client tool executor");
                self.deliver_dynamic_output(
                    &id,
                    thread,
                    running.turn,
                    Output::failure(format!("client tool cancelled: {reason}")),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Default)]
    struct Transport {
        responses: Vec<(RequestId, Value)>,
        fail: bool,
    }
    #[async_trait]
    impl CodexTransport for Transport {
        async fn request_json(&mut self, _: &str, _: Value) -> Result<Value, HarnessError> {
            panic!("unexpected request")
        }
        async fn next_message(
            &mut self,
        ) -> Result<Option<codex_codes::ServerMessage>, CodexStreamError> {
            std::future::pending().await
        }
        async fn respond_json(&mut self, id: RequestId, value: Value) -> Result<(), HarnessError> {
            if self.fail {
                return Err(HarnessError::Transport("write failed".into()));
            }
            self.responses.push((id, value));
            Ok(())
        }
        async fn respond_error_json(
            &mut self,
            _: RequestId,
            _: i64,
            _: &str,
        ) -> Result<(), HarnessError> {
            panic!("unexpected protocol error")
        }
        async fn shutdown_transport(self) -> Result<(), HarnessError> {
            Ok(())
        }
    }
    fn instance(script: &str) -> CodexInstance<Transport> {
        let config = serde_json::from_value(json!([{"name":"local_tools","description":"local","tools":[{"name":"run","description":"run","input_schema":{"type":"object"},"command":"/bin/sh","cwd":"/","args":["-c",script],"timeout_ms":1000}]}])).unwrap();
        let mut instance = CodexInstance::new(
            Transport::default(),
            WorkerReceivers {
                commands: mpsc::channel(1).1,
                controls: mpsc::channel(1).1,
                shutdown: watch::channel(false).1,
                done: watch::channel(false).0,
            },
            Arc::new(EventLogs::default()),
            Arc::new(EventLog::new()),
            Arc::new(WorkerQueueWatchdog::new()),
            PathBuf::from("/workspace"),
            Vec::new(),
            HarnessBootstrap::default(),
        )
        .unwrap()
        .with_dynamic_tools(dynamic_tools::Registry::from_config(config).unwrap());
        instance
            .claim_thread_route("native-parent".into(), ThreadId::new())
            .unwrap();
        instance
    }
    fn request(id: Value, thread: &str, tool: &str) -> codex_codes::ServerMessage {
        codex_codes::ServerMessage::from_value(json!({"id":id,"method":"item/tool/call","params":{"namespace":"local_tools","tool":tool,"arguments":{},"threadId":thread,"turnId":"native-turn","callId":"call"}})).unwrap()
    }
    async fn finish(instance: &mut CodexInstance<Transport>) {
        let result = instance.dynamic_calls.tasks.join_next().await;
        instance.finish_dynamic_call(result).await;
    }
    #[tokio::test]
    async fn configured_call_discovers_child_and_routes_exact_native_id_without_browser_action() {
        let mut instance = instance(
            "cat >/dev/null; printf '%s' '{\"success\":true,\"contentItems\":[{\"type\":\"inputText\",\"text\":\"done\"}]}'",
        );
        instance
            .handle_server_message(request(json!(17), "native-child", "run"))
            .await;
        assert_eq!(instance.dynamic_calls.running.len(), 1);
        let id = ServerRequestId("17".into());
        assert!(instance.mapper.pending_server_request(&id).is_ok());
        finish(&mut instance).await;
        assert_eq!(instance.client.responses[0].0, RequestId::Integer(17));
        assert_eq!(instance.client.responses[0].1["success"], true);
        assert!(instance.mapper.pending_server_request(&id).is_err());
        assert!(instance.dynamic_calls.running.is_empty());
        assert_eq!(lock_senders(&instance.senders).len(), 2);
    }
    #[tokio::test]
    async fn unknown_tool_is_explicit_failure_without_executing_or_pending_manual_success() {
        let mut instance = instance("exit 99");
        instance
            .handle_server_message(request(json!("unknown"), "child", "other"))
            .await;
        assert!(instance.dynamic_calls.tasks.is_empty());
        assert_eq!(instance.client.responses[0].1["success"], false);
        assert!(
            instance.client.responses[0]
                .1
                .to_string()
                .contains("No configured client executor")
        );
        assert!(
            instance
                .mapper
                .pending_server_request(&ServerRequestId("unknown".into()))
                .is_err()
        );
    }
    #[tokio::test]
    async fn duplicate_requests_do_not_spawn_second_execution_and_cancellation_is_scoped() {
        let mut instance = instance("cat >/dev/null; sleep 30");
        instance
            .handle_server_message(request(json!("a"), "child-a", "run"))
            .await;
        instance
            .handle_server_message(request(json!("a"), "child-a", "run"))
            .await;
        instance
            .handle_server_message(request(json!("b"), "child-b", "run"))
            .await;
        assert_eq!(instance.dynamic_calls.running.len(), 2);
        let thread = instance
            .mapper
            .pending_server_request(&ServerRequestId("a".into()))
            .unwrap()
            .thread;
        instance
            .cancel_dynamic_calls(thread, None, "test interruption")
            .await;
        assert_eq!(instance.dynamic_calls.running.len(), 1);
        assert!(
            instance
                .dynamic_calls
                .running
                .contains_key(&ServerRequestId("b".into()))
        );
        assert_eq!(instance.client.responses.len(), 1);
        assert_eq!(instance.client.responses[0].1["success"], false);
        assert!(
            instance
                .mapper
                .pending_server_request(&ServerRequestId("a".into()))
                .is_err()
        );
    }
    #[tokio::test]
    async fn running_executor_does_not_block_clock_and_failure_to_write_never_reexecutes() {
        let mut instance =
            instance("cat >/dev/null; printf '%s' '{\"success\":true,\"contentItems\":[]}'");
        instance
            .handle_server_message(request(json!("tool"), "child", "run"))
            .await;
        instance
            .handle_server_message(
                codex_codes::ServerMessage::from_value(
                    json!({"id":"clock","method":"currentTime/read","params":{"threadId":"child"}}),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(
            instance.client.responses[0].0,
            RequestId::String("clock".into())
        );
        instance.client.fail = true;
        finish(&mut instance).await;
        assert!(instance.dynamic_calls.running.is_empty());
        assert!(
            instance
                .mapper
                .pending_server_request(&ServerRequestId("tool".into()))
                .is_ok()
        );
        assert_eq!(instance.client.responses.len(), 1);
        let id = ServerRequestId("tool".into());
        assert!(instance.dynamic_calls.owns(&id));
        let cached = instance
            .dynamic_calls
            .finished
            .get(&id)
            .unwrap()
            .output
            .value();
        let (tx, rx) = oneshot::channel();
        instance
            .handle_control_command(ControlCommand::RespondServerRequest {
                id: id.clone(),
                response_payload: ServerRequestResponse::result(json!({"fake":true})),
                response: tx,
            })
            .await;
        assert!(matches!(rx.await.unwrap(), Err(HarnessError::Protocol(_))));
        instance
            .handle_server_message(request(json!("tool"), "child", "run"))
            .await;
        assert!(instance.dynamic_calls.tasks.is_empty());
        assert!(instance.dynamic_calls.owns(&id));
        assert_eq!(
            instance
                .dynamic_calls
                .finished
                .get(&id)
                .unwrap()
                .output
                .value(),
            cached
        );
        // A later duplicate retries the retained result without executing the tool again.
        instance.client.fail = false;
        instance
            .handle_server_message(request(json!("tool"), "child", "run"))
            .await;
        assert!(instance.dynamic_calls.tasks.is_empty());
        assert!(!instance.dynamic_calls.owns(&id));
        assert!(instance.mapper.pending_server_request(&id).is_err());
        assert_eq!(instance.client.responses.len(), 2);
        assert_eq!(
            instance.client.responses[1],
            (RequestId::String("tool".into()), cached)
        );
    }
    #[tokio::test]
    async fn concurrency_limit_returns_failure_without_extra_process_and_turn_end_cancels() {
        let mut instance = instance("cat >/dev/null; sleep 30");
        for id in 0..17 {
            instance
                .handle_server_message(request(json!(id), "child", "run"))
                .await;
        }
        assert_eq!(instance.dynamic_calls.running.len(), 16);
        assert_eq!(instance.client.responses.len(), 1);
        assert_eq!(instance.client.responses[0].1["success"], false);
        instance.handle_server_message(codex_codes::ServerMessage::from_value(json!({
            "method":"turn/completed", "params":{"threadId":"child","turn":{"id":"native-turn","status":"completed","items":[]}}
        })).unwrap()).await;
        assert!(instance.dynamic_calls.running.is_empty());
        assert_eq!(instance.client.responses.len(), 17);
        assert!(
            instance
                .client
                .responses
                .iter()
                .all(|(_, value)| value["success"] == false)
        );
    }

    #[tokio::test]
    async fn shutdown_aborts_every_executor_without_writing_after_connection_end() {
        let mut instance = instance("cat >/dev/null; sleep 30");
        instance
            .handle_server_message(request(json!("a"), "child-a", "run"))
            .await;
        instance
            .handle_server_message(request(json!("b"), "child-b", "run"))
            .await;
        instance.dynamic_calls.abort_all("shutdown test");
        assert!(instance.dynamic_calls.running.is_empty());
        while !instance.dynamic_calls.tasks.is_empty() {
            finish(&mut instance).await;
        }
        assert!(instance.client.responses.is_empty());
    }
    #[tokio::test]
    async fn cancelled_undelivered_result_survives_interrupt_rejection_until_connection_cleanup() {
        let mut instance = instance("cat >/dev/null; sleep 30");
        instance
            .handle_server_message(request(json!("a"), "child-a", "run"))
            .await;
        let id = ServerRequestId("a".into());
        let thread = instance.mapper.pending_server_request(&id).unwrap().thread;
        instance.client.fail = true;
        instance
            .cancel_dynamic_calls(thread, None, "thread interrupted")
            .await;
        assert!(instance.dynamic_calls.owns(&id));
        assert!(
            !instance
                .dynamic_calls
                .finished
                .get(&id)
                .unwrap()
                .output
                .success
        );
        reject_pending_requests_for_interrupted_thread(
            &mut instance.client,
            &mut instance.mapper,
            &instance.senders,
            thread,
            &instance.dynamic_calls.undelivered_ids(),
        )
        .await;
        assert!(instance.mapper.pending_server_request(&id).is_ok());
        instance
            .handle_server_message(request(json!("a"), "child-a", "run"))
            .await;
        assert!(instance.dynamic_calls.running.is_empty());
        instance.dynamic_calls.abort_all("connection teardown");
        instance.dynamic_calls.tasks.shutdown().await;
        assert!(!instance.dynamic_calls.owns(&id));
        assert!(instance.dynamic_calls.finished.is_empty());
        assert!(instance.client.responses.is_empty());
    }
    #[tokio::test]
    async fn undelivered_results_retain_executor_capacity_without_replacing_cached_outputs() {
        let mut instance = instance(
            "cat >/dev/null; printf '%s' '{\"success\":true,\"contentItems\":[{\"type\":\"inputText\",\"text\":\"retained result\"}]}'",
        );
        instance.client.fail = true;
        for id in 0..dynamic_tools::MAX_CONCURRENT {
            instance
                .handle_server_message(request(json!(id), "child", "run"))
                .await;
            finish(&mut instance).await;
        }
        assert!(instance.dynamic_calls.running.is_empty());
        assert_eq!(
            instance.dynamic_calls.finished.len(),
            dynamic_tools::MAX_CONCURRENT
        );
        let original = instance
            .dynamic_calls
            .finished
            .get(&ServerRequestId("0".into()))
            .unwrap()
            .output
            .value();
        instance
            .handle_server_message(request(json!("overloaded"), "child", "run"))
            .await;
        // Every process/result slot is held by an undelivered result: a fresh call cannot start.
        assert!(instance.dynamic_calls.tasks.is_empty());
        assert!(instance.dynamic_calls.running.is_empty());
        let rejected = &instance
            .dynamic_calls
            .finished
            .get(&ServerRequestId("overloaded".into()))
            .unwrap()
            .output;
        assert!(!rejected.success);
        assert!(rejected.value().to_string().contains("capacity limit"));
        assert_eq!(
            instance
                .dynamic_calls
                .finished
                .get(&ServerRequestId("0".into()))
                .unwrap()
                .output
                .value(),
            original
        );
        instance.dynamic_calls.abort_all("test teardown");
        assert!(instance.dynamic_calls.finished.is_empty());
    }
}
