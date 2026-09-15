# Native Codex service providers

Giskard can answer `attestation/generate` and `account/chatgptAuthTokens/refresh`
through operator-configured host programs. These are automatic connection services;
credentials never become browser questions or transcript items. No provider runs by
default, and no attestation capability is advertised without its provider configured.

Configure the host programs in Giskard's `config.toml`, then restart Giskard:

```toml
[harness]
attestation_provider_command = ["/absolute/path/to/trusted-attestation-provider"]
external_auth_provider_command = ["/absolute/path/to/trusted-auth-provider"]
```

Each array is executable plus literal arguments. Giskard executes the absolute path
directly, without a shell, with `/` as the working directory and the server's environment.
These are trusted operator programs, not model-selected commands. Keep their executables
and configuration outside model-writable project directories; do not put credentials
in argv or Giskard configuration. Programs must finish within 8 seconds, produce at most
65,536 stdout bytes, and avoid starting detached descendants. Giskard kills the direct
child on provider timeout or transport shutdown; it does not manage a provider's descendant processes.
Stderr is discarded to prevent accidental credential logging. Provider failures are
reported with fixed diagnostic categories, never raw output. The operator-owned program
is responsible for safe private diagnostics and any credential storage.

## Program protocol (version 1)

One fresh process is started per request. Separate project app-servers can invoke the provider
concurrently; the host provider owns coordination of its credential lifecycle. Stdin contains
one JSON object followed by a
newline, then EOF. Stdout must contain exactly one JSON object and the program must exit
zero. Native response writes have a separate 1-second deadline so a blocked app-server pipe
cannot hang the owner indefinitely. There is no JSON-RPC envelope on this private provider interface.

Attestation input:

```json
{"version":1,"method":"attestation/generate","params":{}}
```

Output:

```json
{"token":"<fresh opaque attestation issued by your trusted provider>"}
```

Giskard negotiates `capabilities.requestAttestation = true` only when this command is
configured. The program must implement the actual upstream trusted attestation contract;
Giskard does not manufacture a token, substitute an access token, or provide a signer.
Activating this capability requires an authorized host signer/provider capable of issuing
an attestation accepted by the service. The app-server wire schema describes the opaque
token response but does not supply that signer or an enrollment API.

External auth initial input, after each app-server handshake:

```json
{"version":1,"method":"account/login/start","params":{"type":"chatgptAuthTokens"}}
```

Refresh input after Codex receives a backend 401:

```json
{"version":1,"method":"account/chatgptAuthTokens/refresh","params":{"reason":"unauthorized","previousAccountId":"account-id"}}
```

`previousAccountId` can be absent or null. The program must use the account hint to refresh
the appropriate account. Giskard rejects a refresh response naming a different account when
a nonempty previous account hint was supplied. Both operations return:

```json
{"accessToken":"<host-owned ChatGPT token>","chatgptAccountId":"account-id","chatgptPlanType":"business"}
```

`chatgptPlanType` is optional/null. Required strings must be nonempty. Extra provider output
fields are discarded; only the native response fields are forwarded. Giskard sends initial
tokens using `account/login/start` with `type: "chatgptAuthTokens"` and answers subsequent
refresh requests using the same program. A configured initial login failure prevents that
project's harness from starting; it never silently falls back to a different account.
A runtime provider failure becomes a native JSON-RPC error; unconfigured unsolicited
requests also receive an explicit error. Native Codex reports the resulting operation
failure through its existing error flow.

The host application owns user consent, token acquisition, account selection, refresh,
revocation and storage. Giskard does not extract Codex auth files or initiate an OAuth flow
on the host's behalf. Configuring the program explicitly selects external auth for every
project app-server in this Giskard process. Leaving it empty preserves Codex-managed login.
Tokens are handled in memory and forwarded over app-server stdin, never persisted by
Giskard. Native app-server behavior and host provider storage remain their responsibility.

## Protocol compatibility and delivery

[Official Codex app-server documentation](https://developers.openai.com/codex/app-server/)
documents host-owned external ChatGPT login and refresh as experimental, requiring
`experimentalApi = true`. Giskard already negotiates that capability. The installed native
schema examined for this change still labels `chatgptAuthTokens` internal/unstable; deployments
must use a Codex build and authorized host provider supporting the documented external flow.
This implementation does not claim a bundled host auth or attestation provider.

The single task owning the transport answers these services while awaiting client RPC
responses as well as during normal message reads. Ordinary frames received during an RPC
wait remain ordered in a bounded deferred queue. Overflow is fatal, rather than silently
losing lifecycle events. Cancelling an individual message read (for example for an active-turn
timer tick) retains the exact in-flight provider and response-write future. The next task-owned
read resumes it without rerunning the provider or enqueueing the reply again. Provider deadlines
continue across those pauses. Transport shutdown drops the future and kills its direct child.
A failed or timed-out response write poisons the connection, rather than retrying credential
generation or an uncertain write. Transport failure can leave response delivery uncertain.

Tests use only local stub programs and in-memory app-server pipes. They verify protocol
shapes, initial login plus refresh, request correlation, response-before-RPC completion,
ordered ordinary delivery, repeated read cancellation without reexecution, provider shutdown,
missing-provider errors, invalid output,
nonzero exit, bounded output, and timeout. No live credentials or signing services are used.
