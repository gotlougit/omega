//! Protocol between `omega-sh-client` and `omega-sh`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OmegaRequest {
    pub id: String,
    pub tool: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OmegaResponse {
    pub id: String,
    pub result: OmegaToolResult,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OmegaToolResult {
    #[serde(flatten)]
    pub content: OmegaContent,
    pub is_error: bool,
}

/// Binary response payloads are base64 strings on the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum OmegaContent {
    Text {
        data: String,
    },
    Image {
        data: String,
        media_type: String,
    },
    Document {
        data: String,
        media_type: String,
        description: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_fixture_keeps_existing_shape() {
        let fixture =
            r#"{"id":"1","tool":"Read","args":{"file_path":"a"},"session":"s","dir":"/tmp"}"#;
        let request: OmegaRequest = serde_json::from_str(fixture).unwrap();
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::from_str::<Value>(fixture).unwrap()
        );
    }

    #[test]
    fn absent_request_hints_remain_absent() {
        let request = OmegaRequest {
            id: "1".into(),
            tool: "Read".into(),
            args: serde_json::json!({}),
            session: None,
            dir: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({"id":"1","tool":"Read","args":{}})
        );
    }

    #[test]
    fn response_fixture_keeps_flattened_content_shape() {
        let fixture = r#"{"id":"1","result":{"type":"Document","data":"AA==","media_type":"application/octet-stream","description":"blob","is_error":false}}"#;
        let response: OmegaResponse = serde_json::from_str(fixture).unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::from_str::<Value>(fixture).unwrap()
        );
    }
}
