//! Explicit operator-installed client tools. Process workers do I/O only; the instance retains
//! native request correlation, thread identity, cancellation and response delivery authority.
use super::*;
use codex_codes::DynamicToolCallParams;
use serde_json::json;
use std::collections::HashSet;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const MAX_BYTES: usize = 1024 * 1024;
pub(super) const MAX_CONCURRENT: usize = 16;

use giskard_core::dynamic_tool_config::{
    DynamicToolConfig as ToolConfig, DynamicToolNamespaceConfig as NamespaceConfig,
};

#[derive(Clone)]
pub(super) struct Executor {
    config: ToolConfig,
    validator: Arc<jsonschema::Validator>,
}

#[derive(Default)]
pub(super) struct Registry {
    // Configuration owned for this app-server lifetime; keys are explicit namespace/tool names.
    executors: HashMap<(String, String), Executor>,
    specs: Vec<Value>,
}

impl Registry {
    pub(super) fn from_config(namespaces: Vec<NamespaceConfig>) -> Result<Self, String> {
        let mut registry = Self::default();
        let mut names = HashSet::new();
        for namespace in namespaces {
            if !valid_name(&namespace.name)
                || !names.insert(namespace.name.clone())
                || namespace.tools.is_empty()
            {
                return Err("dynamic tool namespaces must have unique identifier names and at least one tool".into());
            }
            let mut specs = Vec::new();
            for tool in namespace.tools {
                if !valid_name(&tool.name)
                    || !tool.command.is_absolute()
                    || !tool.cwd.is_absolute()
                    || !(1..=3_600_000).contains(&tool.timeout_ms)
                {
                    return Err(format!(
                        "invalid dynamic tool {}: require identifier name, absolute command/cwd, timeout_ms 1..3600000",
                        tool.name
                    ));
                }
                let validator = jsonschema::validator_for(&tool.input_schema)
                    .map_err(|e| format!("invalid inputSchema for {}: {e}", tool.name))?;
                specs.push(json!({"type":"function", "name":tool.name, "description":tool.description, "inputSchema":tool.input_schema}));
                let key = (namespace.name.clone(), tool.name.clone());
                if registry
                    .executors
                    .insert(
                        key,
                        Executor {
                            config: tool,
                            validator: Arc::new(validator),
                        },
                    )
                    .is_some()
                {
                    return Err("duplicate dynamic tool name in namespace".into());
                }
            }
            registry.specs.push(json!({"type":"namespace", "name":namespace.name, "description":namespace.description, "tools":specs}));
        }
        Ok(registry)
    }

    pub(super) fn specs(&self) -> &[Value] {
        &self.specs
    }

