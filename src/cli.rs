use clap::{Parser, Subcommand};
use serde_json::json;

use crate::command::{self, Mode, WriteRequest};
use crate::install;
use crate::language::Languages;

#[derive(Parser)]
#[command(name = "edo", version, about = "AST-aware headless code editor for agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print a file with line numbers and function bodies elided, or the definitions/lines selected by a scope.
    ///
    /// <TARGET> is <file>, <file>:<identifier> (Foo.bar or Foo::bar), <file>:<N> or <file>:<N-M>, or several of these after one <file>: separated by commas. <file> may be a glob with * and ?.
    /// With several targets, each one's text follows its path.
    Read {
        #[arg(required = true)]
        targets: Vec<String>,
    },
    /// Replace <BEFORE> with <AFTER>, ignoring indentation and code style differences.
    ///
    /// Without <BEFORE>, the whole target is replaced. Pass - as <AFTER> to read stdin; stdin with
    /// <<<<<<< before / ======= / >>>>>>> after markers gives both.
    ///
    /// On success, "replaced" in the report gives the line ranges that now hold <AFTER>; there is no need
    /// to read the file back.
    Write {
        /// What to do with each match (or the whole target without <BEFORE>).
        #[arg(long, value_enum, default_value_t = Mode::ReplaceMatches)]
        mode: Mode,
        /// Replace every match instead of failing as ambiguous.
        #[arg(long)]
        all_matches: bool,
        target: String,
        /// [BEFORE] <AFTER>
        #[arg(num_args = 1..)]
        texts: Vec<String>,
    },
    /// Serve the read and write tools over MCP (stdio).
    Mcp,
    /// Manage language packages.
    Lang {
        #[command(subcommand)]
        cmd: LangCmd,
    },
    /// Set up coding harnesses to use edo.
    Harness {
        #[command(subcommand)]
        cmd: HarnessCmd,
    },
    /// Answer a harness hook (<HARNESS>:<EVENT>, such as ClaudeCode:PreToolUse) for the input on stdin.
    ///
    /// Run by the hooks that `edo harness init` installs.
    Hook { target: String },
}

#[derive(Subcommand)]
enum HarnessCmd {
    /// Register edo's MCP server and hooks in the user settings of the given harnesses (ClaudeCode, Codex,
    /// Cursor), or of every harness whose settings directory exists.
    Init {
        harnesses: Vec<String>,
        /// Take out what init added instead.
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Subcommand)]
enum LangCmd {
    /// Install packages given as <name> (default registry), <repo>:<name>, or <repo> whose root is a package.
    ///
    /// Without any, opens an interactive installer listing the known registries' packages.
    Install { specs: Vec<String> },
    /// List builtin and installed packages.
    List,
    /// Remove an installed package given as <name> or <repo>:<name>.
    Remove { spec: String },
    /// Manage the known registries the interactive installer lists.
    Registry {
        #[command(subcommand)]
        cmd: RegistryCmd,
    },
}

#[derive(Subcommand)]
enum RegistryCmd {
    /// Fetch a registry given as <repo> and make it known.
    Add { repo: String },
    /// Forget a registry given as <repo>; packages installed from it stay.
    Remove { repo: String },
}

fn lang(cmd: LangCmd, interactive: bool) -> (String, i32) {
    let home = install::home_dir();
    let fetch = &install::fetch_github;
    let out = match cmd {
        LangCmd::Install { specs } if specs.is_empty() => {
            if !interactive {
                return usage("give packages to install, or run in a terminal for the interactive installer".into());
            }
            match crate::installer::run(&home, fetch) {
                Ok(reports) => reports.into(),
                Err(e) => e.to_json(),
            }
        }
        LangCmd::Install { specs } => install::install_all(&home, &specs, fetch).into(),
        LangCmd::List => install::list(&home).unwrap_or_else(|e| e.to_json()),
        LangCmd::Remove { spec } => install::remove(&home, &spec),
        LangCmd::Registry { cmd: RegistryCmd::Add { repo } } => install::add_registry(&home, &repo, fetch),
        LangCmd::Registry { cmd: RegistryCmd::Remove { repo } } => install::remove_registry(&home, &repo),
    };
    // Every report is an object with ok, or an array of them.
    let failed = |v: &serde_json::Value| v["ok"] == false;
    let ok = !(failed(&out) || out.as_array().is_some_and(|a| a.iter().any(failed)));
    (crate::report(&out), if ok { 0 } else { 1 })
}

