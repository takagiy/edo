//! `edo hook <harness>:<event>`: steers a harness's built-in file tools to edo.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::language::Languages;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Pre,
    Post,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Read,
    Edit,
    Write,
    Shell,
}

/// A tool call, whatever harness it came from.
#[derive(Debug, PartialEq)]
pub struct Call {
    pub action: Action,
    /// The files it touches; for a shell command, the files it prints.
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Allow,
    Deny(String),
    Advise(String),
}

/// Denies the built-in file tools on files edo has a language for before they run, and notes edo
/// after a shell command printed such a file. Phrased as facts, as the harnesses' docs advise.
pub fn decide(phase: Phase, call: &Call, langs: &Languages) -> Decision {
    let Some(path) = call.paths.iter().find(|p| langs.for_path(p).is_some()) else { return Decision::Allow };
    let path = path.display();
    match (phase, call.action) {
        (Phase::Pre, Action::Read) => Decision::Deny(format!("{path} is a source file edo reads: use mcp__edo__read.")),
        (Phase::Pre, Action::Edit) => {
            Decision::Deny(format!("{path} is a source file edo edits: use mcp__edo__write with before and after."))
        }
        (Phase::Pre, Action::Write) => Decision::Deny(format!(
            "{path} is a source file edo writes: use mcp__edo__write without before or scope to write the whole file."
        )),
        (Phase::Post, Action::Shell) => Decision::Advise(format!("The user wants {path} read with mcp__edo__read.")),
        _ => Decision::Allow,
    }
}

/// Commands that print the files they are given.
const PRINTERS: [&str; 11] = ["cat", "head", "tail", "less", "more", "bat", "nl", "sed", "type", "Get-Content", "gc"];

/// The files a shell command prints: the arguments of a printing command, up to the first operator.
// shortcut: reads only the first command and splits on whitespace; a miss only skips advice, so it stays simple.
pub fn shown_files(command: &str) -> Vec<PathBuf> {
    let mut words = command.split_whitespace().take_while(|w| !matches!(*w, "|" | "||" | "&&" | ";" | ">" | ">>"));
    if !words.next().is_some_and(|w| PRINTERS.contains(&w)) {
        return vec![];
    }
    words.filter(|w| !w.starts_with('-')).map(|w| PathBuf::from(w.trim_matches(['"', '\'']))).collect()
}

/// How one harness's hook input becomes a `Call` and a `Decision` becomes its answer.
struct Adapter {
    call: fn(&Value) -> Option<Call>,
    answer: fn(Phase, Decision) -> String,
}

fn adapter(harness: &str) -> Option<Adapter> {
    match harness {
        "ClaudeCode" => Some(Adapter { call: claude_call, answer: claude_answer }),
        _ => None,
    }
}

/// Answers `<harness>:<event>` for the hook input on stdin. Anything it cannot read is allowed, since a
/// broken hook must not stop the agent; the reason goes to stderr.
pub fn run(target: &str, input: &str, langs: &Languages) -> String {
    let (harness, event) = target.split_once(':').unwrap_or((target, ""));
    let Some(adapter) = adapter(harness) else {
        eprintln!("edo hook: unknown harness {harness}; allowing");
        return String::new();
    };
    let phase = match event {
        "PreToolUse" => Phase::Pre,
        "PostToolUse" => Phase::Post,
        _ => {
            eprintln!("edo hook: unknown event {event}; allowing");
            return (adapter.answer)(Phase::Pre, Decision::Allow);
        }
    };
    let decision = match serde_json::from_str::<Value>(input) {
        Ok(input) => (adapter.call)(&input).map_or(Decision::Allow, |call| decide(phase, &call, langs)),
        Err(e) => {
            eprintln!("edo hook: unreadable input ({e}); allowing");
            Decision::Allow
        }
    };
    (adapter.answer)(phase, decision)
}

fn claude_call(input: &Value) -> Option<Call> {
    let tool = &input["tool_input"];
    let file = || tool["file_path"].as_str().map(PathBuf::from).into_iter().collect();
    Some(match input["tool_name"].as_str()? {
        "Read" => Call { action: Action::Read, paths: file() },
        "Edit" | "MultiEdit" => Call { action: Action::Edit, paths: file() },
        "Write" => Call { action: Action::Write, paths: file() },
        "Bash" => Call { action: Action::Shell, paths: shown_files(tool["command"].as_str()?) },
        _ => return None,
    })
}

