# Astra interaction support

This change targets the interaction contract exposed by Codex CLI 0.153.4. It does not require
an Astra-specific model-name check: model discovery and arbitrary reasoning effort already flow
through the Codex adapter. The installed CLI's experimental JSON schema is the protocol baseline;
the [official app-server documentation](https://developers.openai.com/codex/app-server/)
defines `turn/steer` and its expected-turn guard.

## Implementation checklist

- [x] Preserve asynchronous questions from native `agentMessage.questions` in the domain,
  persisted turn payload, wire projection, and history/live replay.
- [x] Render choice and free-text questions, including messages with empty text; require explicit
  submission and preserve uncertain delivery rather than silently retrying.
- [x] Deliver text into the active turn with `turn/steer` and its `expectedTurnId` guard;
  acknowledge acceptance separately from turn start. Deliver idle replies through ordinary input.
- [x] Respond automatically to `currentTime/read` when Codex requests its external clock;
  do not enable or change the user's clock configuration.
- [x] Verify mapping/serialization, stale-turn rejection, backend errors, browser interactions,
  reconnect behavior, and existing thread ownership constraints.

## Protocol distinction

An asynchronous question is an ordinary agent message carrying `{title, options?}` questions.
It is not `item/tool/requestUserInput`, has no pending JSON-RPC request ID, and must not enter
Giskard's approval or generic server-request registry. Its answer is ordinary user input. The
agent keeps working while the question is visible. Selecting the default option is not consent
and does not transmit an answer. Free text is available with or without suggested options.

Question text and choices belong in the turn payload, not the bounded history index. Older
messages omit `questions`; decoding treats that as an empty list. A completed question message
must remain visible even if its text is empty. Native item identity still follows the existing
thread/turn/item mapping; no new entity authority or pending-request owner is introduced.

Steering must not reserve a second turn lease, rewrite model/mode settings, or interrupt the
current turn. Both Giskard and Codex validate the expected active turn. Completion racing with a
reply produces a visible rejection and preserves the reply for retry; it must never silently
send that reply to a subsequent turn. The native `clientUserMessageId`/`userMessage.clientId`
pair correlates exact steering echoes rather than equating identical text with delivery. General child-thread messages remain read-only; matched active-question answers are validated against
the child's current turn and item.

Question delivery receipts are browser-local and survive reload in session storage. They do not
claim to be a cross-browser answered-state authority. A lost acknowledgment remains uncertain;
a user can check the transcript and explicitly re-enable the preserved answer before submitting
again. Text steering accepts no attachments; the composer preserves them and explains that they
can be sent after the active turn finishes.

## Audit of other native surfaces

Experimental API negotiation, arbitrary reasoning effort (including `ultra`), native subagent
activity, permission/command/file approvals, and ordinary MCP elicitation are already supported.
There is no new Astra toggle needed for those paths. Native `multiAgentMode` is deprecated and
ignored according to the generated schema; it should not be introduced as a model capability fix.

The remaining native client surfaces are implemented in the follow-on
[native capability work](native-ui-capabilities.md): goals, queues, audio, model capabilities and
service tiers, rich MCP forms, configured client tools, and host authentication/attestation
providers. The linked checklist records behavior, validation, and provider prerequisites.

## Review artifacts and validation

[Desktop question UI](screenshots/async-questions-desktop.png) and
[mobile question UI](screenshots/async-questions-mobile.png) are generated from the deterministic
replay server. The existing IDE screenshots were regenerated and remained byte-identical.

- `cargo test --workspace --no-fail-fast`: 1,066 passed, none ignored.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo fmt --all --check` and `node --check crates/giskard-server/static/app.js`: passed.
- Full Playwright run: 167 passed. Final interaction rerun after correlation changes: 18 passed.
- Rust UI suite after the final browser-only fix: 29 passed.

Browser validation uses the deterministic replay harness; the protocol baseline comes from the
installed Codex app-server schema. This branch prepares the implementation without updating the
installed Giskard binary or the dotfiles upstream pin.
