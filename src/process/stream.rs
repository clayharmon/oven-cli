use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Parsed result from a Claude stream-json session.
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub cost_usd: f64,
    pub duration: Duration,
    pub turns: u32,
    pub output: String,
    pub session_id: String,
}

/// Events emitted by `claude --output-format stream-json`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StreamEvent {
    #[serde(rename = "system")]
    System {},
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default)]
        message: AssistantMessage,
    },
    #[serde(rename = "result")]
    Result { result: ResultData },
}

#[derive(Debug, Default, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ResultData {
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    num_turns: Option<u32>,
    #[serde(default)]
    session_id: String,
}

/// Parse a Claude stream-json output, extracting text, cost, and metadata.
///
/// Reads line by line, skipping malformed lines for forward compatibility.
pub async fn parse_stream<R: AsyncRead + Unpin>(reader: R) -> Result<StreamResult> {
    let buf = BufReader::new(reader);
    let mut lines = buf.lines();

    let mut output_parts: Vec<String> = Vec::new();
    let mut cost_usd = 0.0;
    let mut duration = Duration::ZERO;
    let mut turns = 0u32;
    let mut session_id = String::new();

    while let Some(line) = lines.next_line().await.context("reading stream line")? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Try to parse the line as a stream event; skip if it fails
        let event: StreamEvent = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };

        match event {
            StreamEvent::System { .. } => {}
            StreamEvent::Assistant { message } => {
                for block in message.content {
                    if let ContentBlock::Text { text } = block {
                        output_parts.push(text);
                    }
                }
            }
            StreamEvent::Result { result } => {
                if let Some(c) = result.cost_usd {
                    cost_usd = c;
                }
                if let Some(d) = result.duration_ms {
                    duration = Duration::from_millis(d);
                }
                if let Some(t) = result.num_turns {
                    turns = t;
                }
                session_id = result.session_id;
            }
        }
    }

    Ok(StreamResult { cost_usd, duration, turns, output: output_parts.join(""), session_id })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn parse_stream_never_panics_on_arbitrary_input(data in proptest::collection::vec(any::<u8>(), 0..500)) {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let _ = parse_stream(data.as_slice()).await;
            });
        }

        #[test]
        fn valid_result_event_extracts_cost(
            cost in 0.0..1000.0f64,
            duration_ms in 0..600_000u64,
            turns in 0..100u32,
        ) {
            let data = format!(
                r#"{{"type":"result","result":{{"cost_usd":{cost},"duration_ms":{duration_ms},"num_turns":{turns},"session_id":"s1"}}}}"#
            );
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let result = parse_stream(data.as_bytes()).await.unwrap();
                assert!((result.cost_usd - cost).abs() < 1e-6);
                assert_eq!(result.duration, std::time::Duration::from_millis(duration_ms));
                assert_eq!(result.turns, turns);
            });
        }

        #[test]
        fn multiple_text_blocks_concatenate(
            texts in proptest::collection::vec("[a-zA-Z0-9 ]{1,20}", 1..5),
        ) {
            let content_blocks: Vec<String> = texts.iter()
                .map(|t| format!(r#"{{"type":"text","text":"{t}"}}"#))
                .collect();
            let content_json = content_blocks.join(",");
            let data = format!(
                r#"{{"type":"assistant","message":{{"content":[{content_json}]}}}}
    {{"type":"result","result":{{"session_id":"s1"}}}}"#
            );
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let result = parse_stream(data.as_bytes()).await.unwrap();
                let expected: String = texts.into_iter().collect();
                assert_eq!(result.output, expected);
            });
        }
    }

    fn stream_fixture() -> &'static str {
        r#"{"type":"system","subtype":"init","session_id":"sess-123"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Hello "},{"type":"text","text":"world"}]}}
{"type":"result","result":{"cost_usd":2.50,"duration_ms":15000,"num_turns":5,"session_id":"sess-123"}}
"#
    }

    #[tokio::test]
    async fn parse_valid_stream() {
        let reader = stream_fixture().as_bytes();
        let result = parse_stream(reader).await.unwrap();

        assert_eq!(result.output, "Hello world");
        assert!((result.cost_usd - 2.50).abs() < f64::EPSILON);
        assert_eq!(result.duration, Duration::from_millis(15000));
        assert_eq!(result.turns, 5);
        assert_eq!(result.session_id, "sess-123");
    }

    #[tokio::test]
    async fn parse_empty_stream() {
        let reader = b"" as &[u8];
        let result = parse_stream(reader).await.unwrap();

        assert_eq!(result.output, "");
        assert!((result.cost_usd).abs() < f64::EPSILON);
        assert_eq!(result.turns, 0);
    }

    #[tokio::test]
    async fn parse_stream_with_missing_cost() {
        let data = r#"{"type":"result","result":{"session_id":"s1"}}
"#;
        let result = parse_stream(data.as_bytes()).await.unwrap();

        assert!((result.cost_usd).abs() < f64::EPSILON);
        assert_eq!(result.turns, 0);
    }

    #[tokio::test]
    async fn parse_stream_skips_malformed_lines() {
        let data = r#"not json at all
{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}
also bad {{{
{"type":"result","result":{"cost_usd":1.0,"session_id":"s1"}}
"#;
        let result = parse_stream(data.as_bytes()).await.unwrap();

        assert_eq!(result.output, "ok");
        assert!((result.cost_usd - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn parse_stream_handles_unknown_event_types() {
        // The serde tagged enum should fail to deserialize unknown types,
        // and we skip those lines gracefully
        let data = r#"{"type":"unknown_future_event","data":"whatever"}
{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}
{"type":"result","result":{"session_id":"s1"}}
"#;
        let result = parse_stream(data.as_bytes()).await.unwrap();
        assert_eq!(result.output, "hi");
    }

    #[tokio::test]
    async fn parse_stream_handles_other_content_blocks() {
        let data = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read"},{"type":"text","text":"done"}]}}
{"type":"result","result":{"session_id":"s1"}}
"#;
        let result = parse_stream(data.as_bytes()).await.unwrap();
        assert_eq!(result.output, "done");
    }
}