/// Claude Code reads nothing as allow, `permissionDecision` as a denial and `additionalContext` as a note.
fn claude_answer(phase: Phase, decision: Decision) -> String {
    let event = match phase {
        Phase::Pre => "PreToolUse",
        Phase::Post => "PostToolUse",
    };
    let output = match decision {
        Decision::Allow => return String::new(),
        Decision::Deny(reason) => {
            json!({"hookEventName": event, "permissionDecision": "deny", "permissionDecisionReason": reason})
        }
        Decision::Advise(note) => json!({"hookEventName": event, "additionalContext": note}),
    };
    json!({ "hookSpecificOutput": output }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs() -> Languages {
        Languages::builtin()
    }

    fn call(action: Action, path: &str) -> Call {
        Call { action, paths: vec![PathBuf::from(path)] }
    }

    fn denial(decision: Decision) -> String {
        match decision {
            Decision::Deny(reason) => reason,
            other => panic!("expected a denial, got {other:?}"),
        }
    }

    #[test]
    fn reading_source_file_before_run_is_denied_toward_edo_read() {
        // Given / When: 実行前に、対応言語のファイルを読む呼び出しを判断する
        let reason = denial(decide(Phase::Pre, &call(Action::Read, "src/a.rs"), &langs()));
        // Then: 止められ、mcp__edo__read が理由になる
        assert!(reason.contains("mcp__edo__read"), "{reason}");
    }

    #[test]
    fn editing_source_file_before_run_is_denied_toward_edo_write_with_before() {
        // Given / When: 実行前に、対応言語のファイルを編集する呼び出しを判断する
        let reason = denial(decide(Phase::Pre, &call(Action::Edit, "src/a.rs"), &langs()));
        // Then: 止められ、before と after を使う mcp__edo__write が理由になる
        assert!(reason.contains("mcp__edo__write") && reason.contains("before"), "{reason}");
    }

    #[test]
    fn writing_source_file_before_run_is_denied_toward_whole_file_edo_write() {
        // Given / When: 実行前に、対応言語のファイルを書く呼び出しを判断する
        let reason = denial(decide(Phase::Pre, &call(Action::Write, "src/a.rs"), &langs()));
        // Then: 止められ、ファイル全体を書く mcp__edo__write が理由になる
        assert!(reason.contains("mcp__edo__write") && reason.contains("whole file"), "{reason}");
    }

    #[test]
    fn unsupported_file_before_run_is_allowed() {
        // Given / When: 実行前に、対応していないファイルを編集する呼び出しを判断する
        let decision = decide(Phase::Pre, &call(Action::Edit, "config.yaml"), &langs());
        // Then: 通る
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn shell_before_run_is_allowed() {
        // Given / When: 実行前に、対応言語のファイルを表示するシェルの呼び出しを判断する
        let shell = Call { action: Action::Shell, paths: shown_files("cat src/a.rs") };
        // Then: 通る
        assert_eq!(decide(Phase::Pre, &shell, &langs()), Decision::Allow);
    }

    #[test]
    fn shell_showing_source_file_after_run_is_advised_edo() {
        // Given / When: 実行後に、対応言語のファイルを表示したシェルの呼び出しを判断する
        let shell = Call { action: Action::Shell, paths: shown_files("cat src/a.rs | head -5") };
        // Then: edo が使えるという文が添えられる
        match decide(Phase::Post, &shell, &langs()) {
            Decision::Advise(note) => assert!(note.contains("src/a.rs") && note.contains("mcp__edo__read"), "{note}"),
            other => panic!("expected advice, got {other:?}"),
        }
    }

    #[test]
    fn shell_not_showing_files_after_run_gets_nothing() {
        // Given / When: 実行後に、表示するコマンドでないシェルの呼び出しを判断する
        let shell = Call { action: Action::Shell, paths: shown_files("rm src/a.rs") };
        // Then: 何も添えられない
        assert_eq!(decide(Phase::Post, &shell, &langs()), Decision::Allow);
    }

    #[test]
    fn shell_showing_unsupported_file_after_run_gets_nothing() {
        // Given / When: 実行後に、対応していないファイルを表示したシェルの呼び出しを判断する
        let shell = Call { action: Action::Shell, paths: shown_files("cat config.yaml") };
        // Then: 何も添えられない
        assert_eq!(decide(Phase::Post, &shell, &langs()), Decision::Allow);
    }

    #[test]
    fn file_tools_after_run_get_nothing() {
        // Given / When: 実行後に、対応言語のファイルを読む呼び出しを判断する
        let decision = decide(Phase::Post, &call(Action::Read, "src/a.rs"), &langs());
        // Then: 何も添えられない
        assert_eq!(decision, Decision::Allow);
    }

    fn claude(event: &str, input: Value) -> Value {
        let out = run(&format!("ClaudeCode:{event}"), &input.to_string(), &langs());
        if out.is_empty() { Value::Null } else { serde_json::from_str(&out).unwrap() }
    }

    #[test]
    fn claude_file_tools_take_path_from_file_path() {
        // Given: Claude Code の Edit の入力
        let input = json!({"tool_name": "Edit", "tool_input": {"file_path": "C:\\src\\a.rs", "old_string": "a", "new_string": "b"}});
        // When: 実行前の hook に渡す
        let out = claude("PreToolUse", input);
        // Then: file_path のファイルについての判断になる
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny", "{out}");
    }

    #[test]
    fn claude_bash_takes_shell_command_from_command() {
        // Given: Claude Code の Bash の入力
        let input = json!({"tool_name": "Bash", "tool_input": {"command": "cat src/a.rs"}, "tool_response": {}});
        // When: 実行後の hook に渡す
        let out = claude("PostToolUse", input);
        // Then: command のシェルの呼び出しについての判断になる
        assert!(out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("src/a.rs"), "{out}");
    }

    #[test]
    fn claude_denial_is_permission_decision_with_reason() {
        // Given: 対応言語のファイルを読む Claude Code の入力
        let input = json!({"tool_name": "Read", "tool_input": {"file_path": "src/a.rs"}});
        // When: 実行前の hook に渡す
        let out = claude("PreToolUse", input);
        // Then: permissionDecision: deny と理由を返す
        let hook = &out["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], "PreToolUse");
        assert_eq!(hook["permissionDecision"], "deny");
        assert!(hook["permissionDecisionReason"].as_str().unwrap().contains("mcp__edo__read"), "{out}");
    }

    #[test]
    fn claude_advice_is_additional_context() {
        // Given: 対応言語のファイルを表示した Claude Code の Bash の入力
        let input = json!({"tool_name": "Bash", "tool_input": {"command": "head -20 src/a.rs"}});
        // When: 実行後の hook に渡す
        let out = claude("PostToolUse", input);
        // Then: additionalContext を返す
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(out["hookSpecificOutput"]["additionalContext"].is_string(), "{out}");
    }

    #[test]
    fn claude_allow_prints_nothing() {
        // Given: 対応していないファイルを読む Claude Code の入力
        let input = json!({"tool_name": "Read", "tool_input": {"file_path": "a.yaml"}});
        // When: 実行前の hook に渡す
        let out = run("ClaudeCode:PreToolUse", &input.to_string(), &langs());
        // Then: 何も出力しない
        assert_eq!(out, "");
    }

    #[test]
    fn unreadable_input_is_allowed_in_harness_form() {
        // Given / When: JSON でない入力を Claude Code の実行前の hook に渡す
        let out = run("ClaudeCode:PreToolUse", "not json", &langs());
        // Then: Claude Code の形で通す（何も出力しない）
        assert_eq!(out, "");
    }

    #[test]
    fn unknown_harness_or_event_is_allowed() {
        // Given / When: 知らないハーネスとイベントで呼ぶ
        let input = json!({"tool_name": "Read", "tool_input": {"file_path": "src/a.rs"}}).to_string();
        // Then: 通す（何も出力しない）
        assert_eq!(run("Nope:PreToolUse", &input, &langs()), "");
        assert_eq!(run("ClaudeCode:Stop", &input, &langs()), "");
    }
}
