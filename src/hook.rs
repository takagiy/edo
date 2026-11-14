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

/// Denies the built-in file tools on files edo has a language for before they run, and tells the agent to use
/// edo after a shell command printed such a file.
pub fn decide(phase: Phase, call: &Call, langs: &Languages) -> Decision {
    if !call.paths.iter().any(|p| langs.for_path(p).is_some()) {
        return Decision::Allow;
    }
    match (phase, call.action) {
        (Phase::Pre, Action::Read) => Decision::Deny("Use mcp__edo__read instead.".into()),
        (Phase::Pre, Action::Edit | Action::Write) => Decision::Deny("Use mcp__edo__write instead.".into()),
        (Phase::Post, Action::Shell) => Decision::Advise("Use mcp__edo__read instead.".into()),
        _ => Decision::Allow,
    }
}

/// Commands that print the files they are given, by their leading words, with how many of their arguments come
/// before the files (grep's pattern). `rtk read` and `rtk grep` are what rtk's hook turns cat, head, tail and grep
/// into before edo's hook sees the command.
const PRINTERS: [(&[&str], usize); 15] = [
    (&["cat"], 0),
    (&["head"], 0),
    (&["tail"], 0),
    (&["less"], 0),
    (&["more"], 0),
    (&["bat"], 0),
    (&["nl"], 0),
    (&["sed"], 0),
    (&["type"], 0),
    (&["Get-Content"], 0),
    (&["gc"], 0),
    (&["rtk", "read"], 0),
    (&["grep"], 1),
    (&["rg"], 1),
    (&["rtk", "grep"], 1),
];

/// The files a shell command prints: the arguments of each printing command between operators.
// shortcut: splits on whitespace, so quoting and operators without spaces around them are missed; a miss only
// skips advice, so it stays simple.
pub fn shown_files(command: &str) -> Vec<PathBuf> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let commands = words.split(|w| matches!(*w, "|" | "||" | "&&" | ";" | ">" | ">>"));
    let args = commands.filter_map(|c| {
        let (p, skip) = PRINTERS.iter().find(|(p, _)| c.starts_with(p))?;
        Some(c[p.len()..].iter().filter(|w| !w.starts_with('-')).skip(*skip))
    });
    args.flatten().map(|w| PathBuf::from(w.trim_matches(['"', '\'']))).collect()
}

/// How one harness's hook input becomes a `Call` and a `Decision` becomes its answer.
struct Adapter {
    call: fn(&Value) -> Option<Call>,
    answer: fn(Phase, Decision) -> String,
}

