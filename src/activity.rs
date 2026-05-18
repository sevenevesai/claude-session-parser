//! Deterministic classifier for Claude Code turns.
//!
//! Classifies a single assistant turn into one of 13 activity categories based
//! on the tools it invoked, any bash commands it ran, the preceding user
//! message text, and the amount of text it produced. No LLM calls, no regex —
//! just case-insensitive substring scanning. Mirrors the taxonomy from
//! CodeBurn (github.com/AgentSeal/codeburn) with minor naming adjustments.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activity {
    Coding,
    Debugging,
    Feature,
    Refactor,
    Testing,
    Exploration,
    Planning,
    Delegation,
    GitOps,
    Build,
    Brainstorm,
    Conversation,
    General,
}

// Tool name sets. These match the canonical Claude Code tool names plus a few
// legacy aliases. Comparison is case-sensitive — Claude Code emits canonical
// names, so this keeps the classifier O(n) and branch-friendly.
const EDIT_TOOLS: &[&str] = &[
    "Edit", "Write", "FileEditTool", "FileWriteTool", "NotebookEdit",
];
const READ_TOOLS: &[&str] = &[
    "Read", "Grep", "Glob", "FileReadTool", "GrepTool", "GlobTool",
];
const BASH_TOOLS: &[&str] = &["Bash", "BashTool", "PowerShellTool"];
const TASK_TOOLS: &[&str] = &[
    "TaskCreate", "TaskUpdate", "TaskGet", "TaskList", "TaskOutput",
    "TaskStop", "TodoWrite",
];
const SEARCH_TOOLS: &[&str] = &["WebSearch", "WebFetch", "ToolSearch"];

// User-message intent keywords. All compared against the lowercased snippet.
const DEBUG_KEYWORDS: &[&str] = &[
    "fix", "bug", "error", "broken", "failing", "crash", "issue",
    "debug", "traceback", "exception", "regression",
];
const FEATURE_KEYWORDS: &[&str] = &[
    "add", "create", "implement", "new feature", "build ", "introduce",
    "set up", "scaffold",
];
const REFACTOR_KEYWORDS: &[&str] = &[
    "refactor", "clean up", "rename", "reorganize", "simplify",
    "extract", "restructure", "dry up",
];
const BRAINSTORM_KEYWORDS: &[&str] = &[
    "brainstorm", "what if", "think about", "approach", "design",
    "ideation", "explore ideas",
];
const RESEARCH_KEYWORDS: &[&str] = &[
    "research", "investigate", "look into", "find out", "analyze",
    "review", "compare",
];

// Bash-command patterns. Matched as lowercased substrings of the raw command.
// Order matters when multiple match: first one wins via the dispatch order
// below (Testing → GitOps → Build → Install).
const TEST_PATTERNS: &[&str] = &[
    "pytest", "vitest", "jest", "mocha", " spec", "coverage",
    "npm test", "npm run test", "yarn test", "pnpm test", "cargo test",
    "go test", "rspec",
];
const GIT_PATTERNS: &[&str] = &[
    "git push", "git pull", "git commit", "git merge", "git rebase",
    "git checkout", "git branch", "git fetch", "git stash",
    "git reset", "git restore", "git tag", "git cherry-pick",
];
const BUILD_PATTERNS: &[&str] = &[
    "npm run build", "yarn build", "pnpm build", "webpack",
    "vite build", "rollup", "esbuild", "cargo build", "docker build",
    "docker compose", "pm2 ", "deploy",
];
const INSTALL_PATTERNS: &[&str] = &[
    "npm install", "npm i ", "yarn add", "pnpm install", "pnpm add",
    "pip install", "cargo add", "bundle install", "brew install",
];

