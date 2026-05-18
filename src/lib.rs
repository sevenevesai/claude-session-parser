//! Streaming parser for [Claude Code](https://claude.com/claude-code) session JSONL files.
//!
//! Three modules, all `pub`:
//!
//! - [`usage`] — per-assistant-turn token/cost/tool entries (`UsageEntry`), session
//!   summaries (`SessionUsage`), hook executions (`HookExecution`), pricing tables,
//!   and project-path encoding compatible with Claude Code's `~/.claude/projects/`
//!   directory naming.
//! - [`chat`] — flat, timestamp-ordered `ChatEvent` stream for rendering the
//!   conversation (user / assistant / thinking / tool-use / tool-result blocks).
//! - [`activity`] — deterministic classifier that tags each assistant turn with one
//!   of 13 categories (Coding, Debugging, Refactor, Testing, Exploration, …).
//!
//! Parsing is streaming (`BufReader::lines()`), tolerates malformed lines, and
//! caps inputs at [`MAX_JSONL_BYTES`] (200MB) so a corrupted or runaway file
//! cannot block a caller.
//!
//! See the crate README for the trust-surface description, the JSONL format
//! notes (split content blocks, `<synthetic>` skip, hook formats), and a usage
//! example.

pub mod activity;
pub mod chat;
pub mod usage;

/// Hard cap on JSONL file size accepted by [`usage::parse_jsonl_entries`] and
/// [`chat::parse_chat_events`].
///
/// Parsing is streaming (`BufReader::lines()`), so memory is O(1) per line —
/// this cap only exists to keep a runaway / corrupted file from blocking the
/// caller indefinitely. Real agent sessions (Task-tool sub-agents) frequently
/// exceed 30MB, so the cap is set generously at 200MB.
pub const MAX_JSONL_BYTES: u64 = 200 * 1024 * 1024;

pub use activity::{classify, normalize_user_text, Activity};
pub use chat::{parse_chat_events, ChatEvent};
pub use usage::{
    context_window_for_model, encode_project_path, find_current_session_jsonl,
    parse_jsonl_entries, pricing_for_model, reduce_session, HookExecution, ModelPricing,
    ParsedJsonl, SessionUsage, UsageEntry,
};
