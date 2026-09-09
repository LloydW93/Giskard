use serde::{Deserialize, Serialize};

/// A model identified by the pair (provider, model_id) plus optional reasoning effort.
///
/// The same model name on two providers is two distinct entries (spec §8.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Effort>,
    /// Optional advertised service tier, applied independently to each turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

/// A reasoning-effort level (model-dependent).
///
/// Reasoning efforts are open-ended and model-specific — the set a model accepts comes from the
/// harness's catalog (Codex's own `ReasoningEffort` is likewise a bare string), not a fixed list.
/// Giskard is a pass-through: it never branches on the value, it just carries the user's selection to
/// the harness. So this is a transparent string newtype, not a closed enum. Common values are
/// `minimal | low | medium | high | xhigh`, but any string a model advertises is valid.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Effort(pub String);

impl Effort {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Metadata describing a model, used by the UI and context gauge (spec §8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub provider: String,
    pub model: String,
    /// Token limit; drives the context gauge (§10.3).
    pub context_window: u32,
    /// Provider-advertised request capacity, independent of configured or runtime defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertised_context_window: Option<u32>,
    /// Whether the effort selector is shown (§8.5).
    pub supports_reasoning_effort: bool,
    /// The exact reasoning-effort levels this model advertises (e.g. from Codex's `model/list`),
    /// used to populate the effort selector. Empty means "unknown" — the UI falls back to the
    /// default effort set when `supports_reasoning_effort` is true.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Whether the harness marks this as the model to start from when nothing else is chosen
    /// (Codex's `model/list` `isDefault`). Used to seed a project's default model (§8.3); it is
    /// never a fallback for a thread that already has one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tiers: Option<Vec<ModelServiceTier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_service_tier: Option<String>,
    /// None means the harness has not reported supported input types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_version: Option<String>,
}

/// Service tier identifiers and labels are supplied by the model catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelServiceTier {
    pub id: String,
    pub name: String,
    pub description: String,
}

impl ModelRef {
    /// Returns the composite key "provider/model" used by the per-thread effort-retention map.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// Last verified against official OpenAI model/pricing documentation on 2026-09-09.
/// Exact identifiers avoid assigning a family's prices to mini/nano or unknown future variants.
/// See docs/context-window-policy.md for sources. Values are decimal tokens, not kibit units.
pub fn non_premium_context_window(model_id: &str) -> Option<u32> {
    match model_id {
        "gpt-6-astra"
        | "gpt-5.6-sol"
        | "gpt-5.6-terra"
        | "gpt-5.6-luna"
        | "gpt-5.5"
        | "gpt-5.5-2026-04-23"
        | "gpt-5.5-pro"
        | "gpt-5.5-pro-2026-04-23"
        | "gpt-5.4"
        | "gpt-5.4-2026-03-05"
        | "gpt-5.4-pro"
        | "gpt-5.4-pro-2026-03-05" => Some(272_000),
        _ => None,
    }
}

impl ModelDescriptor {
    /// A catalog maximum is never replaced by token-usage reports from a limited session.
    pub fn maximum_session_context_window(&self) -> u32 {
        self.advertised_context_window
            .filter(|value| *value > 0)
            .or(Some(self.context_window).filter(|value| *value > 0))
            .unwrap_or(Self::CONSERVATIVE_CONTEXT_WINDOW)
    }

    pub fn default_session_context_window(&self) -> u32 {
        let maximum = self.maximum_session_context_window();
        non_premium_context_window(&self.model).map_or(maximum, |threshold| maximum.min(threshold))
    }

    /// Conservative context window used when the model's size is unknown (spec §8.3 step 3).
    pub const CONSERVATIVE_CONTEXT_WINDOW: u32 = 128_000;

    /// A conservative fallback descriptor for an otherwise-unknown model (spec §8.3 step 3).
    /// A harness-reported runtime window replaces this fallback when one becomes available.
    pub fn conservative(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            context_window: Self::CONSERVATIVE_CONTEXT_WINDOW,
            advertised_context_window: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            display_name: None,
            is_default: false,
            service_tiers: None,
            default_service_tier: None,
            input_modalities: None,
            multi_agent_version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_defaults_respect_exact_pricing_boundaries_and_remote_capacity() {
        for model in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-5.5-2026-04-23",
            "gpt-5.5-pro-2026-04-23",
            "gpt-5.4",
            "gpt-5.4-pro",
            "gpt-5.4-2026-03-05",
            "gpt-5.4-pro-2026-03-05",
        ] {
            let mut descriptor = ModelDescriptor::conservative("provider", model);
            for maximum in [100_000, 271_999, 272_000, 272_001, 1_050_000] {
                descriptor.advertised_context_window = Some(maximum);
                assert_eq!(descriptor.maximum_session_context_window(), maximum);
                assert_eq!(
                    descriptor.default_session_context_window(),
                    maximum.min(272_000),
                    "{model}"
                );
            }
        }
        for model in [
            "gpt-5.4-mini",
            "gpt-5.4-nano",
            "gpt-5.6-cyber",
            "gpt-6-astra-custom",
            "other/gpt-6-astra",
            "unknown",
        ] {
            assert_eq!(non_premium_context_window(model), None);
            let mut descriptor = ModelDescriptor::conservative("provider", model);
            descriptor.advertised_context_window = Some(1_050_000);
            assert_eq!(descriptor.default_session_context_window(), 1_050_000);
        }
    }

