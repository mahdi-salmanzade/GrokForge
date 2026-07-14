//! `GET /v1/models` types and startup validation.
//!
//! Configured model slugs are validated at startup because retired slugs are silently
//! redirected (and re-priced) by the API; a loud warning beats a surprise bill.

use serde::Deserialize;

/// A model advertised by the endpoint. Pricing is intentionally not inferred from this response:
/// it is unreliable across endpoints, and GrokForge does not yet ship a maintained price catalog.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub created: Option<i64>,
    #[serde(default)]
    pub owned_by: Option<String>,
    /// Alternate slugs that the API accepts for this model.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Present on some endpoints; used as a hint only.
    #[serde(default, alias = "context_length")]
    pub context_window: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelsResponse {
    pub data: Vec<ModelInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_shaped_models_list() {
        let body = r#"{"object":"list","data":[
            {"id":"grok-build-0.1","created":1,"owned_by":"xai","aliases":["grok-build-latest"]},
            {"id":"grok-4.5","created":2,"owned_by":"xai","context_length":500000}
        ]}"#;
        let parsed: ModelsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].id, "grok-build-0.1");
        assert_eq!(parsed.data[0].aliases, ["grok-build-latest"]);
        assert_eq!(parsed.data[1].context_window, Some(500_000));
    }
}
