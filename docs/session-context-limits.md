# Session context limits

Scope checklist from the user request:

- done: Default each session to min(advertised model maximum, known non-premium input threshold).
- done: Preserve advertised maximum independently of selected and effective runtime windows.
- done: Store explicit larger limits per session, validated against that model's advertised maximum.
- done: Add the configuration to the existing top-right Context control, with reset/default and pending/error states.
- done: Apply the policy to native context/compaction configuration on creation, resume and subsequent turns, including native goal/queue launches.
- done: Preserve session isolation and model/provider scoping, handle active turns without interruption, and test persistence/reconnect/model changes.

No Giskard service stop/restart is part of implementation: the agent itself runs in that service.

The durable selection is a raw native limit. Giskard keeps it separate from both the model's
known capacity and the latest effective window reported by a running turn. The known capacity is
the lower of catalog metadata and the existing runtime gauge observation for this session and
provider/model. A reduced configured turn updates the current gauge without erasing the larger
natural capacity observed before the override. For example, a raw
272,000-token Codex limit normally yields a 258,400-token effective gauge after native headroom.
The control therefore describes budgeting and compaction behavior rather than guaranteeing a
pricing boundary for every possible tool-result burst.

Native Codex accepts these settings only when starting or cold-resuming a thread. Before admitted
input, compaction, goal activation, or queued work, Giskard verifies an idle primary is loaded with
the selected limit. A running turn, active goal, or nonempty queue leaves the new preference
pending; stopping or completing a goal remains possible while it is pending. Agent-owned children
are read-only and inherit the parent's raw limit when first created, clamped to their model's
maximum. An already-loaded child keeps the configuration it started with.

Changing effort or service tier preserves the override. Changing provider or model clears it so a
limit chosen against one advertised capacity cannot silently carry to another. If discovery later
shrinks the maximum, the applied selection is clamped to the current bound. Codex's native catalog
may supply `contextWindow` as the normal session limit and `maxContextWindow` as the configurable
ceiling. When it supplies neither, Giskard lets the first unconfigured turn establish capacity
through the same runtime value used by the gauge. It does not inject the conservative fallback
before that observation.
