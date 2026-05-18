use chrono::DateTime;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Flat, timestamp-ordered event stream extracted from a Claude Code session's
/// JSONL. Produced by `parse_chat_events` and rendered by the frontend chat
/// view. This is a read-only projection of the same file used by the usage
/// tracker — the live PTY remains the input path.
///
/// Pairing of `ToolUse` → `ToolResult` is left to the frontend via `tool_use_id`
/// so the event stream stays flat and cheap to serialize.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase", tag = "kind")]
pub enum ChatEvent {
    /// A real user turn (keyboard input). Does NOT include `tool_result`
    /// entries — those are emitted as `ToolResult` below.
    #[serde(rename = "user")]
    UserText {
        timestamp_ms: u64,
        text: String,
    },
    /// Assistant text block. One `ChatEvent::AssistantText` per text block in
    /// the assistant message; turns with no text block emit none.
    #[serde(rename = "assistant")]
    AssistantText {
        timestamp_ms: u64,
        message_id: String,
        model: String,
        text: String,
    },
    /// Extended-thinking block (Opus). Collapsed by default in the UI.
    #[serde(rename = "thinking")]
    Thinking {
        timestamp_ms: u64,
        message_id: String,
        model: String,
        text: String,
    },
    /// Tool invocation by the assistant. `input_json` is the raw JSON of the
    /// `input` field, serialized as a string — the frontend decides whether to
    /// pretty-print, truncate, or tree-render.
    #[serde(rename = "tool-use")]
    ToolUse {
        timestamp_ms: u64,
        message_id: String,
        tool_use_id: String,
        name: String,
        input_json: String,
    },
    /// Tool result injected by the harness. Pairs with a `ToolUse` by
    /// `tool_use_id`. `content` is the text payload (stringified if the JSONL
    /// stored it as a block array).
    #[serde(rename = "tool-result")]
    ToolResult {
        timestamp_ms: u64,
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

use super::MAX_JSONL_BYTES;

/// Parse a session JSONL into a flat `Vec<ChatEvent>` for chat-view rendering.
///
/// Mirrors `parse_jsonl_entries`'s skip rules:
/// - Malformed lines silently skipped.
/// - `model: "<synthetic>"` assistant messages skipped (internal routing, no
///   real turn).
/// - `type:"user"` with tool-result-only content is classified as `ToolResult`,
///   not `UserText`.
/// - Assistant messages dedup by `message.id` (Claude Code writes 2-3 streaming
///   chunks with identical payloads).
pub fn parse_chat_events(path: &Path) -> std::io::Result<Vec<ChatEvent>> {
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_JSONL_BYTES {
        tracing::warn!(
            path = %path.display(),
            size = meta.len(),
            "chat: skipping JSONL larger than {}MB",
            MAX_JSONL_BYTES / 1024 / 1024
        );
        return Ok(Vec::new());
    }

    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    // Dedup key is (message_id + content-serialized). Claude Code splits a
    // single assistant message across MULTIPLE JSONL rows — one per completed
    // content block (thinking, then text, then tool_use, …) — all sharing the
    // same `message.id` but carrying DIFFERENT content arrays. Deduping by
    // message.id alone (as usage.rs does for billing) would drop every block
    // after the first, which is why the chat view was missing assistant
    // replies. Including the content string in the key keeps distinct blocks
    // while still collapsing any true exact-duplicate rows if a Claude Code
    // version emits them.
    let mut seen_assistant: HashSet<String> = HashSet::new();
    let mut out: Vec<ChatEvent> = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let raw: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp_ms = raw
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp_millis().max(0) as u64)
            .unwrap_or(0);

        match entry_type {
            "user" => {
                let msg = match raw.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let content = match msg.get("content") {
                    Some(c) => c,
                    None => continue,
                };
                push_user_events(content, timestamp_ms, &mut out);
            }
            "assistant" => {
                let msg = match raw.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let mid = match msg.get("id").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let model = msg
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if model == "<synthetic>" {
                    continue;
                }
                let content = match msg.get("content") {
                    Some(c) => c,
                    None => continue,
                };
                // Key on (message_id + serialized content) — see comment at
                // `seen_assistant` declaration for why id alone is wrong here.
                let content_sig = serde_json::to_string(content).unwrap_or_default();
                let dedup_key = format!("{}|{}", mid, content_sig);
                if !seen_assistant.insert(dedup_key) {
                    continue;
                }
                push_assistant_events(content, timestamp_ms, &mid, &model, &mut out);
            }
            _ => {
                // Sidecar types (file-history-snapshot, permission-mode, …)
                // are ignored for chat rendering.
            }
        }
    }

