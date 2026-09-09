# Native capability support

This follows the Astra interaction audit and covers every remaining gap named there.
The implementation is based on Codex CLI 0.153.4 and rebased on upstream `2525288`.

| Item | Disposition | Implementation |
| --- | --- | --- |
| Goals | done | Native goal controls, budgets/usage/status, live invalidation and reconnect reads. |
| Queue management | done | Add/edit/delete/reorder/start, pagination, non-text preservation and child read-only rules. |
| Audio input | done | WAV/MP3 native audio input, signature/MIME checks, model modality validation, durable descriptors. |
| Model capabilities | done | Provider-scoped tiers, default tier, input modalities and multi-agent version, including statically configured models. |
| Service tiers | done | Persistent picker choice, advertised-value validation, native per-turn overrides. |
| Rich MCP forms | done | Nested fields/defaults/optional values, JSON editor for complex shapes, complete server-side JSON Schema validation, exact responses/retries. |
| Dynamic client tools | done | Explicit TOML executor allowlist, native registration, argument schemas, typed results and bounded process/result lifecycles. |
| Attestation integration | done | Explicit host command provider, conditional negotiation, bounded/redacted service handling. |
| External auth refresh | done | Explicit host login/refresh provider, account consistency checks, cancellation-safe request servicing. |

## Using the controls

The model picker preserves native capability metadata and offers only advertised service tiers.
“Native default” omits a per-turn override; selecting a tier persists it for subsequent messages.
WAV/MP3 attachments use native audio input when the model permits it. Other audio containers remain
ordinary file attachments with a visible explanation; they are not relabelled as supported codecs.

The Goals & queue dialog reads native state rather than a second stored copy. Goal Save and queue
Add/Start apply the selected model, effort, tier, mode, permissions and workspace through
`thread/settings/update` before issuing the native action. These settings remain the native
snapshot for autonomous work; pause/save/resume a goal to apply later selector changes. Failed
settings updates prevent launch. Status/budget-only goal edits omit an unchanged objective so
terminal-goal accounting is preserved. Queue reorder requires the complete loaded list.

Forms support nested controls and use a JSON editor when the layout is complex. The server
validates every accepted response, including local references and composite schemas. Validation
failure keeps the form pending for correction. External HTTP/file schema references are never
retrieved. The `openai/form` extension is negotiated only with this renderer/validator present.

## Configured host integrations

`[[harness.dynamic_tools]]` and its `tools` entries in `config.example.toml` register trusted
namespace/tool executors. Each has a fixed absolute command and explicit working directory; model
arguments travel as JSON on stdin, never as shell command text. Executors return typed text/image/
audio results. Unknown tools fail, and the browser cannot fabricate successful results. Execution
and undelivered large results retain bounded capacity. A failed delivery retries the cached result,
never the side effect. Timeout, cancellation and teardown terminate the process group.

`[harness].attestation_provider_command` and `external_auth_provider_command` default to empty.
See [the host provider contract](native-service-providers.md) for their versioned JSON protocol.
These are working integration hooks; live use requires an authorized attestation issuer and a
host-owned login/refresh provider. Giskard cannot manufacture trusted attestation or credential
ownership. Tests use deterministic local providers, not live account credentials. Tokens never
enter browser events or stored transcripts. A retained provider/write future survives event-loop
cancellation; shutdown kills it and failed response writes terminate the connection.

## Review artifacts

The [API inventory](api-endpoints.md), [adapter contract](../crates/giskard-harness-codex/README.md),
configuration example and specification describe the final interfaces.

- Goals: [desktop](screenshots/native-goal-desktop.png), [mobile](screenshots/native-goal-mobile.png).
- Queue: [desktop](screenshots/native-queue-desktop.png), [mobile](screenshots/native-queue-mobile.png).
- Rich forms: [desktop](screenshots/native-mcp-form-desktop.png), [mobile](screenshots/native-mcp-form-mobile.png).
- Model/tier controls: [desktop](screenshots/native-model-desktop.png), [mobile](screenshots/native-model-mobile.png).

Existing IDE and async-question screenshots were regenerated too. Browser validation uses the
replay server; native transport/provider tests use deterministic protocol and process fixtures.

## Validation

- `cargo test --workspace --no-fail-fast`: 1,128 passed; none failed or ignored.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo fmt --all --check` and JavaScript syntax validation: passed.
- Full Playwright suite: 191 passed. Focused native-feature rerun: 22 passed.
- Eight desktop/mobile screenshot cases plus two final goal captures passed and were inspected.

The original Astra interaction branch was rebased and force-pushed as `a75c0e1` after its
1,066 Rust tests passed. This follow-on work is based on that rebased commit and preserves
upstream's WebSocket and adapter module split. Live deployments must configure real trusted
host providers before enabling external auth or attestation; no credentials or service
configuration were changed during validation.