    pub(super) fn executor(&self, params: &DynamicToolCallParams) -> Result<Executor, String> {
        if params.thread_id.trim().is_empty()
            || params.turn_id.trim().is_empty()
            || params.call_id.trim().is_empty()
        {
            return Err("dynamic tool call requires threadId, turnId and callId".into());
        }
        let key = (
            params.namespace.clone().unwrap_or_default(),
            params.tool.clone(),
        );
        let executor = self
            .executors
            .get(&key)
            .ok_or_else(|| format!("No configured client executor for {}.{}", key.0, key.1))?;
        executor
            .validator
            .validate(&params.arguments)
            .map_err(|e| {
                format!(
                    "dynamic tool arguments do not match inputSchema at {}",
                    e.instance_path
                )
            })?;
        Ok(executor.clone())
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Output {
    pub(super) success: bool,
    content_items: Vec<ContentItem>,
    #[serde(skip)]
    pub(super) diagnostic: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", deny_unknown_fields)]
enum ContentItem {
    #[serde(rename = "inputText")]
    Text { text: String },
    #[serde(rename = "inputImage")]
    Image {
        #[serde(rename = "imageUrl")]
        image_url: String,
    },
    #[serde(rename = "inputAudio")]
    Audio {
        #[serde(rename = "audioUrl")]
        audio_url: String,
    },
}

impl Output {
    pub(super) fn failure(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            success: false,
            content_items: vec![ContentItem::Text {
                text: message.clone(),
            }],
            diagnostic: Some(message),
        }
    }
    pub(super) fn value(&self) -> Value {
        // Construct explicitly so the runtime response path is infallible.
        let items: Vec<Value> = self
            .content_items
            .iter()
            .map(|item| match item {
                ContentItem::Text { text } => json!({"type":"inputText", "text":text}),
                ContentItem::Image { image_url } => {
                    json!({"type":"inputImage", "imageUrl":image_url})
                }
                ContentItem::Audio { audio_url } => {
                    json!({"type":"inputAudio", "audioUrl":audio_url})
                }
            })
            .collect();
        json!({"success":self.success,"contentItems":items})
    }
}

// Dropping a cancelled or timed-out worker kills the entire executor process group on Unix,
// including descendants that might otherwise keep its output pipes open. The Child guard also
// kills/reaps the direct child through Tokio. Configured programs must not daemonize/escape groups.
struct ProcessGroup(Option<u32>);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: negative child PID targets only the new process group created for this call.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

async fn read_bounded(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("reading executor output: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err("executor output exceeds 1 MiB".into());
    }
    Ok(bytes)
}

impl Executor {
    pub(super) async fn execute(self, params: DynamicToolCallParams) -> Output {
        let timeout = Duration::from_millis(self.config.timeout_ms);
        let result = tokio::time::timeout(timeout, self.run(params)).await;
        match result {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => Output::failure(error),
            Err(_) => Output::failure(format!(
                "client tool executor timed out after {} ms",
                timeout.as_millis()
            )),
        }
    }

    async fn run(self, params: DynamicToolCallParams) -> Result<Output, String> {
        let mut input = serde_json::to_vec(&params).map_err(|e| e.to_string())?;
        if input.len() > MAX_BYTES {
            return Err("dynamic tool input exceeds 1 MiB".into());
        }
        input.push(b'\n');
        let mut command = tokio::process::Command::new(&self.config.command);
        command
            .args(&self.config.args)
            .current_dir(&self.config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawning configured client executor: {e}"))?;
        let _group = ProcessGroup(child.id());
        let mut stdin = child.stdin.take().ok_or("executor stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("executor stdout unavailable")?;
        let stderr = child.stderr.take().ok_or("executor stderr unavailable")?;
        let write_input = async move {
            stdin
                .write_all(&input)
                .await
                .map_err(|e| format!("writing executor input: {e}"))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| format!("closing executor input: {e}"))
        };
        let (_, stdout, stderr, status) = tokio::try_join!(
            write_input,
            read_bounded(stdout),
            read_bounded(stderr),
            async {
                child
                    .wait()
                    .await
                    .map_err(|e| format!("waiting for executor: {e}"))
            }
        )?;
        // Never log stdout, stdin or stderr: configured tools can handle credentials and secrets.
        if !status.success() {
            return Err(format!(
                "client tool executor exited with {status} ({} stderr bytes)",
                stderr.len()
            ));
        }
        serde_json::from_slice(&stdout).map_err(|e| {
            format!("executor must return one typed contentItems/success JSON object (invalid output at line {}, column {})", e.line(), e.column())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace(script: &str) -> NamespaceConfig {
        NamespaceConfig {
            name: "local_tools".into(),
            description: "Installed tools".into(),
            tools: vec![ToolConfig {
                name: "echo".into(),
                description: "Echo".into(),
                input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
                command: PathBuf::from("/bin/sh"),
                cwd: std::env::current_dir().unwrap(),
                args: vec!["-c".into(), script.into()],
                timeout_ms: 1000,
            }],
        }
    }
    fn params() -> DynamicToolCallParams {
        DynamicToolCallParams {
            namespace: Some("local_tools".into()),
            tool: "echo".into(),
            arguments: json!({"text":"test"}),
            thread_id: "native-child".into(),
            turn_id: "turn".into(),
            call_id: "call".into(),
        }
    }
    async fn execute(script: &str) -> Output {
        Registry::from_config(vec![namespace(script)])
            .unwrap()
            .executor(&params())
            .unwrap()
            .execute(params())
            .await
    }

    #[test]
    fn registration_uses_native_namespace_schema_without_exposing_executables() {
        let registry = Registry::from_config(vec![namespace("exit 1")]).unwrap();
        assert_eq!(registry.specs()[0]["type"], "namespace");
        assert_eq!(registry.specs()[0]["tools"][0]["type"], "function");
        assert_eq!(
            registry.specs()[0]["tools"][0]["inputSchema"]["type"],
            "object"
        );
        assert!(!registry.specs()[0].to_string().contains("/bin/sh"));
    }
    #[test]
    fn config_rejects_ambiguity_relative_commands_invalid_schemas_and_timeouts() {
        assert!(Registry::from_config(vec![namespace(""), namespace("")]).is_err());
        let mut ns = namespace("");
        ns.tools.push(ns.tools[0].clone());
        assert!(Registry::from_config(vec![ns]).is_err());
        for which in 0..4 {
            let mut ns = namespace("");
            match which {
                0 => ns.tools[0].command = "relative".into(),
                1 => ns.tools[0].timeout_ms = 0,
                2 => ns.tools[0].input_schema = json!({"type":"invalid"}),
                _ => ns.name = "a.b".into(),
            }
            assert!(Registry::from_config(vec![ns]).is_err());
        }
    }
    #[test]
    fn calls_require_allowlist_exact_namespace_scope_and_valid_input() {
        let registry = Registry::from_config(vec![namespace("")]).unwrap();
        for which in 0..5 {
            let mut p = params();
            match which {
                0 => p.namespace = None,
                1 => p.tool = "other".into(),
                2 => p.arguments = json!({"text":3}),
                3 => p.thread_id = String::new(),
                _ => p.arguments = json!({"text":"ok","extra":true}),
            }
            assert!(registry.executor(&p).is_err());
        }
        assert!(Registry::default().executor(&params()).is_err());
    }
    #[tokio::test]
    async fn process_receives_native_json_stdin_and_returns_typed_text_image_audio() {
        let output=execute("read -r request; case \"$request\" in *native-child*) ;; *) exit 9;; esac; printf '%s' '{\"success\":true,\"contentItems\":[{\"type\":\"inputText\",\"text\":\"ok\"},{\"type\":\"inputImage\",\"imageUrl\":\"data:image/png;base64,aA==\"},{\"type\":\"inputAudio\",\"audioUrl\":\"data:audio/wav;base64,aA==\"}]}'").await;
        assert!(output.success);
        assert_eq!(output.value()["contentItems"].as_array().unwrap().len(), 3);
    }
    #[tokio::test]
    async fn process_failure_and_malformed_output_never_synthesize_success() {
        for script in [
            "cat >/dev/null; exit 7",
            "cat >/dev/null; printf '{} ' ",
            "cat >/dev/null; printf '%s' '{\"success\":true,\"contentItems\":[{\"type\":\"text\",\"text\":\"wrong type\"}]}'",
            "cat >/dev/null; printf '%s' '{\"success\":true,\"contentItems\":[]} trailing'",
        ] {
            assert!(!execute(script).await.success, "{script}");
        }
        let mut ns = namespace("");
        ns.tools[0].command = "/nonexistent-giskard-executor".into();
        let output = Registry::from_config(vec![ns])
            .unwrap()
            .executor(&params())
            .unwrap()
            .execute(params())
            .await;
        assert!(!output.success);
    }
    #[tokio::test]
    async fn timeout_bounds_process_and_output_pipe_lifetime() {
        let mut ns = namespace("cat >/dev/null; sleep 30");
        ns.tools[0].timeout_ms = 30;
        let output = Registry::from_config(vec![ns])
            .unwrap()
            .executor(&params())
            .unwrap()
            .execute(params())
            .await;
        assert!(!output.success);
        assert!(output.value().to_string().contains("timed out"));
    }
    #[tokio::test]
    async fn output_limit_bounds_unterminated_or_infinite_streams() {
        assert!(
            !execute("cat >/dev/null; head -c 1048577 /dev/zero")
                .await
                .success
        );
        assert!(
            !execute("cat >/dev/null; head -c 1048577 /dev/zero >&2")
                .await
                .success
        );
    }
    #[tokio::test]
    async fn malformed_executor_output_diagnostics_never_echo_secret_values_or_field_names() {
        for payload in [
            r#"{"success":true,"contentItems":[{"type":"SECRET_SENTINEL_987"}]}"#,
            r#"{"success":true,"contentItems":[],"SECRET_SENTINEL_987":"private"}"#,
        ] {
            let output = execute(&format!("cat >/dev/null; printf '%s' '{payload}'")).await;
            assert!(!output.success);
            assert!(!output.value().to_string().contains("SECRET_SENTINEL_987"));
            assert!(
                !output
                    .diagnostic
                    .as_deref()
                    .unwrap()
                    .contains("SECRET_SENTINEL_987")
            );
            assert!(
                output
                    .diagnostic
                    .as_deref()
                    .unwrap()
                    .contains("invalid output at line")
            );
        }
    }
}