    Ok(out)
}

/// A user JSONL row's `content` field may be:
/// - A plain string → emit one `UserText`.
/// - An array of blocks. Each block can be:
///   - `{"type":"text","text":"..."}` → `UserText`
///   - `{"type":"tool_result","tool_use_id":"...","content":"...","is_error":bool}` → `ToolResult`
///     (content can itself be a string or an array of text blocks).
fn push_user_events(content: &serde_json::Value, timestamp_ms: u64, out: &mut Vec<ChatEvent>) {
    match content {
        serde_json::Value::String(s) => {
            if !s.is_empty() {
                out.push(ChatEvent::UserText {
                    timestamp_ms,
                    text: s.clone(),
                });
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match t {
                    "text" => {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                out.push(ChatEvent::UserText {
                                    timestamp_ms,
                                    text: text.to_string(),
                                });
                            }
                        }
                    }
                    "tool_result" => {
                        let tool_use_id = item
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let is_error = item
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let content_str = stringify_result_content(item.get("content"));
                        out.push(ChatEvent::ToolResult {
                            timestamp_ms,
                            tool_use_id,
                            content: content_str,
                            is_error,
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Extract assistant content blocks into a sequence of `AssistantText`,
/// `Thinking`, and `ToolUse` events — preserving the source order inside the
/// message so the UI renders text → tool_use → text naturally.
fn push_assistant_events(
    content: &serde_json::Value,
    timestamp_ms: u64,
    message_id: &str,
    model: &str,
    out: &mut Vec<ChatEvent>,
) {
    let arr = match content.as_array() {
        Some(a) => a,
        None => return,
    };

    for block in arr {
        let block_type = match block.get("type").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => continue,
        };
        match block_type {
            "text" => {
                if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        out.push(ChatEvent::AssistantText {
                            timestamp_ms,
                            message_id: message_id.to_string(),
                            model: model.to_string(),
                            text: text.to_string(),
                        });
                    }
                }
            }
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        out.push(ChatEvent::Thinking {
                            timestamp_ms,
                            message_id: message_id.to_string(),
                            model: model.to_string(),
                            text: text.to_string(),
                        });
                    }
                }
            }
            "tool_use" => {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_use_id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_json = block
                    .get("input")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                out.push(ChatEvent::ToolUse {
                    timestamp_ms,
                    message_id: message_id.to_string(),
                    tool_use_id,
                    name,
                    input_json,
                });
            }
            _ => {}
        }
    }
}

