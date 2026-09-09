//! Authoritative MCP form validation. The browser may render a schema as JSON, but acceptance
//! always checks the complete original schema before any response reaches the harness.
use giskard_core::error::HarnessError;
use giskard_core::server_request::{ServerRequest, ServerRequestResponse};
use serde_json::Value;

struct NoExternalSchemas;
impl jsonschema::Retrieve for NoExternalSchemas {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!("External form schema references are disabled: {uri}").into())
    }
}

pub(crate) fn validate_response(
    request: &ServerRequest,
    response: &ServerRequestResponse,
) -> Result<(), HarnessError> {
    if request.method != "mcpServer/elicitation/request" {
        return Ok(());
    }
    let ServerRequestResponse::Result { value } = response else {
        return Ok(());
    };
    let invalid = |message: String| HarnessError::Protocol(format!("MCP form: {message}"));
    match value.get("action").and_then(Value::as_str) {
        Some("decline" | "cancel") => return Ok(()),
        Some("accept") => {}
        _ => {
            return Err(invalid(
                "response requires accept, decline, or cancel action".into(),
            ));
        }
    }
    match request.params.get("mode").and_then(Value::as_str) {
        Some("url") => return Ok(()),
        Some("form" | "openai/form" | "openaiForm") => {}
        _ => return Err(invalid("unsupported elicitation mode".into())),
    }
    let schema = request
        .params
        .get("requestedSchema")
        .ok_or_else(|| invalid("request omitted requestedSchema".into()))?;
    let content = value
        .get("content")
        .ok_or_else(|| invalid("accepted response omitted content".into()))?;
    // Draft detection honors $schema. Default features are also disabled in Cargo.toml, but
    // an explicit retriever preserves this boundary if another dependency enables them later.
    let validator = jsonschema::options()
        .with_retriever(NoExternalSchemas)
        .should_validate_formats(true)
        .should_ignore_unknown_formats(false)
        .build(schema)
        .map_err(|error| invalid(format!("schema cannot be validated: {error}")))?;
    validator
        .validate(content)
        .map_err(|error| invalid(format!("{}: {error}", error.instance_path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use giskard_core::ids::ServerRequestId;
    use serde_json::json;

    fn request(schema: Value) -> ServerRequest {
        ServerRequest {
            id: ServerRequestId::new("form-test"),
            method: "mcpServer/elicitation/request".into(),
            params: json!({"mode":"openai/form", "requestedSchema":schema}),
            received_at: Utc::now(),
        }
    }
    fn accept(content: Value) -> ServerRequestResponse {
        ServerRequestResponse::result(json!({"action":"accept", "content":content}))
    }

    #[test]
    fn validates_local_references_composites_and_conditional_fields() {
        let request = request(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "$defs":{"count":{"type":"integer","minimum":1}},
            "type":"object", "properties":{"count":{"$ref":"#/$defs/count"}, "name":{"type":"string"}},
            "required":["count"], "additionalProperties":false,
            "if":{"properties":{"count":{"minimum":3}}}, "then":{"required":["name"]}
        }));
        assert!(validate_response(&request, &accept(json!({"count":2}))).is_ok());
        assert!(validate_response(&request, &accept(json!({"count":3,"name":"daily"}))).is_ok());
        for value in [
            json!({"count":0}),
            json!({"count":3}),
            json!({"count":1.5}),
            json!({"count":1,"other":true}),
        ] {
            assert!(validate_response(&request, &accept(value)).is_err());
        }
    }

    #[test]
    fn honors_schema_draft_and_array_constraints() {
        let request = request(
            json!({"$schema":"http://json-schema.org/draft-07/schema#", "oneOf":[
                {"type":"array", "items":{"type":"boolean"}, "minItems":1, "uniqueItems":true},
                {"type":"number", "exclusiveMinimum":0, "multipleOf":0.5}
            ]}),
        );
        for value in [json!([true, false]), json!(0.5)] {
            assert!(validate_response(&request, &accept(value)).is_ok());
        }
        for value in [
            json!([]),
            json!([true, true]),
            json!([1]),
            json!(0),
            json!(0.3),
        ] {
            assert!(validate_response(&request, &accept(value)).is_err());
        }
    }

    #[test]
    fn external_references_invalid_schemas_and_bad_content_fail_without_io() {
        for schema in [
            json!({"$ref":"file:///etc/passwd"}),
            json!({"$ref":"https://example.com/schema"}),
            json!({"type":7}),
            json!({"$schema":"https://example.com/custom-dialect"}),
            json!({"type":"string","format":"unknown-format"}),
        ] {
            let request = request(schema);
            assert!(validate_response(&request, &accept(json!({}))).is_err());
            for action in ["decline", "cancel"] {
                assert!(
                    validate_response(
                        &request,
                        &ServerRequestResponse::result(json!({"action":action,"content":null}))
                    )
                    .is_ok()
                );
            }
        }
        assert!(validate_response(&request(json!(false)), &accept(json!({}))).is_err());
        assert!(
            validate_response(
                &request(json!({})),
                &ServerRequestResponse::result(json!({"action":"accept"}))
            )
            .is_err()
        );
    }
}
