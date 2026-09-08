use super::*;
use codex_codes::jsonrpc::RequestId;
use codex_codes::messages::ServerRequest;
use serde_json::json;

/// The CLI exposes this request before codex-codes has typed bindings for it. Keep its exact
/// native method and response shape here rather than passing a machine service to the browser.
pub(super) async fn respond(
    client: &mut dyn CodexTransport,
    id: &RequestId,
    request: &ServerRequest,
) -> Result<bool, HarnessError> {
    let ServerRequest::Unknown { method, params } = request else {
        return Ok(false);
    };
    if method != "currentTime/read" {
        return Ok(false);
    }
    let native_thread = params
        .as_ref()
        .and_then(|p| p.get("threadId"))
        .and_then(|v| v.as_str());
    let context = CodexOperationContext::new("respond_current_time");
    if native_thread.is_none_or(|thread| thread.trim().is_empty()) {
        warn!(action = "respond_current_time", method, request_id = ?id,
            "rejecting Codex current-time request without a non-empty threadId");
        codex_respond_error_json(
            client,
            context,
            id.clone(),
            -32602,
            "currentTime/read requires a non-empty threadId",
        )
        .await?;
    } else {
        codex_respond_json(
            client,
            context,
            id.clone(),
            json!({"currentTimeAt": chrono::Utc::now().timestamp()}),
        )
        .await?;
        debug!(action = "respond_current_time", method, request_id = ?id,
            harness_thread_id = native_thread, "answered Codex current-time request");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[derive(Default)]
    struct ClockTransport {
        responses: Vec<(RequestId, Value)>,
        errors: Vec<(RequestId, i64)>,
        fail: bool,
    }

    #[async_trait]
    impl CodexTransport for ClockTransport {
        async fn request_json(&mut self, _: &str, _: Value) -> Result<Value, HarnessError> {
            panic!("clock service must not send a client request")
        }
        async fn next_message(
            &mut self,
        ) -> Result<Option<codex_codes::ServerMessage>, CodexStreamError> {
            panic!("clock service must not consume another message")
        }
        async fn respond_json(&mut self, id: RequestId, value: Value) -> Result<(), HarnessError> {
            if self.fail {
                return Err(HarnessError::Transport("clock write failed".into()));
            }
            self.responses.push((id, value));
            Ok(())
        }
        async fn respond_error_json(
            &mut self,
            id: RequestId,
            code: i64,
            _: &str,
        ) -> Result<(), HarnessError> {
            if self.fail {
                return Err(HarnessError::Transport("clock write failed".into()));
            }
            self.errors.push((id, code));
            Ok(())
        }
        async fn shutdown_transport(self) -> Result<(), HarnessError> {
            Ok(())
        }
    }

    fn request(params: Option<Value>) -> ServerRequest {
        ServerRequest::from_envelope("currentTime/read", params).unwrap()
    }

    #[tokio::test]
    async fn clock_request_is_serviced_without_browser_events_or_thread_discovery() {
        let senders = Arc::new(EventLogs::default());
        let mut instance = CodexInstance::new(
            ClockTransport::default(),
            WorkerReceivers {
                commands: mpsc::channel(1).1,
                controls: mpsc::channel(1).1,
                shutdown: watch::channel(false).1,
                done: watch::channel(false).0,
            },
            senders.clone(),
            Arc::new(EventLog::new()),
            Arc::new(WorkerQueueWatchdog::new()),
            PathBuf::from("/workspace"),
            Vec::new(),
            HarnessBootstrap::default(),
        )
        .unwrap();
        let message = codex_codes::ServerMessage::from_value(json!({
            "id":"clock", "method":"currentTime/read", "params":{"threadId":"unseen-child"}
        }))
        .unwrap();
        assert!(matches!(
            instance.handle_server_message(message).await,
            MessageOutcome::Handled
        ));
        assert_eq!(instance.client.responses.len(), 1);
        assert!(senders.0.lock().unwrap().is_empty());
        assert!(instance.active_turns.is_empty());
    }

    #[tokio::test]
    async fn clock_request_preserves_string_and_numeric_ids_and_returns_unix_seconds() {
        let mut transport = ClockTransport::default();
        for id in [RequestId::String("clock-1".into()), RequestId::Integer(17)] {
            let before = chrono::Utc::now().timestamp();
            assert!(
                respond(
                    &mut transport,
                    &id,
                    &request(Some(json!({"threadId":"native-child"})))
                )
                .await
                .unwrap()
            );
            let (actual_id, value) = transport.responses.last().unwrap();
            assert_eq!(actual_id, &id);
            let seconds = value["currentTimeAt"].as_i64().unwrap();
            assert!((before..=chrono::Utc::now().timestamp()).contains(&seconds));
            assert_eq!(value.as_object().unwrap().len(), 1);
        }
        assert!(transport.errors.is_empty());
    }

    #[tokio::test]
    async fn clock_request_rejects_malformed_params_without_success() {
        let mut transport = ClockTransport::default();
        let id = RequestId::String("invalid-clock".into());
        for params in [
            None,
            Some(json!({})),
            Some(json!({"threadId":42})),
            Some(json!({"threadId":" "})),
        ] {
            assert!(
                respond(&mut transport, &id, &request(params))
                    .await
                    .unwrap()
            );
        }
        assert_eq!(transport.errors, vec![(id, -32602); 4]);
        assert!(transport.responses.is_empty());
    }

    #[tokio::test]
    async fn clock_request_propagates_response_transport_errors() {
        let mut transport = ClockTransport {
            fail: true,
            ..Default::default()
        };
        for params in [Some(json!({"threadId":"native"})), None] {
            let result = respond(&mut transport, &RequestId::Integer(1), &request(params)).await;
            assert!(
                matches!(result, Err(HarnessError::Transport(message)) if message == "clock write failed")
            );
        }
        assert!(transport.responses.is_empty());
    }

    #[tokio::test]
    async fn clock_request_leaves_other_server_requests_for_normal_routing() {
        let mut transport = ClockTransport::default();
        let request = ServerRequest::Unknown {
            method: "other/request".into(),
            params: None,
        };
        assert!(
            !respond(&mut transport, &RequestId::Integer(1), &request)
                .await
                .unwrap()
        );
        assert!(transport.responses.is_empty());
        assert!(transport.errors.is_empty());
    }
}