/// Tool-result `content` can be a string or an array of text blocks. Collapse
/// to a single string; the frontend truncates for display.
fn stringify_result_content(content: Option<&serde_json::Value>) -> String {
    let v = match content {
        Some(v) => v,
        None => return String::new(),
    };
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts: Vec<String> = Vec::new();
            for item in arr {
                let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if t == "text" {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        parts.push(text.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_jsonl(lines: &[&str]) -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{}", l).unwrap();
        }
        (tmp, path)
    }

    #[test]
    fn parses_plain_user_and_assistant_text() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"user","timestamp":"2026-04-14T08:00:00.000Z","message":{"content":"hi claude"}}"#,
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"msg_01","model":"claude-opus-4-6","content":[{"type":"text","text":"hello!"}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            ChatEvent::UserText { text, .. } => assert_eq!(text, "hi claude"),
            _ => panic!("expected UserText"),
        }
        match &events[1] {
            ChatEvent::AssistantText { text, model, .. } => {
                assert_eq!(text, "hello!");
                assert_eq!(model, "claude-opus-4-6");
            }
            _ => panic!("expected AssistantText"),
        }
    }

    #[test]
    fn dedups_exact_duplicate_assistant_rows() {
        // Exact-duplicate rows (same id, same content) collapse to one event.
        let line = r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"msg_01","model":"claude-opus-4-6","content":[{"type":"text","text":"hello"}]}}"#;
        let (_tmp, path) = write_jsonl(&[line, line, line]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn keeps_distinct_blocks_sharing_message_id() {
        // Regression test: Claude Code splits a single assistant message into
        // separate JSONL rows per completed content block (thinking → text →
        // tool_use). All rows share message.id but carry DIFFERENT content.
        // Deduping by message.id alone would drop the text and tool_use rows.
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"msg_01","model":"claude-haiku-4-5","content":[{"type":"thinking","thinking":"hmm"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:02.000Z","message":{"id":"msg_01","model":"claude-haiku-4-5","content":[{"type":"text","text":"hello world"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:03.000Z","message":{"id":"msg_01","model":"claude-haiku-4-5","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"/x"}}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 3, "should keep thinking + text + tool_use distinct blocks");
        assert!(matches!(events[0], ChatEvent::Thinking { .. }));
        match &events[1] {
            ChatEvent::AssistantText { text, .. } => assert_eq!(text, "hello world"),
            _ => panic!("expected AssistantText"),
        }
        assert!(matches!(events[2], ChatEvent::ToolUse { .. }));
    }

    #[test]
    fn skips_synthetic_model() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"m1","model":"<synthetic>","content":[{"type":"text","text":"internal"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:02.000Z","message":{"id":"m2","model":"claude-sonnet-4-6","content":[{"type":"text","text":"real"}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChatEvent::AssistantText { text, .. } => assert_eq!(text, "real"),
            _ => panic!("expected AssistantText"),
        }
    }

    #[test]
    fn emits_tool_use_and_pairs_with_tool_result() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"m1","model":"claude-sonnet-4-6","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"/tmp/x"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-04-14T08:00:02.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":"contents here","is_error":false}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            ChatEvent::ToolUse { name, tool_use_id, input_json, .. } => {
                assert_eq!(name, "Read");
                assert_eq!(tool_use_id, "tu_1");
                assert!(input_json.contains("file_path"));
            }
            _ => panic!("expected ToolUse"),
        }
        match &events[1] {
            ChatEvent::ToolResult { tool_use_id, content, is_error, .. } => {
                assert_eq!(tool_use_id, "tu_1");
                assert_eq!(content, "contents here");
                assert!(!is_error);
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn tool_result_handles_array_content() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"user","timestamp":"2026-04-14T08:00:02.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":[{"type":"text","text":"line1"},{"type":"text","text":"line2"}],"is_error":false}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChatEvent::ToolResult { content, .. } => {
                assert_eq!(content, "line1\nline2");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn tool_result_is_error_propagates() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"user","timestamp":"2026-04-14T08:00:02.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":"boom","is_error":true}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        match &events[0] {
            ChatEvent::ToolResult { is_error, .. } => assert!(*is_error),
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn user_array_with_text_emits_user_text() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"user","timestamp":"2026-04-14T08:00:00.000Z","message":{"content":[{"type":"text","text":"typed message"}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChatEvent::UserText { text, .. } => assert_eq!(text, "typed message"),
            _ => panic!("expected UserText"),
        }
    }

    #[test]
    fn thinking_block_emitted() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"m1","model":"claude-opus-4-6","content":[{"type":"thinking","thinking":"reasoning..."},{"type":"text","text":"answer"}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            ChatEvent::Thinking { text, .. } => assert_eq!(text, "reasoning..."),
            _ => panic!("expected Thinking"),
        }
        match &events[1] {
            ChatEvent::AssistantText { text, .. } => assert_eq!(text, "answer"),
            _ => panic!("expected AssistantText"),
        }
    }

    #[test]
    fn skips_malformed_lines() {
        let (_tmp, path) = write_jsonl(&[
            "not json at all",
            "",
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"m1","model":"claude-sonnet-4-6","content":[{"type":"text","text":"hi"}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn skips_sidecar_types() {
        let (_tmp, path) = write_jsonl(&[
            r#"{"type":"file-history-snapshot","timestamp":"2026-04-14T08:00:00.000Z"}"#,
            r#"{"type":"permission-mode","timestamp":"2026-04-14T08:00:00.000Z"}"#,
            r#"{"type":"assistant","timestamp":"2026-04-14T08:00:01.000Z","message":{"id":"m1","model":"claude-sonnet-4-6","content":[{"type":"text","text":"hi"}]}}"#,
        ]);
        let events = parse_chat_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn serializes_to_camel_case_with_kind_tag() {
        let evt = ChatEvent::ToolUse {
            timestamp_ms: 123,
            message_id: "m1".into(),
            tool_use_id: "tu_1".into(),
            name: "Read".into(),
            input_json: "{}".into(),
        };
        let s = serde_json::to_string(&evt).unwrap();
        assert!(s.contains("\"kind\":\"tool-use\""));
        assert!(s.contains("\"toolUseId\":\"tu_1\""));
        assert!(s.contains("\"messageId\":\"m1\""));
        assert!(s.contains("\"timestampMs\":123"));
    }
}
