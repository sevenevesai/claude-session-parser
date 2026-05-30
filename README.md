# Streaming parser for Claude Code session JSONL files

[<img alt="github" src="https://img.shields.io/badge/github-sevenevesai/claude--session--parser-8da0cb?style=for-the-badge&labelColor=555555&logo=github" height="20">](https://github.com/sevenevesai/claude-session-parser)
[<img alt="crates.io" src="https://img.shields.io/crates/v/claude-session-parser.svg?style=for-the-badge&color=fc8d62&logo=rust" height="20">](https://crates.io/crates/claude-session-parser)

Pulls structured token-usage and chat-event data out of the `.jsonl` files
[Claude Code](https://claude.com/claude-code) writes to
`~/.claude/projects/<encoded-path>/<session-id>.jsonl`. Streaming,
read-only, no network, no subprocess, no writes.

<br>

## Install

```toml
[dependencies]
claude-session-parser = "0.2"
```

<br>

## Usage

```rust
use std::path::Path;
use claude_session_parser::{parse_jsonl_entries, parse_chat_events, reduce_session};

let path = Path::new("/path/to/session.jsonl");

// Usage / billing view: one UsageEntry per assistant turn
let parsed = parse_jsonl_entries(path)?;
let summary = reduce_session(&parsed.entries, "claude-opus-4-7", "session-id");
if let Some(s) = summary {
    println!(
        "turns={} cost=${:.4} cache_hit={:.1}%",
        s.message_count, s.estimated_cost_usd, s.cache_hit_pct
    );
}

// Chat view: flat, timestamp-ordered event stream
let events = parse_chat_events(path)?;
for evt in events {
    println!("{:?}", evt);
}
# Ok::<(), std::io::Error>(())
```

<br>

## API

| Function | What it returns |
|----------|----------------|
| `parse_jsonl_entries(path)` | `Vec<UsageEntry>` — one per assistant message (tokens, model, tool uses, cache stats, activity class, hooks) |
| `parse_chat_events(path)` | `Vec<ChatEvent>` — flat, timestamp-ordered user / assistant / thinking / tool-use / tool-result events |
| `reduce_session(entries, model, session_id)` | `Option<SessionUsage>` — totals, cost, context-window %, cache-hit rate |
| `cost_for_entry(&UsageEntry)` | `f64` — canonical per-turn cost (USD); tiered cache writes, fast-mode multiplier, web-search fee |
| `classify(...)` | `Activity` — deterministic activity tag (13 categories: Coding, Debugging, Refactor, Testing, Exploration, …). No LLM, no regex. |
| `encode_project_path(path)` | `String` — replicates Claude Code's `~/.claude/projects/` directory-name encoding |
| `find_current_session_jsonl(project_root)` | Active session JSONL for a project from Claude Code's `sessions/` registry |

<br>

## Trust surface

> *"If a tool uses this crate to read my `~/.claude/` JSONL files, what does
> it touch on my machine?"*

The full surface is the functions above plus the data types they return.
Every file read is bounded (`MAX_JSONL_BYTES = 200 MB`), streaming
(`BufReader::lines()`, O(1) memory per line), and read-only. No network.
No subprocess. No filesystem writes. ~2600 lines of source.

<br>

## JSONL format notes (load-bearing for correctness)

- **Split content blocks.** Claude Code 2.1+ writes one JSONL row per content
  block of an assistant message (thinking, then text, then each `tool_use`),
  all sharing the same `message.id`. The usage parser dedupes billing fields
  by id (they repeat identically) but unions content fields across rows; the
  chat parser keys dedup on `(message_id + serialized_content)`. Dedup by id
  alone silently drops every block after the first.
- **`<synthetic>` model rows** are skipped — internal routing entries, not
  real LLM turns.
- **Tiered cache / fast mode / web search.** Cache writes split into 5-minute
  (1.25× input) and 1-hour (2× input) tiers via
  `usage.cache_creation.ephemeral_{5m,1h}_input_tokens`; fast mode
  (`usage.speed == "fast"`) multiplies the token line (Opus 4.6/4.7 = 6×,
  4.8 = 2×); `usage.server_tool_use.web_search_requests` bills $0.01 flat each.
  `cost_for_entry` accounts for all three — pricing only input/output/cache_read
  undercounts 1h-cache-heavy turns by ~38%.
- **Hook executions** are parsed from both the legacy `attachment` format and
  the v2.1.119+ `system.subtype = "*_hook_summary"` format.

<br>

## Scope

- No file watching, no incremental parsing, no cache layer. Layer that on
  top with e.g. [file-parse-cache](https://crates.io/crates/file-parse-cache).
- No I/O beyond reading the JSONL file you point it at.
- No async runtime, no GUI coupling, no build-system coupling.

<br>

#### License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>