fn harness(names: &[String], remove: bool) -> (String, i32) {
    let (Some(home), Ok(exe)) = (dirs::home_dir(), std::env::current_exe()) else {
        return (
            crate::report(
                &json!({"ok": false, "error": "io", "message": "cannot find the home directory or edo itself"}),
            ),
            1,
        );
    };
    let env = crate::harness::Env { home, exe, run: &crate::harness::run_program };
    let reports = crate::harness::init(&env, names, remove);
    let code = if reports.iter().any(|r| r["ok"] == false) { 1 } else { 0 };
    (crate::report(&reports.into()), code)
}

/// Runs the CLI and returns what to print and the exit code.
/// `interactive` says whether a person is at the terminal, so `lang install` may open the installer.
pub fn run(args: &[String], stdin: &mut dyn FnMut() -> String, langs: Languages, interactive: bool) -> (String, i32) {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() => return (e.to_string(), 0),
        Err(e) => return usage(e.to_string()),
    };
    match cli.cmd {
        Cmd::Lang { cmd } => lang(cmd, interactive),
        Cmd::Hook { target } => (crate::hook::run(&target, &stdin(), &langs), 0),
        Cmd::Harness { cmd: HarnessCmd::Init { harnesses, remove } } => harness(&harnesses, remove),
        Cmd::Mcp => match crate::mcp::serve(langs) {
            Ok(()) => (String::new(), 0),
            Err(e) => (crate::report(&json!({"ok": false, "error": "io", "message": e.to_string()})), 1),
        },
        Cmd::Read { targets } => {
            let items: Vec<command::ReadItem> = targets
                .iter()
                .map(|t| split_target(t))
                .map(|(path, scope)| command::ReadItem { path, scope })
                .collect();
            let read = command::read(&langs, &items);
            (read.text, if read.ok { 0 } else { 1 })
        }
        Cmd::Write { mode, all_matches, target, texts } => {
            let (path, scope) = split_target(&target);
            // `-` stands for stdin, which holds one text, or before/after pairs between conflict markers.
            let input = texts.iter().any(|t| t == "-").then(&mut *stdin);
            // An empty stdin is a forgotten pipe, not a request to delete everything.
            if input.as_deref() == Some("") {
                return usage("stdin is empty".into());
            }
            let blocks = input.as_deref().map(split_conflict).unwrap_or_default();
            let texts: Vec<String> =
                texts.into_iter().map(|t| if t == "-" { input.clone().unwrap() } else { t }).collect();
            let edits: Vec<(Option<String>, String)> = match texts.as_slice() {
                [_] if !blocks.is_empty() => blocks.into_iter().map(|(b, a)| (Some(b), a)).collect(),
                _ if !blocks.is_empty() => {
                    return usage("conflict markers in stdin already give before and after".into());
                }
                [after] => vec![(None, after.clone())],
                [before, after] => vec![(Some(before.clone()), after.clone())],
                _ => return usage("expected [BEFORE] <AFTER>".into()),
            };
            let reqs: Vec<WriteRequest> = edits
                .iter()
                .map(|(before, after)| WriteRequest {
                    path,
                    scope,
                    before: before.as_deref(),
                    after,
                    all_matches,
                    place: mode.into(),
                })
                .collect();
            // The CLI runs no language servers; `edo mcp` keeps them running between writes.
            match command::write(&langs, &reqs, &mut ()) {
                Ok(reports) => {
                    let code = if reports.iter().any(|r| r["ok"] == false) { 1 } else { 0 };
                    (crate::report(&reports.into()), code)
                }
                Err(e) => (crate::report(&e.to_json()), 1),
            }
        }
    }
}

/// The `<<<<<<< before` / `=======` / `>>>>>>> after` blocks as (before, after); markers count only as whole lines.
fn split_conflict(input: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = input.lines().collect();
    let at = |from: usize, marker: &str| lines[from..].iter().position(|l| l.trim_end() == marker).map(|i| from + i);
    let mut blocks = vec![];
    let mut from = 0;
    while let Some(open) = at(from, "<<<<<<< before")
        && let Some(mid) = at(open, "=======")
        && let Some(close) = at(mid, ">>>>>>> after")
    {
        blocks.push((lines[open + 1..mid].join("\n"), lines[mid + 1..close].join("\n")));
        from = close + 1;
    }
    blocks
}

fn usage(message: String) -> (String, i32) {
    (crate::report(&json!({"ok": false, "error": "usage", "message": message})), 2)
}

