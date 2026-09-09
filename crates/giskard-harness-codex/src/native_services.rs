//! Optional host-owned credential services. Credentials never cross the browser boundary.
use std::{path::Path, process::Stdio, time::Duration};

use codex_codes::ServerRequest;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{CodexTransport, HarnessError};

const MAX_RESPONSE_BYTES: u64 = 65_536;
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(8);

/// Operator-owned executable argv. Empty vectors disable the corresponding service.
/// The executable must be absolute; arguments are literal and never interpreted by a shell.
#[derive(Clone, Default)]
pub struct NativeServiceProviders {
    pub attestation_command: Vec<String>,
    pub external_auth_command: Vec<String>,
}

impl NativeServiceProviders {
    pub fn validate(&self) -> Result<(), HarnessError> {
        for command in [&self.attestation_command, &self.external_auth_command] {
            if let Some(executable) = command.first()
                && !Path::new(executable).is_absolute()
            {
                return Err(HarnessError::Protocol(
                    "native service provider executable must be an absolute path".into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn handles(request: &ServerRequest) -> bool {
        matches!(
            request.method(),
            "attestation/generate" | "account/chatgptAuthTokens/refresh"
        )
    }

    pub(crate) async fn response(&self, request: &ServerRequest) -> Result<Value, &'static str> {
        let method = request.method();
        let params = match request {
            ServerRequest::ChatgptAuthTokensRefresh(params) => {
                serde_json::to_value(params).map_err(|_| "invalid native refresh parameters")?
            }
            ServerRequest::Unknown { params, .. } => params.clone().unwrap_or_else(|| json!({})),
            _ => json!({}),
        };
        match method {
            "attestation/generate" => {
                let value = invoke(&self.attestation_command, method, params).await?;
                let token = nonempty(&value, "token")?;
                Ok(json!({"token": token}))
            }
            "account/chatgptAuthTokens/refresh" => {
                if params.get("reason").and_then(Value::as_str) != Some("unauthorized") {
                    return Err("unsupported native auth refresh reason");
                }
                let previous_account = match params.get("previousAccountId") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(account)) if !account.trim().is_empty() => {
                        Some(account.clone())
                    }
                    _ => return Err("invalid native auth refresh account hint"),
                };
                let response =
                    auth_response(invoke(&self.external_auth_command, method, params).await?)?;
                if let Some(previous_account) = previous_account
                    && response["chatgptAccountId"].as_str() != Some(previous_account.as_str())
                {
                    return Err("native auth provider attempted to change accounts during refresh");
                }
                Ok(response)
            }
            _ => Err("unsupported native service"),
        }
    }

    pub(crate) async fn login(&self, client: &mut dyn CodexTransport) -> Result<(), HarnessError> {
        if self.external_auth_command.is_empty() {
            return Ok(());
        }
        let mut params = auth_response(
            invoke(
                &self.external_auth_command,
                "account/login/start",
                json!({"type": "chatgptAuthTokens"}),
            )
            .await
            .map_err(service_error)?,
        )
        .map_err(service_error)?;
        params["type"] = json!("chatgptAuthTokens");
        // Never forward an upstream auth error verbatim: it may echo request credentials.
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            client.request_json("account/login/start", params),
        )
        .await
        .map_err(|_| service_error("external auth login timed out"))?
        .map_err(|_| service_error("Codex rejected external auth login"))?;
        if response.get("type").and_then(Value::as_str) != Some("chatgptAuthTokens") {
            return Err(service_error(
                "Codex returned an unexpected external auth login response",
            ));
        }
        tracing::info!(
            action = "native_external_auth_login",
            "host-provided external auth initialized"
        );
        Ok(())
    }
}

fn service_error(message: &'static str) -> HarnessError {
    HarnessError::Protocol(message.into())
}

fn nonempty<'a>(value: &'a Value, key: &str) -> Result<&'a str, &'static str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("native service provider response has a missing or empty required field")
}

fn auth_response(value: Value) -> Result<Value, &'static str> {
    let access_token = nonempty(&value, "accessToken")?;
    let account_id = nonempty(&value, "chatgptAccountId")?;
    let plan = value.get("chatgptPlanType").unwrap_or(&Value::Null);
    if !plan.is_null() && !plan.is_string() {
        return Err("native service provider returned invalid chatgptPlanType");
    }
    Ok(
        json!({"accessToken": access_token, "chatgptAccountId": account_id, "chatgptPlanType": plan}),
    )
}

async fn invoke(command: &[String], method: &str, params: Value) -> Result<Value, &'static str> {
    invoke_with_timeout(command, method, params, PROVIDER_TIMEOUT).await
}

