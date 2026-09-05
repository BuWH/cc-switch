//! Keep Copilot's native Responses identities stable for the lifetime of a stream.
//!
//! Copilot can return a different opaque ID at every stage of the same output
//! item. Codex then treats the streamed message and the completed message as two
//! items. Correlate by output_index, retaining the first upstream ID (not a
//! synthetic ID) so completed items can still be replayed to the provider.

use std::collections::HashMap;

use bytes::{Bytes, BytesMut};
use futures::stream::{Stream, StreamExt};
use serde_json::Value;

use crate::proxy::{hyper_client::MAX_RESPONSE_BODY_BYTES, sse::strip_sse_field};

#[derive(Default)]
struct ResponseIds {
    response_id: Option<String>,
    item_ids: HashMap<u64, String>,
}

impl ResponseIds {
    fn normalize(&mut self, event: &mut Value) -> bool {
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            return false;
        };
        if !event_type.starts_with("response.") {
            return false;
        }

        let mut changed = false;
        if let Some(index) = event.get("output_index").and_then(Value::as_u64) {
            if let Some(id) = event.pointer_mut("/item/id") {
                changed |= normalize_item_id(&mut self.item_ids, index, id);
            }
            if let Some(id) = event.get_mut("item_id") {
                changed |= normalize_item_id(&mut self.item_ids, index, id);
            }
        }

        if let Some(response) = event.get_mut("response") {
            if let Some(id) = response.get_mut("id") {
                if let Some(value) = id.as_str().filter(|value| !value.is_empty()) {
                    let canonical = self.response_id.get_or_insert_with(|| value.to_owned());
                    if value != canonical {
                        *id = Value::String(canonical.clone());
                        changed = true;
                    }
                }
            }
            if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
                for (index, item) in output.iter_mut().enumerate() {
                    if let Some(id) = item.get_mut("id") {
                        changed |= normalize_item_id(&mut self.item_ids, index as u64, id);
                    }
                }
            }
        }
        changed
    }

    fn rewrite_frame(&mut self, frame: Bytes) -> Bytes {
        let Ok(text) = std::str::from_utf8(&frame) else {
            return frame;
        };
        let data = text
            .lines()
            .filter_map(|line| strip_sse_field(line, "data"))
            .collect::<Vec<_>>()
            .join("\n");
        let Ok(mut event) = serde_json::from_str::<Value>(&data) else {
            return frame;
        };
        if !self.normalize(&mut event) {
            return frame;
        }

        // Replace only the data field, preserving SSE id/retry/comment fields
        // and the original framing. Multiline data becomes one JSON data line.
        let serialized = event.to_string();
        let mut output = String::with_capacity(text.len());
        let mut wrote_data = false;
        for line in text.split_inclusive('\n') {
            if strip_sse_field(line.trim_end_matches(['\r', '\n']), "data").is_some() {
                if !wrote_data {
                    output.push_str("data: ");
                    output.push_str(&serialized);
                    if line.ends_with("\r\n") {
                        output.push_str("\r\n");
                    } else if line.ends_with('\n') {
                        output.push('\n');
                    }
                    wrote_data = true;
                }
            } else {
                output.push_str(line);
            }
        }
        Bytes::from(output)
    }
}

fn normalize_item_id(ids: &mut HashMap<u64, String>, index: u64, id: &mut Value) -> bool {
    let Some(value) = id.as_str().filter(|value| !value.is_empty()) else {
        return false;
    };
    let canonical = ids.entry(index).or_insert_with(|| value.to_owned());
    if value == canonical {
        return false;
    }
    *id = Value::String(canonical.clone());
    true
}

