//! Provider observations retain the distinction between configured and requested models.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionObservation {
    pub(crate) native_session_id: String,
    pub(crate) configured_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) requested_model: Option<String>,
    pub(crate) native_pid: u32,
    pub(crate) request_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) codex: Option<CodexSessionMetadata>,
}

/// A Codex thread ID is not necessarily its session-tree ID.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexSessionMetadata {
    pub(crate) session_tree_id: String,
    pub(crate) model_provider: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_grok_observation_retains_its_serialized_shape() {
        let previous = serde_json::json!({
            "native_session_id":"fixture", "configured_model":"grok-fixture",
            "native_pid":123, "request_id":2
        });
        let decoded: SessionObservation = serde_json::from_value(previous.clone()).unwrap();
        assert_eq!(decoded.request_id, Some(2));
        assert_eq!(decoded.requested_model, None);
        assert_eq!(serde_json::to_value(decoded).unwrap(), previous);
    }
}
