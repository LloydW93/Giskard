# Context capacity and pricing policy

A model descriptor preserves `advertised_context_window` separately from its configured or
runtime context default. Discovery uses `max_context_window` when present, otherwise
`context_window`; an advertised `max_input_tokens` further bounds the selectable request capacity.
For example, a 1,050,000 total context with a 922,000 input maximum permits at most 922,000 input
context tokens. A smaller provider `context_window` default does not erase its larger maximum.
Config metadata overrides the ordinary descriptor window but retains this remote capacity.
A limited session's token-usage events describe that session, never a new model maximum.

`ModelDescriptor::maximum_session_context_window()` returns the positive advertised capacity,
falling back to the configured descriptor window or the conservative 128,000-token value.
`default_session_context_window()` takes the smaller of that capacity and the known non-premium
input threshold. Unknown model identifiers have no inferred pricing threshold.

The explicit registry in `giskard-core/src/model.rs` was checked against official OpenAI
documentation on 2026-09-09. The following models have a **272,000 input token** non-premium
boundary (decimal, not 272 × 1024). Prompts above it enter the higher price tier:

- [GPT-6 Astra](https://developers.openai.com/api/docs/models/gpt-6-astra)
- [GPT-5.6 Sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol)
- [GPT-5.6 Terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra)
- [GPT-5.6 Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna)
- [GPT-5.5](https://developers.openai.com/api/docs/models/gpt-5.5), including `gpt-5.5-2026-04-23`
- [GPT-5.5 Pro](https://developers.openai.com/api/docs/models/gpt-5.5-pro), including `gpt-5.5-pro-2026-04-23`; threshold from the [pricing table](https://developers.openai.com/api/docs/pricing)
- [GPT-5.4](https://developers.openai.com/api/docs/models/gpt-5.4), including `gpt-5.4-2026-03-05`
- [GPT-5.4 Pro](https://developers.openai.com/api/docs/models/gpt-5.4-pro), including `gpt-5.4-pro-2026-03-05`

Only those exact identifiers match. Mini/nano variants, provider-prefixed aliases, and unknown
future snapshots are not assigned a price boundary merely because their names share a prefix.
The threshold is a default context policy, not a billing guarantee: provider pricing can differ,
and tools, new input, or native context accounting can add tokens to a request.