    #[test]
    fn session_capacity_falls_back_without_inventing_remote_metadata() {
        let mut descriptor = ModelDescriptor::conservative("provider", "gpt-6-astra");
        assert_eq!(descriptor.advertised_context_window, None);
        assert_eq!(descriptor.default_session_context_window(), 128_000);
        descriptor.context_window = 400_000;
        assert_eq!(descriptor.default_session_context_window(), 272_000);
        descriptor.advertised_context_window = Some(0);
        descriptor.context_window = 0;
        assert_eq!(descriptor.maximum_session_context_window(), 128_000);
    }

    #[test]
    fn model_ref_key() {
        let m = ModelRef {
            provider: "openai".into(),
            model: "gpt-5.5".into(),
            reasoning_effort: None,
            service_tier: None,
        };
        assert_eq!(m.key(), "openai/gpt-5.5");
    }

    #[test]
    fn model_ref_equality_provider_significant() {
        let a = ModelRef {
            provider: "openai".into(),
            model: "gpt-5.5".into(),
            reasoning_effort: None,
            service_tier: None,
        };
        let b = ModelRef {
            provider: "cloudflare-litellm".into(),
            model: "gpt-5.5".into(),
            reasoning_effort: None,
            service_tier: None,
        };
        assert_ne!(a, b, "same model on different providers must be distinct");
    }

    #[test]
    fn effort_serde() {
        // Serializes transparently as its string, so persisted/wire values are unchanged from the
        // old enum ("xhigh"), and any model-defined value round-trips.
        let e = Effort::new("xhigh");
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, "\"xhigh\"");
        let back: Effort = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);

        // A value outside the old closed set is now valid and round-trips.
        let custom: Effort = serde_json::from_str("\"ultra\"").unwrap();
        assert_eq!(custom, Effort::new("ultra"));
        assert_eq!(custom.as_str(), "ultra");
    }

    #[test]
    fn conservative_fallback() {
        let d = ModelDescriptor::conservative("acme", "mystery-1");
        assert_eq!(
            d.context_window,
            ModelDescriptor::CONSERVATIVE_CONTEXT_WINDOW
        );
        assert!(!d.supports_reasoning_effort);
        assert_eq!(d.provider, "acme");
    }

    #[test]
    fn model_descriptor_reasoning_efforts_serde() {
        // Missing field deserializes to an empty vec (serde default), for older payloads.
        let missing: ModelDescriptor = serde_json::from_str(
            r#"{"provider":"p","model":"m","context_window":1000,"supports_reasoning_effort":false}"#,
        )
        .unwrap();
        assert!(missing.reasoning_efforts.is_empty());
        assert_eq!(missing.advertised_context_window, None);

        // An explicit empty array also deserializes to empty, and is omitted on serialize.
        let empty: ModelDescriptor = serde_json::from_str(
            r#"{"provider":"p","model":"m","context_window":1000,"supports_reasoning_effort":false,"reasoning_efforts":[]}"#,
        )
        .unwrap();
        assert!(empty.reasoning_efforts.is_empty());
        let json = serde_json::to_value(&empty).unwrap();
        assert!(
            json.get("reasoning_efforts").is_none(),
            "empty efforts are omitted (skip_serializing_if): {json}"
        );

        // A populated list serializes and round-trips.
        let mut d = ModelDescriptor::conservative("p", "m");
        d.reasoning_efforts = vec!["low".into(), "high".into()];
        d.advertised_context_window = Some(1_050_000);
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(
            json["reasoning_efforts"],
            serde_json::json!(["low", "high"])
        );
        let back: ModelDescriptor = serde_json::from_value(json).unwrap();
        assert_eq!(back.reasoning_efforts, vec!["low", "high"]);
        assert_eq!(back.advertised_context_window, Some(1_050_000));

        // The conservative constructor initializes the field empty.
        assert!(
            ModelDescriptor::conservative("p", "m")
                .reasoning_efforts
                .is_empty()
        );
    }

    #[test]
    fn model_ref_serde_roundtrip() {
        let m = ModelRef {
            provider: "openai".into(),
            model: "gpt-5.5".into(),
            reasoning_effort: Some(Effort::new("high")),
            service_tier: Some("future-tier".into()),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ModelRef = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }
}
