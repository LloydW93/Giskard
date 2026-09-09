use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use giskard_core::{
    ThreadId,
    error::HarnessError,
    model::{ModelDescriptor, ModelRef},
};
use giskard_harness::{HarnessCapabilities, HarnessProvider, OpenThreadOptions, ThreadHandle};
use giskard_proto::{ClientMessage, ServerMessage};
use giskard_testenv::{
    TestServer,
    fake::{FakeCore, FakeHarness, Script, TurnCall},
    ws,
};
use serde_json::{Value, json};

type LaunchedLimits = Arc<Mutex<Vec<(ThreadId, Option<u32>)>>>;

struct ContextScript {
    maximum: Option<u32>,
    opened: Arc<Mutex<Vec<Option<u32>>>>,
    launched: LaunchedLimits,
}

#[async_trait]
impl Script for ContextScript {
    fn capabilities(&self) -> HarnessCapabilities {
        HarnessCapabilities {
            context_window_configuration: true,
            model_listing: true,
            provider_listing: true,
            ..giskard_testenv::fake::caps::TURNS
        }
    }
    async fn list_models(&self) -> Result<Vec<ModelDescriptor>, HarnessError> {
        let mut model = ModelDescriptor::conservative("openai", "gpt-6-astra");
        model.context_window = self.maximum.unwrap_or(1_000_000);
        model.advertised_context_window = self.maximum;
        Ok(vec![model])
    }
    async fn list_providers(&self) -> Result<Vec<HarnessProvider>, HarnessError> {
        Ok(vec![HarnessProvider {
            id: "openai".into(),
            name: None,
            base_url: None,
            auth: None,
        }])
    }
    async fn open_thread(
        &self,
        core: &FakeCore,
        opts: &OpenThreadOptions,
    ) -> Result<ThreadHandle, HarnessError> {
        self.opened.lock().unwrap().push(opts.context_window);
        Ok(core.opened(opts, format!("native-{}", opts.thread)))
    }
    async fn start_turn(&self, core: &FakeCore, call: &TurnCall) -> Result<(), HarnessError> {
        self.launched
            .lock()
            .unwrap()
            .push((call.thread, call.overrides.context_window));
        core.complete_turn(call.thread, call.turn);
        Ok(())
    }
}

fn model() -> Value {
    json!({"provider":"openai", "model":"gpt-6-astra"})
}