fn frame_end(bytes: &[u8]) -> Option<usize> {
    let lf = bytes
        .windows(2)
        .position(|value| value == b"\n\n")
        .map(|p| p + 2);
    let crlf = bytes
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

pub(crate) fn normalize_copilot_responses_stream(
    stream: impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut ids = ResponseIds::default();
        let mut buffer = BytesMut::new();
        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    while let Some(end) = frame_end(&buffer) {
                        yield Ok(ids.rewrite_frame(buffer.split_to(end).freeze()));
                    }
                    if buffer.len() > MAX_RESPONSE_BODY_BYTES {
                        yield Err(std::io::Error::other("Copilot Responses SSE event exceeded the response body limit"));
                        return;
                    }
                }
                Err(error) => {
                    if !buffer.is_empty() {
                        yield Ok(buffer.split().freeze());
                    }
                    yield Err(error);
                    return;
                }
            }
        }
        if !buffer.is_empty() {
            yield Ok(ids.rewrite_frame(buffer.freeze()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;
    use serde_json::json;

    fn frame(event: &Value) -> Bytes {
        Bytes::from(format!(
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        ))
    }

    fn parse_events(bytes: &[u8]) -> Vec<Value> {
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .filter_map(|line| strip_sse_field(line, "data"))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }

    async fn normalize(chunks: Vec<Bytes>) -> Vec<u8> {
        normalize_copilot_responses_stream(futures::stream::iter(chunks.into_iter().map(Ok)))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .concat()
    }

    fn rotating_message_events() -> Vec<Value> {
        vec![
            json!({"type":"response.created","response":{"id":"response-start","output":[]}}),
            json!({"type":"response.in_progress","response":{"id":"response-progress","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message-start","type":"message","role":"assistant","status":"in_progress","phase":"commentary","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0,"item_id":"message-part","content_index":0,"part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"message-delta-a","content_index":0,"delta":"hello "}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"message-delta-b","content_index":0,"delta":"world"}),
            json!({"type":"response.output_text.done","output_index":0,"item_id":"message-text-done","content_index":0,"text":"hello world"}),
            json!({"type":"response.content_part.done","output_index":0,"item_id":"message-part-done","content_index":0,"part":{"type":"output_text","text":"hello world"}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"message-done","type":"message","role":"assistant","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"hello world"}]}}),
            json!({"type":"response.completed","response":{"id":"response-done","status":"completed","output":[{"id":"message-snapshot","type":"message","content":[{"type":"output_text","text":"hello world"}]}],"usage":{"input_tokens":3,"output_tokens":2}}}),
        ]
    }

    #[tokio::test]
    async fn copilot_rotating_ids_render_one_message_and_preserve_payloads() {
        let input = rotating_message_events();
        let output = parse_events(&normalize(input.iter().map(frame).collect()).await);
        assert_eq!(output.len(), input.len());
        let mut visible_messages = HashMap::new();
        for (before, after) in input.iter().zip(&output) {
            let mut expected = before.clone();
            if expected.get("response").is_some() {
                expected["response"]["id"] = json!("response-start");
                if expected["response"]["output"]
                    .as_array()
                    .is_some_and(|v| !v.is_empty())
                {
                    expected["response"]["output"][0]["id"] = json!("message-start");
                }
            }
            if expected.get("item_id").is_some() {
                expected["item_id"] = json!("message-start");
            }
            if expected.get("item").is_some() {
                expected["item"]["id"] = json!("message-start");
            }
            assert_eq!(*after, expected, "only identity fields may change");

            match after["type"].as_str().unwrap() {
                "response.output_item.added" => {
                    visible_messages.insert(after["item"]["id"].as_str().unwrap(), String::new());
                }
                "response.output_text.delta" => {
                    visible_messages
                        .entry(after["item_id"].as_str().unwrap())
                        .or_default()
                        .push_str(after["delta"].as_str().unwrap());
                }
                "response.output_item.done" => {
                    visible_messages.insert(
                        after["item"]["id"].as_str().unwrap(),
                        after["item"]["content"][0]["text"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    );
                }
                _ => {}
            }
        }
        assert_eq!(visible_messages.len(), 1);
        assert_eq!(visible_messages["message-start"], "hello world");
    }

    #[test]
    fn copilot_interleaved_items_preserve_tools_reasoning_and_equal_text() {
        let mut ids = ResponseIds::default();
        for (index, kind) in [
            (0, "reasoning"),
            (1, "message"),
            (2, "function_call"),
            (3, "message"),
        ] {
            ids.normalize(&mut json!({"type":"response.output_item.added","output_index":index,"item":{"id":format!("start-{index}"),"type":kind}}));
        }
        for index in [3, 2, 0, 1] {
            let mut event = json!({
                "type":"response.output_item.done",
                "output_index":index,
                "item":{"id":"rotated","call_id":"call-unchanged","arguments":"{\"id\":\"argument-id\"}","encrypted_content":"opaque-thinking","phase":"commentary","content":[{"text":"same text"}]}
            });
            let mut expected = event.clone();
            expected["item"]["id"] = json!(format!("start-{index}"));
            assert!(ids.normalize(&mut event));
            assert_eq!(event, expected);
        }
        let mut delta = json!({"type":"response.function_call_arguments.delta","output_index":2,"item_id":"rotated-tool","delta":"{\"id\":"});
        ids.normalize(&mut delta);
        assert_eq!(delta["item_id"], "start-2");
        assert_eq!(delta["delta"], "{\"id\":");
        let mut reasoning = json!({"type":"response.reasoning_summary_text.delta","output_index":0,"item_id":"rotated-reasoning","summary_index":0,"delta":"summary"});
        ids.normalize(&mut reasoning);
        assert_eq!(reasoning["item_id"], "start-0");
        let mut completed = json!({"type":"response.completed","response":{"id":"r","output":[{"id":"end-0"},{"id":"end-1"},{"id":"end-2"},{"id":"end-3"}]}});
        ids.normalize(&mut completed);
        for index in 0..4 {
            assert_eq!(
                completed["response"]["output"][index]["id"],
                format!("start-{index}")
            );
        }
    }

    #[tokio::test]
    async fn copilot_stable_ids_and_unknown_frames_are_byte_exact() {
        let input = Bytes::from_static(
            b": heartbeat\r\n\r\n\
event: response.output_item.added\r\nid: transport-1\r\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"stable\",\"type\":\"message\"}}\r\n\r\n\
data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"item_id\":\"stable\",\"delta\":\"ok\"}\n\n\
data: {\"type\":\"other\",\"output_index\":0,\"item_id\":\"leave-alone\"}\n\n\
data: not-json\n\n\
data: [DONE]\n\n"
        );
        assert_eq!(normalize(vec![input.clone()]).await, input);
    }

    #[tokio::test]
    async fn copilot_fragmented_utf8_crlf_and_multiline_data_keep_transport_fields() {
        let first = frame(
            &json!({"type":"response.output_item.added","output_index":0,"item":{"id":"first"}}),
        );
        let text = "id: transport-2\r\n: keepalive\r\nevent: response.output_text.delta\r\nretry: 1000\r\ndata: {\"type\":\"response.output_text.delta\",\r\ndata: \"output_index\":0,\"item_id\":\"second\",\"delta\":\"\u{4f60}\u{597d}\"}\r\n\r\n";
        let mut chunks = vec![first];
        chunks.extend(
            text.as_bytes()
                .iter()
                .map(|byte| Bytes::copy_from_slice(&[*byte])),
        );
        let output = String::from_utf8(normalize(chunks).await).unwrap();
        assert!(output.contains("id: transport-2\r\n: keepalive\r\nevent: response.output_text.delta\r\nretry: 1000\r\n"));
        assert!(output.ends_with("\r\n\r\n"));
        let events = parse_events(output.as_bytes());
        assert_eq!(events[1]["item_id"], "first");
        assert_eq!(events[1]["delta"], "\u{4f60}\u{597d}");
    }

    #[tokio::test]
    async fn copilot_handles_data_only_missing_added_and_unterminated_last_event() {
        let input = Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\",\"output_index\":7,\"item_id\":\"first-delta\",\"delta\":\"ok\"}\n\n\
data: {\"type\":\"response.output_item.done\",\"output_index\":7,\"item\":{\"id\":\"done\"}}"
        );
        let output = normalize(vec![input]).await;
        assert!(!output.ends_with(b"\n"));
        assert_eq!(parse_events(&output)[1]["item"]["id"], "first-delta");
    }

    #[test]
    fn copilot_does_not_guess_missing_or_invalid_indices_or_ids() {
        let mut ids = ResponseIds::default();
        ids.normalize(
            &mut json!({"type":"response.output_text.delta","output_index":0,"item_id":"valid"}),
        );
        for mut event in [
            json!({"type":"response.output_text.delta","item_id":"no-index"}),
            json!({"type":"response.output_text.delta","output_index":-1,"item_id":"negative"}),
            json!({"type":"response.output_text.delta","output_index":"0","item_id":"string"}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":null}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":""}),
            json!({"type":"error","output_index":0,"item_id":"error-id"}),
        ] {
            let before = event.clone();
            assert!(!ids.normalize(&mut event));
            assert_eq!(event, before);
        }
    }

    #[tokio::test]
    async fn copilot_preserves_partial_bytes_and_transport_errors_without_completing() {
        let partial = Bytes::from_static(b"data: {\"unfinished\": \"\xff");
        let input = futures::stream::iter(vec![
            Ok(partial.clone()),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "upstream reset",
            )),
        ]);
        let output = normalize_copilot_responses_stream(input)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(output.len(), 2);
        assert_eq!(output[0].as_ref().unwrap(), &partial);
        assert_eq!(
            output[1].as_ref().unwrap_err().kind(),
            std::io::ErrorKind::ConnectionReset
        );
        assert_eq!(normalize(vec![partial.clone()]).await, partial);
    }

    #[tokio::test]
    async fn copilot_streams_do_not_share_identity_state() {
        for id in ["request-a", "request-b"] {
            let events = [
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":id}}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rotated"}}),
            ];
            let output = parse_events(&normalize(events.iter().map(frame).collect()).await);
            assert_eq!(output[1]["item"]["id"], id);
        }
    }
}