fn adapter(harness: &str) -> Option<Adapter> {
    match harness {
        // Cursor runs Claude Code's hooks too, so where both have edo, Cursor hears from it twice.
        "ClaudeCode" => Some(Adapter { call: tool_call, answer: hook_specific_answer }),
        "Codex" => Some(Adapter { call: codex_call, answer: hook_specific_answer }),
        "Cursor" => Some(Adapter { call: tool_call, answer: cursor_answer }),
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
    // Cursor spells its events preToolUse and postToolUse.
    let phase = match event.to_ascii_lowercase().as_str() {
        "pretooluse" => Phase::Pre,
        "posttooluse" => Phase::Post,
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

/// Claude Code's and Cursor's tools: their file tools take `file_path` and their shell (Bash or Shell)
/// `command`.
fn tool_call(input: &Value) -> Option<Call> {
    let tool = &input["tool_input"];
    let file = || tool["file_path"].as_str().map(PathBuf::from).into_iter().collect();
    Some(match input["tool_name"].as_str()? {
        "Read" => Call { action: Action::Read, paths: file() },
        "Edit" | "MultiEdit" => Call { action: Action::Edit, paths: file() },
        "Write" => Call { action: Action::Write, paths: file() },
        "Bash" | "Shell" => Call { action: Action::Shell, paths: shown_files(tool["command"].as_str()?) },
        _ => return None,
    })
}

/// Cursor blocks an action when a permission hook answers anything but valid JSON, so allow is spelled out.
fn cursor_answer(phase: Phase, decision: Decision) -> String {
    let answer = match (phase, decision) {
        (_, Decision::Deny(reason)) => json!({"permission": "deny", "agent_message": reason}),
        (_, Decision::Advise(note)) => json!({"additional_context": note}),
        (Phase::Pre, Decision::Allow) => json!({"permission": "allow"}),
        (Phase::Post, Decision::Allow) => json!({}),
    };
    answer.to_string()
}

/// Codex edits files with `apply_patch`, whose patch names each file on a `*** Add File:` or `*** Update File:`
/// line; it has no read tool. A deleted file is left alone, since edo cannot delete one.
fn codex_call(input: &Value) -> Option<Call> {
    let command = input["tool_input"]["command"].as_str()?;
    match input["tool_name"].as_str()? {
        "Bash" => Some(Call { action: Action::Shell, paths: shown_files(command) }),
        "apply_patch" => {
            let files =
                |line: &'static str| command.lines().filter_map(move |l| l.strip_prefix(line)).map(PathBuf::from);
            let updated: Vec<PathBuf> = files("*** Update File: ").collect();
            let action = if updated.is_empty() { Action::Write } else { Action::Edit };
            Some(Call { action, paths: updated.into_iter().chain(files("*** Add File: ")).collect() })
        }
        _ => None,
    }
}

/// Claude Code and Codex read nothing as allow, `permissionDecision` as a denial and `additionalContext` as a note.
fn hook_specific_answer(phase: Phase, decision: Decision) -> String {
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
        // Then: 止められ、mcp__edo__read を使うよう指示される
        assert!(reason.starts_with("Use mcp__edo__read"), "{reason}");
    }

    #[test]
    fn editing_source_file_before_run_is_denied_toward_edo_write() {
        // Given / When: 実行前に、対応言語のファイルを編集する呼び出しを判断する
        let reason = denial(decide(Phase::Pre, &call(Action::Edit, "src/a.rs"), &langs()));
        // Then: 止められ、mcp__edo__write を使うよう指示される
        assert!(reason.starts_with("Use mcp__edo__write"), "{reason}");
    }

    #[test]
    fn writing_source_file_before_run_is_denied_toward_edo_write() {
        // Given / When: 実行前に、対応言語のファイルを書く呼び出しを判断する
        let reason = denial(decide(Phase::Pre, &call(Action::Write, "src/a.rs"), &langs()));
        // Then: 止められ、mcp__edo__write を使うよう指示される
        assert!(reason.starts_with("Use mcp__edo__write"), "{reason}");
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
    fn shell_showing_source_file_after_run_is_told_to_use_edo_read() {
        // Given / When: 実行後に、対応言語のファイルを表示したシェルの呼び出しを判断する
        let shell = Call { action: Action::Shell, paths: shown_files("cat src/a.rs | head -5") };
        // Then: mcp__edo__read を使うよう指示する文が添えられ、エージェントの知っているパスは繰り返さない
        match decide(Phase::Post, &shell, &langs()) {
            Decision::Advise(note) => {
                assert!(note.starts_with("Use mcp__edo__read") && !note.contains("src/a.rs"), "{note}")
            }
            other => panic!("expected advice, got {other:?}"),
        }
    }

    #[test]
    fn grep_showing_source_file_after_run_is_advised_edo() {
        // Given / When: 実行後に、grep、rg、rtk grep で対応言語のファイルを表示した呼び出しを判断する
        for command in ["grep -n main -A5 src/a.rs", "rg main src/a.rs", "rtk grep main src/a.rs"] {
            let shell = Call { action: Action::Shell, paths: shown_files(command) };
            // Then: 文が添えられる
            assert!(matches!(decide(Phase::Post, &shell, &langs()), Decision::Advise(_)), "{command}");
        }
    }

    #[test]
    fn grep_pattern_is_not_a_shown_file() {
        // Given / When: 対応言語のファイル名に見えるパターンで、対応していないファイルを grep する
        let files = shown_files("grep main.rs Cargo.toml");
        // Then: パターンは表示したファイルに含まれない
        assert_eq!(files, [PathBuf::from("Cargo.toml")]);
    }

    #[test]
    fn shell_showing_source_file_through_rtk_read_is_advised_edo() {
        // Given / When: 実行後に、rtk が cat から書き換えた rtk read の呼び出しを判断する
        let shell = Call { action: Action::Shell, paths: shown_files("rtk read src/a.rs --head-lines 5") };
        // Then: edo が使えるという文が添えられる
        assert!(matches!(decide(Phase::Post, &shell, &langs()), Decision::Advise(_)));
    }

    #[test]
    fn shell_showing_source_file_after_cd_is_advised_edo() {
        // Given / When: 実行後に、cd の後のコマンドで対応言語のファイルを表示した呼び出しを判断する
        let shell = Call { action: Action::Shell, paths: shown_files("cd x && cat src/a.rs") };
        // Then: edo が使えるという文が添えられる
        assert!(matches!(decide(Phase::Post, &shell, &langs()), Decision::Advise(_)));
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
        assert!(out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("mcp__edo__read"), "{out}");
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

    fn codex(event: &str, input: Value) -> Value {
        let out = run(&format!("Codex:{event}"), &input.to_string(), &langs());
        if out.is_empty() { Value::Null } else { serde_json::from_str(&out).unwrap() }
    }

    fn patch(body: &str) -> Value {
        json!({"tool_name": "apply_patch", "tool_input": {"command": format!("*** Begin Patch\n{body}*** End Patch\n")}})
    }

    #[test]
    fn codex_patch_updating_file_is_editing_it() {
        // Given: Codex の、src/a.rs を更新する apply_patch の入力
        let input = patch("*** Update File: src/a.rs\n@@\n-fn a() {}\n+fn b() {}\n");
        // When: 呼び出しとして読む
        let call = codex_call(&input);
        // Then: src/a.rs を編集する呼び出しになる
        assert_eq!(call, Some(Call { action: Action::Edit, paths: vec![PathBuf::from("src/a.rs")] }));
    }

    #[test]
    fn codex_patch_adding_file_is_writing_it() {
        // Given: Codex の、src/a.rs を追加する apply_patch の入力
        let input = patch("*** Add File: src/a.rs\n+fn a() {}\n");
        // When: 呼び出しとして読む
        let call = codex_call(&input);
        // Then: src/a.rs を書く呼び出しになる
        assert_eq!(call, Some(Call { action: Action::Write, paths: vec![PathBuf::from("src/a.rs")] }));
    }

    #[test]
    fn codex_patch_on_unsupported_file_is_allowed() {
        // Given: Codex の、README.txt を更新する apply_patch の入力
        let input = patch("*** Update File: README.txt\n@@\n-a\n+b\n");
        // When: 実行前の hook に渡す
        let out = run("Codex:PreToolUse", &input.to_string(), &langs());
        // Then: 何も出力せずに通す
        assert_eq!(out, "");
    }

    #[test]
    fn codex_bash_takes_shell_command_from_command() {
        // Given: Codex の Bash の入力
        let input = json!({"tool_name": "Bash", "tool_input": {"command": "cat src/a.rs"}});
        // When: 実行後の hook に渡す
        let out = codex("PostToolUse", input);
        // Then: additionalContext で edo を勧める
        assert!(out["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("mcp__edo__read"), "{out}");
    }

    fn cursor(event: &str, input: Value) -> Value {
        serde_json::from_str(&run(&format!("Cursor:{event}"), &input.to_string(), &langs())).unwrap()
    }

    #[test]
    fn cursor_read_is_denied_with_agent_message() {
        // Given: Cursor の、対応言語のファイルを読む Read の入力
        let input =
            json!({"tool_name": "Read", "tool_input": {"file_path": "C:\\work\\a.rs"}, "cursor_version": "3.27.12"});
        // When: 実行前の hook に渡す
        let out = cursor("preToolUse", input);
        // Then: permission: deny と、エージェント向けの理由を返す
        assert_eq!(out["permission"], "deny");
        assert!(out["agent_message"].as_str().unwrap().contains("mcp__edo__read"), "{out}");
    }

    #[test]
    fn cursor_write_is_denied_toward_edo_write() {
        // Given: Cursor の、対応言語のファイルを書く Write の入力
        let input = json!({"tool_name": "Write", "tool_input": {"file_path": "a.rs", "content": "fn a() {}"}});
        // When: 実行前の hook に渡す
        let out = cursor("preToolUse", input);
        // Then: mcp__edo__write へ誘導して止める
        assert!(out["agent_message"].as_str().unwrap().contains("mcp__edo__write"), "{out}");
    }

    #[test]
    fn cursor_allow_is_permission_allow() {
        // Given: Cursor の、対応していないファイルを読む Read の入力
        let input = json!({"tool_name": "Read", "tool_input": {"file_path": "a.yaml"}});
        // When: 実行前の hook に渡す
        let out = cursor("preToolUse", input);
        // Then: 正しい JSON で permission: allow を返す
        assert_eq!(out, json!({"permission": "allow"}));
    }

    #[test]
    fn cursor_shell_after_run_is_advised_with_additional_context() {
        // Given: Cursor の、対応言語のファイルを表示した Shell の入力
        let input = json!({"tool_name": "Shell", "tool_input": {"command": "cat a.rs"}});
        // When: 実行後の hook に渡す
        let out = cursor("postToolUse", input);
        // Then: additional_context で edo を勧める
        assert!(out["additional_context"].as_str().unwrap().contains("mcp__edo__read"), "{out}");
    }

    #[test]
    fn cursor_unreadable_input_is_allowed_with_valid_json() {
        // Given / When: JSON でない入力を Cursor の実行前の hook に渡す
        let out = run("Cursor:preToolUse", "not json", &langs());
        // Then: 正しい JSON で通す（Cursor は不正な JSON の答えで操作を止めるため）
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), json!({"permission": "allow"}));
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