fn any_in(tools: &[&str], needles: &[&str]) -> bool {
    tools.iter().any(|t| needles.contains(t))
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Truncate and lowercase a user message for keyword scanning.
///
/// 500 chars is generous — intent keywords almost always appear in the first
/// sentence or two. Truncating bounds the work per turn and the memory cost of
/// the rolling `last_user_text` variable in the parser.
pub fn normalize_user_text(raw: &str) -> String {
    let mut s: String = raw.chars().take(500).collect();
    s.make_ascii_lowercase();
    s
}

/// Classify a single assistant turn.
///
/// Arguments:
/// - `tools`: tool names invoked in this turn (from `tool_use` content blocks)
/// - `bash_cmds`: bash/shell command strings invoked (from `tool_use.input.command`)
/// - `user_msg`: the preceding user message text, already lowercased+truncated
///   via `normalize_user_text`. `None` if there is no paired user turn (rare —
///   happens for synthetic / SDK-initiated conversations).
/// - `text_chars`: number of text characters the assistant produced in this
///   turn (sum of `text` content blocks). Used to distinguish silent tool-only
///   turns from pure-text conversations.
pub fn classify(
    tools: &[&str],
    bash_cmds: &[&str],
    user_msg: Option<&str>,
    text_chars: u32,
) -> Activity {
    let user = user_msg.unwrap_or("");

    let has_edit = any_in(tools, EDIT_TOOLS);
    let has_bash = any_in(tools, BASH_TOOLS);
    let has_read = any_in(tools, READ_TOOLS);
    let has_search = any_in(tools, SEARCH_TOOLS);
    let has_task = any_in(tools, TASK_TOOLS);
    let has_agent = tools.contains(&"Agent");
    let has_plan = tools.contains(&"ExitPlanMode") || tools.contains(&"EnterPlanMode");
    let has_skill = tools.contains(&"Skill");

    // Delegation wins outright — Agent spawns imply sub-agent work regardless
    // of what else the turn touched.
    if has_agent {
        return Activity::Delegation;
    }

    // Planning: ExitPlanMode or todo-list tools without concrete edits.
    if has_plan || (has_task && !has_edit) {
        return Activity::Planning;
    }

    // Edit-driven turns: refine with user intent. Order matters — "fix the bug
    // in this refactor" reads as Debugging (the active intent) over Refactor.
    if has_edit {
        if contains_any(user, DEBUG_KEYWORDS) {
            return Activity::Debugging;
        }
        if contains_any(user, REFACTOR_KEYWORDS) {
            return Activity::Refactor;
        }
        if contains_any(user, FEATURE_KEYWORDS) {
            return Activity::Feature;
        }
        return Activity::Coding;
    }

    // Bash-only turns: classify by command shape. Testing first (tests can
    // invoke git/build scripts as side effects), then git, then build/install.
    if has_bash {
        let cmds: Vec<String> = bash_cmds.iter().map(|c| c.to_ascii_lowercase()).collect();
        let any_cmd_matches = |pats: &[&str]| cmds.iter().any(|c| contains_any(c, pats));

        if any_cmd_matches(TEST_PATTERNS) {
            return Activity::Testing;
        }
        if any_cmd_matches(GIT_PATTERNS) {
            return Activity::GitOps;
        }
        if any_cmd_matches(BUILD_PATTERNS) || any_cmd_matches(INSTALL_PATTERNS) {
            return Activity::Build;
        }
        return Activity::Exploration;
    }

    // Read / search / MCP with no edits: exploration.
    if has_read || has_search {
        return Activity::Exploration;
    }

    // Skill tool only and nothing else → general scaffolding.
    if has_skill {
        return Activity::General;
    }

    // No tools at all: infer from user intent, fall back to Conversation if
    // the assistant produced any text.
    if tools.is_empty() {
        if contains_any(user, BRAINSTORM_KEYWORDS) {
            return Activity::Brainstorm;
        }
        if contains_any(user, RESEARCH_KEYWORDS) {
            return Activity::Exploration;
        }
        if contains_any(user, DEBUG_KEYWORDS) {
            return Activity::Debugging;
        }
        if contains_any(user, FEATURE_KEYWORDS) {
            return Activity::Feature;
        }
        if text_chars > 0 {
            return Activity::Conversation;
        }
        return Activity::General;
    }

    Activity::General
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegation_wins_over_everything() {
        let tools = ["Agent", "Edit", "Bash"];
        let tools: Vec<&str> = tools.iter().copied().collect();
        assert_eq!(
            classify(&tools, &[], Some("fix the bug"), 100),
            Activity::Delegation
        );
    }

    #[test]
    fn plan_mode_classifies_as_planning() {
        let tools = ["ExitPlanMode"];
        assert_eq!(
            classify(&tools, &[], Some("add a feature"), 50),
            Activity::Planning
        );
    }

    #[test]
    fn task_tools_without_edits_are_planning() {
        let tools = ["TaskCreate", "TaskUpdate"];
        assert_eq!(
            classify(&tools, &[], None, 0),
            Activity::Planning
        );
    }

    #[test]
    fn task_tools_plus_edits_classify_by_edit_rules() {
        let tools = ["TaskCreate", "Edit"];
        assert_eq!(
            classify(&tools, &[], Some("implement the login page"), 20),
            Activity::Feature
        );
    }

    #[test]
    fn edit_plus_debug_keyword_is_debugging() {
        let tools = ["Edit", "Read"];
        assert_eq!(
            classify(&tools, &[], Some("fix the crash in login"), 10),
            Activity::Debugging
        );
    }

    #[test]
    fn edit_plus_refactor_keyword() {
        let tools = ["Edit"];
        assert_eq!(
            classify(&tools, &[], Some("refactor the usage store"), 5),
            Activity::Refactor
        );
    }

    #[test]
    fn edit_plus_feature_keyword() {
        let tools = ["Write"];
        assert_eq!(
            classify(&tools, &[], Some("implement a new dashboard"), 5),
            Activity::Feature
        );
    }

    #[test]
    fn edit_without_keyword_defaults_to_coding() {
        let tools = ["Edit"];
        assert_eq!(
            classify(&tools, &[], Some("update the styling"), 5),
            Activity::Coding
        );
    }

    #[test]
    fn debug_beats_refactor_when_both_keywords_present() {
        let tools = ["Edit"];
        assert_eq!(
            classify(&tools, &[], Some("fix the bug in this refactor"), 5),
            Activity::Debugging
        );
    }

    #[test]
    fn bash_with_test_cmd_is_testing() {
        let tools = ["Bash"];
        assert_eq!(
            classify(&tools, &["pytest tests/"], None, 10),
            Activity::Testing
        );
    }

    #[test]
    fn bash_with_git_cmd_is_gitops() {
        let tools = ["Bash"];
        assert_eq!(
            classify(&tools, &["git push origin main"], None, 5),
            Activity::GitOps
        );
    }

    #[test]
    fn bash_with_build_cmd_is_build() {
        let tools = ["Bash"];
        assert_eq!(
            classify(&tools, &["npm run build"], None, 5),
            Activity::Build
        );
    }

    #[test]
    fn bash_with_install_cmd_is_build() {
        let tools = ["Bash"];
        assert_eq!(
            classify(&tools, &["npm install react"], None, 5),
            Activity::Build
        );
    }

    #[test]
    fn bash_without_recognizable_cmd_is_exploration() {
        let tools = ["Bash"];
        assert_eq!(
            classify(&tools, &["ls -la"], None, 5),
            Activity::Exploration
        );
    }

    #[test]
    fn read_only_is_exploration() {
        let tools = ["Read", "Grep"];
        assert_eq!(
            classify(&tools, &[], None, 100),
            Activity::Exploration
        );
    }

    #[test]
    fn search_only_is_exploration() {
        let tools = ["WebSearch"];
        assert_eq!(
            classify(&tools, &[], None, 50),
            Activity::Exploration
        );
    }

    #[test]
    fn skill_only_is_general() {
        let tools = ["Skill"];
        assert_eq!(
            classify(&tools, &[], None, 10),
            Activity::General
        );
    }

    #[test]
    fn empty_tools_with_brainstorm_keyword() {
        assert_eq!(
            classify(&[], &[], Some("what if we tried a different approach"), 200),
            Activity::Brainstorm
        );
    }

    #[test]
    fn empty_tools_with_research_keyword_is_exploration() {
        assert_eq!(
            classify(&[], &[], Some("research how anthropic handles caching"), 200),
            Activity::Exploration
        );
    }

    #[test]
    fn empty_tools_with_debug_keyword_is_debugging() {
        assert_eq!(
            classify(&[], &[], Some("why is my test failing"), 100),
            Activity::Debugging
        );
    }

    #[test]
    fn empty_tools_with_text_is_conversation() {
        assert_eq!(
            classify(&[], &[], Some("thanks, that works great"), 30),
            Activity::Conversation
        );
    }

    #[test]
    fn empty_tools_no_text_no_user_is_general() {
        assert_eq!(classify(&[], &[], None, 0), Activity::General);
    }

    #[test]
    fn normalize_user_text_lowercases_and_truncates() {
        let long: String = "A".repeat(1000);
        let n = normalize_user_text(&long);
        assert_eq!(n.len(), 500);
        assert!(n.chars().all(|c| c == 'a'));
    }

    #[test]
    fn normalize_preserves_multibyte_boundaries() {
        // chars() not bytes — a 500-char cap must not split a multibyte char.
        let s = "é".repeat(600);
        let n = normalize_user_text(&s);
        assert_eq!(n.chars().count(), 500);
    }

    #[test]
    fn activity_serializes_kebab_case() {
        let json = serde_json::to_string(&Activity::GitOps).unwrap();
        assert_eq!(json, "\"git-ops\"");
        let json = serde_json::to_string(&Activity::Conversation).unwrap();
        assert_eq!(json, "\"conversation\"");
    }
}