/// Splits `<file>:<scope>` at the first `:`, except a drive letter's `:` in `C:/...`.
pub fn split_target(arg: &str) -> (&str, Option<&str>) {
    let b = arg.as_bytes();
    let skip = if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' { 2 } else { 0 };
    match arg[skip..].find(':') {
        Some(i) => (&arg[..skip + i], Some(&arg[skip + i + 1..])),
        None => (arg, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_splits_at_first_colon_into_file_and_scope() {
        // Given / When: <file>:<scope> を分ける / Then: 最初の : でファイルとスコープになる
        assert_eq!(split_target("src/a.kt:Foo.bar"), ("src/a.kt", Some("Foo.bar")));
    }

    #[test]
    fn scope_with_double_colon_still_splits_at_first_colon() {
        // Given / When: スコープに :: を含む引数を分ける / Then: 最初の : で分かれる
        assert_eq!(split_target("src/lib.rs:Foo::bar"), ("src/lib.rs", Some("Foo::bar")));
    }

    #[test]
    fn drive_letter_colon_is_not_a_separator() {
        // Given / When: 英字1文字:/ で始まる引数を分ける / Then: ドライブレターの : では分かれない
        assert_eq!(split_target("C:/src/a.rs:Foo"), ("C:/src/a.rs", Some("Foo")));
        assert_eq!(split_target("C:/src/a.rs"), ("C:/src/a.rs", None));
    }

    fn edo(args: &[&str], stdin: &str) -> (String, i32) {
        let args: Vec<String> = std::iter::once("edo").chain(args.iter().copied()).map(String::from).collect();
        let stdin = stdin.to_string();
        run(&args, &mut || stdin.clone(), Languages::builtin(), false)
    }

    #[test]
    fn report_without_false_ok_exits_zero() {
        // Given: 呼び出しのある Rust のファイルと JSON のファイル
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() { x(); }\n").unwrap();
        std::fs::write(dir.path().join("b.json"), "{}").unwrap();
        let glob = format!("{}/*", dir.path().to_string_lossy().replace('\\', "/"));
        // When: 両方に一致する glob で書き、ok を持たない skipped の要素が報告に付く
        let (out, code) = edo(&["write", &glob, "x();", "y();"], "");
        // Then: ok が false の要素がないので終了コードは 0
        assert!(out.contains("skipped"), "{out}");
        assert_eq!(code, 0);
    }

    fn file(text: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, text).unwrap();
        (dir, path.to_string_lossy().replace('\\', "/"))
    }

    #[test]
    fn dash_after_reads_after_from_stdin() {
        // Given: 呼び出しのある関数
        let (_dir, path) = file("fn a() {\n    x();\n}\n");
        // When: after に - を渡し、標準入力に after を流す
        edo(&["write", &path, "x();", "-"], "y();");
        // Then: 標準入力の内容が after になる
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    y();\n}\n");
    }

    #[test]
    fn stdin_with_conflict_markers_gives_before_and_after() {
        // Given: 呼び出しのある関数
        let (_dir, path) = file("fn a() {\n    x();\n}\n");
        // When: - だけを渡し、コンフリクトマーカーで before と after を書いた標準入力を流す
        edo(&["write", &path, "-"], "<<<<<<< before\nx();\n=======\ny();\nz();\n>>>>>>> after\n");
        // Then: マーカーで分けた before が after に置き換わる
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    y();\n    z();\n}\n");
    }

    #[test]
    fn stdin_with_several_conflict_blocks_writes_each() {
        // Given: 呼び出しが 2 つある関数
        let (_dir, path) = file("fn a() {\n    x();\n    y();\n}\n");
        // When: コンフリクトマーカーの組を 2 つ書いた標準入力を流す
        let input =
            "<<<<<<< before\nx();\n=======\nv();\n>>>>>>> after\n<<<<<<< before\ny();\n=======\nw();\n>>>>>>> after\n";
        edo(&["write", &path, "-"], input);
        // Then: 組ごとに置き換わる
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    v();\n    w();\n}\n");
    }

    #[test]
    fn successful_read_prints_text_and_exits_zero() {
        // Given: 関数のあるファイル
        let (_dir, path) = file("fn a() {}\n");
        // When: 読む
        let (out, code) = edo(&["read", &path], "");
        // Then: テキストが出力され、終了コードは 0
        assert_eq!((out.as_str(), code), ("1\tfn a() {}\n", 0));
    }

    #[test]
    fn read_takes_several_targets() {
        // Given: 関数が 2 つあるファイル
        let (_dir, path) = file("fn a() {}\nfn b() {}\n");
        // When: 同じファイルの 2 つの定義を、対象として並べて読む
        let (out, _) = edo(&["read", &format!("{path}:a"), &format!("{path}:b")], "");
        // Then: 対象ごとに、パスの行の後にその内容が並ぶ
        assert_eq!(out, format!("{path}\n1\tfn a() {{}}\n{path}\n2\tfn b() {{}}\n"));
    }

    #[test]
    fn write_report_is_printed_as_mison() {
        // Given: 呼び出しのある関数
        let (_dir, path) = file("fn a() { x(); }\n");
        // When: 書く
        let (out, _) = edo(&["write", &path, "x();", "y();"], "");
        // Then: 報告が MISON で出力される
        assert_eq!(out, format!("ok=true file={path} replaced=1-1"));
    }

    #[test]
    fn report_with_failure_exits_one() {
        // Given: 呼び出しのない関数
        let (_dir, path) = file("fn a() {}\n");
        // When: 一致しない before で書く
        let (_, code) = edo(&["write", &path, "x();", "y();"], "");
        // Then: 終了コードは 1
        assert_eq!(code, 1);
    }

    fn written(args: &[&str]) -> String {
        let (_dir, path) = file("fn a() {\n    x();\n}\n");
        let args: Vec<&str> = args.iter().map(|a| if *a == "FILE" { path.as_str() } else { a }).collect();
        edo(&args, "");
        std::fs::read_to_string(&path).unwrap()
    }

    #[test]
    fn omitted_mode_replaces_matches() {
        // Given / When: --mode なしで書く / Then: 一致が置き換わる
        assert_eq!(written(&["write", "FILE", "x();", "y();"]), "fn a() {\n    y();\n}\n");
    }

    #[test]
    fn insert_after_matches_mode_inserts_after_match() {
        // Given / When: --mode insert-after-matches で書く / Then: 一致の後ろの行に入る
        let out = written(&["write", "--mode", "insert-after-matches", "FILE", "x();", "y();"]);
        assert_eq!(out, "fn a() {\n    x();\n    y();\n}\n");
    }

    #[test]
    fn insert_before_matches_mode_inserts_before_match() {
        // Given / When: --mode insert-before-matches で書く / Then: 一致の前の行に入る
        let out = written(&["write", "--mode", "insert-before-matches", "FILE", "x();", "y();"]);
        assert_eq!(out, "fn a() {\n    y();\n    x();\n}\n");
    }

    #[test]
    fn unknown_mode_is_usage_error() {
        // Given / When: --mode に未知の値を渡す
        let (out, code) = edo(&["write", "--mode", "append", "a.rs", "x"], "");
        // Then: usage エラーになる
        assert!(out.contains("error=usage") && code != 0, "{out}");
    }

    #[test]
    fn too_many_positional_arguments_are_usage_error() {
        // Given / When: 位置引数を 4 つ渡す
        let (out, code) = edo(&["write", "a.rs", "b.rs", "x", "y"], "");
        // Then: usage エラーになる
        assert!(out.contains("error=usage") && code != 0, "{out}");
    }

    #[test]
    fn before_with_conflict_marker_stdin_is_usage_error() {
        // Given: 呼び出しのある関数
        let (_dir, path) = file("fn a() {\n    x();\n}\n");
        // When: before を渡したうえで、コンフリクトマーカー付きの標準入力を流す
        let (out, code) = edo(&["write", &path, "x();", "-"], "<<<<<<< before\nx();\n=======\ny();\n>>>>>>> after\n");
        // Then: usage エラーになり、ファイルは変わらない
        assert!(out.contains("error=usage") && code != 0, "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    x();\n}\n");
    }

    #[test]
    fn dash_after_with_empty_stdin_is_usage_error() {
        // Given: 関数のあるファイル
        let (_dir, path) = file("fn a() {}\n");
        // When: after に - を渡し、標準入力に何も流さない
        let (out, code) = edo(&["write", &path, "-"], "");
        // Then: usage エラーになり、ファイルは変わらない
        assert!(out.contains("error=usage") && code != 0, "{out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {}\n");
    }

    #[test]
    fn lang_install_without_specs_outside_terminal_is_usage_error() {
        // Given: 端末でない呼び出し
        // When: lang install を引数なしで呼ぶ
        let (out, code) = edo(&["lang", "install"], "");
        // Then: usage エラーになる
        assert_eq!(code, 2, "{out}");
    }

    #[test]
    fn hook_answers_stdin_and_exits_zero() {
        // Given: Rust のファイルを読む Claude Code の hook の入力
        let input = serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": "src/a.rs"}}).to_string();
        // When: hook を呼ぶ
        let (out, code) = edo(&["hook", "ClaudeCode:PreToolUse"], &input);
        // Then: 止める答えが出力され、終了コードは 0 になる
        assert!(out.contains("\"deny\""), "{out}");
        assert_eq!(code, 0);
    }
}