async fn invoke_with_timeout(
    command: &[String],
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, &'static str> {
    let Some(executable) = command.first() else {
        return Err(
            "native service unavailable: configure a trusted host provider in harness configuration",
        );
    };
    if !Path::new(executable).is_absolute() {
        return Err("native service provider executable must be an absolute path");
    }
    let mut child = tokio::process::Command::new(executable)
        .args(&command[1..])
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "native service provider could not be started")?;
    let operation = async {
        let mut input = child
            .stdin
            .take()
            .ok_or("native service provider stdin unavailable")?;
        let output = child
            .stdout
            .take()
            .ok_or("native service provider stdout unavailable")?;
        let mut payload = serde_json::to_vec(&json!({"version":1,"method":method,"params":params}))
            .map_err(|_| "native service request encoding failed")?;
        payload.push(b'\n');
        input
            .write_all(&payload)
            .await
            .map_err(|_| "native service provider input failed")?;
        drop(input);
        let mut bytes = Vec::new();
        output
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "native service provider output failed")?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err("native service provider response exceeded 65536 bytes");
        }
        if !child
            .wait()
            .await
            .map_err(|_| "native service provider wait failed")?
            .success()
        {
            return Err("native service provider exited unsuccessfully");
        }
        serde_json::from_slice(&bytes).map_err(|_| "native service provider returned invalid JSON")
    };
    let result = tokio::time::timeout(timeout, operation)
        .await
        .unwrap_or(Err("native service provider timed out"));
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shell(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }
    #[tokio::test]
    async fn provider_contract_uses_stdin_and_filters_secret_fields() {
        let providers = NativeServiceProviders {
            attestation_command: shell(
                "read payload; case \"$payload\" in *'\"version\":1'*'') printf '%s' '{\"token\":\"stub-attestation\",\"private\":\"discard\"}';; *) exit 1;; esac",
            ),
            external_auth_command: vec![],
        };
        let request =
            ServerRequest::from_envelope("attestation/generate", Some(json!({}))).unwrap();
        assert_eq!(
            providers.response(&request).await.unwrap(),
            json!({"token":"stub-attestation"})
        );
    }
    #[tokio::test]
    async fn refresh_passes_account_hint_and_reason() {
        let providers = NativeServiceProviders {
            external_auth_command: shell(
                "read payload; case \"$payload\" in *'previousAccountId'*'stub-account'*'unauthorized'*) printf '%s' '{\"accessToken\":\"stub-access\",\"chatgptAccountId\":\"stub-account\"}';; *) exit 1;; esac",
            ),
            ..Default::default()
        };
        let request = ServerRequest::from_envelope(
            "account/chatgptAuthTokens/refresh",
            Some(json!({"reason":"unauthorized","previousAccountId":"stub-account"})),
        )
        .unwrap();
        assert_eq!(
            providers.response(&request).await.unwrap(),
            json!({"accessToken":"stub-access","chatgptAccountId":"stub-account","chatgptPlanType":null})
        );
    }
    #[tokio::test]
    async fn refresh_rejects_account_switches_without_echoing_identity_or_tokens() {
        let providers = NativeServiceProviders {
            external_auth_command: shell(
                "printf '%s' '{\"accessToken\":\"secret\",\"chatgptAccountId\":\"other-account\"}'",
            ),
            ..Default::default()
        };
        let request = ServerRequest::from_envelope(
            "account/chatgptAuthTokens/refresh",
            Some(json!({"reason":"unauthorized","previousAccountId":"previous-account"})),
        )
        .unwrap();
        assert_eq!(
            providers.response(&request).await.unwrap_err(),
            "native auth provider attempted to change accounts during refresh"
        );
    }

    #[tokio::test]
    async fn provider_failures_are_bounded_and_redacted() {
        for (script, expected) in [
            (
                "cat >/dev/null; printf 'secret'; exit 2",
                "native service provider exited unsuccessfully",
            ),
            (
                "cat >/dev/null; printf 'secret'",
                "native service provider returned invalid JSON",
            ),
            (
                "cat >/dev/null; head -c 65537 /dev/zero",
                "native service provider response exceeded 65536 bytes",
            ),
            ("exec sleep 60", "native service provider timed out"),
        ] {
            assert_eq!(
                invoke_with_timeout(
                    &shell(script),
                    "test",
                    json!({}),
                    if script == "exec sleep 60" {
                        Duration::from_millis(100)
                    } else {
                        Duration::from_secs(1)
                    }
                )
                .await
                .unwrap_err(),
                expected
            );
        }
        assert!(invoke(&[], "test", json!({})).await.is_err());
        assert!(
            NativeServiceProviders {
                attestation_command: vec!["relative".into()],
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn invalid_auth_fields_never_echo_credentials() {
        for value in [
            json!({"accessToken":"secret"}),
            json!({"accessToken":"secret","chatgptAccountId":" "}),
            json!({"accessToken":"secret","chatgptAccountId":"a","chatgptPlanType":42}),
        ] {
            assert!(!auth_response(value).unwrap_err().contains("secret"));
        }
    }
}