async fn get(server: &TestServer, path: &str) -> Value {
    let response = server
        .client
        .get(server.url(path))
        .header("cookie", &server.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.unwrap()
}

async fn start(server: &TestServer, project: giskard_core::ProjectId) -> ThreadId {
    let response = server.client.post(server.url(&format!("/api/projects/{project}/threads/start")))
        .header("cookie", &server.cookie)
        .json(&json!({"model_ref":model(), "text":"hello", "mode":"build", "permission_preset":"ask_first"}))
        .send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let id = serde_json::from_value(response.json::<Value>().await.unwrap()["thread_id"].clone())
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while server.state.registry.thread_has_active_turn(id).await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn defaults_and_overrides_reach_native_boundaries_and_stay_session_scoped() {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let launched = Arc::new(Mutex::new(Vec::new()));
    let harness = FakeHarness::new(ContextScript {
        maximum: Some(1_000_000),
        opened: opened.clone(),
        launched: launched.clone(),
    });
    let server = TestServer::builder(giskard_testenv::fake::factory(harness))
        .start()
        .await;
    let project = server.create_project("context policy").await;
    let first = start(&server, project.id).await;
    let second = start(&server, project.id).await;
    assert_eq!(*opened.lock().unwrap(), vec![Some(272_000), Some(272_000)]);
    let path = format!(
        "/api/projects/{}/threads/{first}/context-window",
        project.id
    );
    let second_path = format!(
        "/api/projects/{}/threads/{second}/context-window",
        project.id
    );
    server
        .state
        .store
        .update_thread(project.id, first, |tf| {
            let selected = tf.current_model.as_known().unwrap().clone();
            tf.record_model_context_window(&selected, 800_000);
        })
        .await
        .unwrap();
    let settings = get(&server, &path).await;
    assert_eq!(settings["default_window"], 272_000);
    assert_eq!(settings["advertised_maximum"], 800_000);
    assert!(settings["override_window"].is_null());
    let response = server
        .client
        .post(server.url(&path))
        .header("cookie", &server.cookie)
        .json(&json!({"model":model(),"context_window":512000}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(get(&server, &path).await["selected_window"], 512_000);
    assert_eq!(get(&server, &second_path).await["selected_window"], 272_000);
    assert_eq!(
        server
            .state
            .store
            .load_thread(project.id, first)
            .await
            .unwrap()
            .unwrap()
            .context_window_override,
        Some(512_000)
    );
    // The preference is durable but saving it does not interrupt or recreate native work.
    assert_eq!(opened.lock().unwrap().len(), 2);
    let mut socket = server.ws().await;
    socket
        .send(ws::text(&ClientMessage::Subscribe {
            thread_id: first,
            since: None,
        }))
        .await
        .unwrap();
    socket
        .send(ws::text(&ClientMessage::SendInput {
            thread_id: first,
            text: "next".into(),
            attachments: vec![],
        }))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let frame = socket.next().await.unwrap().unwrap();
            if let tokio_tungstenite::tungstenite::Message::Text(text) = frame
                && let Ok(ServerMessage::Event { agent_event, .. }) = serde_json::from_str(&text)
                && matches!(
                    *agent_event,
                    giskard_proto::WireAgentEvent::TurnCompleted { .. }
                )
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(launched.lock().unwrap().contains(&(first, Some(512_000))));
    for invalid in [0, 271_999, 800_001] {
        let response = server
            .client
            .post(server.url(&path))
            .header("cookie", &server.cookie)
            .json(&json!({"model":model(),"context_window":invalid}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    }
    let response = server.client.post(server.url(&path)).header("cookie", &server.cookie)
        .json(&json!({"model":{"provider":"different","model":"gpt-6-astra"},"context_window":512000})).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(get(&server, &path).await["selected_window"], 512_000);
    server
        .state
        .store
        .update_thread(project.id, first, |tf| {
            let mut selected = tf.current_model.as_known().unwrap().clone();
            selected.reasoning_effort = Some(giskard_core::model::Effort::new("high"));
            tf.current_model = giskard_core::turn::TurnModel::Known(selected);
        })
        .await
        .unwrap();
    assert_eq!(get(&server, &path).await["override_window"], 512_000);
    server
        .state
        .store
        .update_thread(project.id, first, |tf| {
            tf.current_model = giskard_core::turn::TurnModel::Known(ModelRef {
                provider: "openai".into(),
                model: "other".into(),
                reasoning_effort: None,
                service_tier: None,
            });
        })
        .await
        .unwrap();
    assert!(
        server
            .state
            .store
            .load_thread(project.id, first)
            .await
            .unwrap()
            .unwrap()
            .context_window_override
            .is_none()
    );
}

#[tokio::test]
async fn small_remote_limits_win_and_unknown_maxima_cannot_be_overridden() {
    for maximum in [Some(128_000), None] {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let launched = Arc::new(Mutex::new(Vec::new()));
        let harness = FakeHarness::new(ContextScript {
            maximum,
            opened: opened.clone(),
            launched: launched.clone(),
        });
        let server = TestServer::builder(giskard_testenv::fake::factory(harness))
            .start()
            .await;
        let project = server.create_project("limited context").await;
        let thread = start(&server, project.id).await;
        assert_eq!(*opened.lock().unwrap(), vec![maximum]);
        assert_eq!(*launched.lock().unwrap(), vec![(thread, maximum)]);
        let path = format!(
            "/api/projects/{}/threads/{thread}/context-window",
            project.id
        );
        let settings = get(&server, &path).await;
        assert_eq!(settings["default_window"], maximum.unwrap_or(272_000));
        let response = server
            .client
            .post(server.url(&path))
            .header("cookie", &server.cookie)
            .json(&json!({"model":model(),"context_window":512000}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        if maximum.is_none() {
            server
                .state
                .store
                .update_thread(project.id, thread, |tf| {
                    let selected = tf.current_model.as_known().unwrap().clone();
                    tf.record_model_context_window(&selected, 828_400);
                    tf.record_model_context_window(&selected, 258_400);
                })
                .await
                .unwrap();
            let settings = get(&server, &path).await;
            assert_eq!(settings["advertised_maximum"], 828_400);
            assert_eq!(settings["default_window"], 272_000);
            assert_eq!(settings["effective_window"], 258_400);
            let response = server
                .client
                .post(server.url(&path))
                .header("cookie", &server.cookie)
                .json(&json!({"model":model(),"context_window":512000}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
        }
        let response = server
            .client
            .post(server.url(&path))
            .header("cookie", &server.cookie)
            .json(&json!({"model":model(),"context_window":null}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        server
            .state
            .store
            .update_thread(project.id, thread, |tf| tf.archived = true)
            .await
            .unwrap();
        assert_eq!(get(&server, &path).await["can_configure"], false);
        let response = server
            .client
            .post(server.url(&path))
            .header("cookie", &server.cookie)
            .json(&json!({"model":model(),"context_window":null}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    }
}
