//! `edo harness init`: registers edo's MCP server and hooks in each harness's user-level settings.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::Error;

/// Runs a program with arguments, failing with a message.
pub type Run<'a> = &'a dyn Fn(&str, &[String]) -> Result<(), String>;

/// The user's home, edo's own path, and how to run an outside program; tests stand in for all three.
pub struct Env<'a> {
    pub home: PathBuf,
    pub exe: PathBuf,
    pub run: Run<'a>,
}

/// A harness edo installs into: its settings directory under the home, and how to install or remove.
struct Harness {
    name: &'static str,
    dir: &'static str,
    install: fn(&Env, &Path, bool) -> Result<(), Error>,
}

const HARNESSES: [Harness; 1] = [Harness { name: "ClaudeCode", dir: ".claude", install: claude }];

/// Installs edo into the named harnesses, or into every harness whose settings directory exists;
/// with `remove`, takes out what an install added. Reports per harness.
pub fn init(env: &Env, names: &[String], remove: bool) -> Vec<Value> {
    let present = HARNESSES.iter().filter(|h| env.home.join(h.dir).is_dir()).map(|h| h.name.to_string());
    let names: Vec<String> = if names.is_empty() { present.collect() } else { names.to_vec() };
    let report = |name: &str| {
        let harness = HARNESSES.iter().find(|h| h.name == name).ok_or_else(|| {
            let known: Vec<&str> = HARNESSES.iter().map(|h| h.name).collect();
            Error::new("unknown_harness", format!("{name} is not one of {}", known.join(", ")))
        })?;
        let dir = env.home.join(harness.dir);
        if !dir.is_dir() {
            return Err(Error::new("not_found", format!("{} does not exist", dir.display())));
        }
        (harness.install)(env, &dir, remove)
    };
    let reports = names.iter().map(|name| match report(name) {
        Ok(()) => json!({"ok": true, "harness": name, "installed": !remove}),
        Err(e) => json!({"ok": false, "harness": name, "error": e.code, "message": e.message}),
    });
    reports.collect()
}

/// Runs a program and reports its stderr when it fails.
pub fn run_program(program: &str, args: &[String]) -> Result<(), String> {
    let out = std::process::Command::new(program).args(args).output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(format!("{program} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
}

/// Claude Code owns `~/.claude.json`, where user-scope MCP servers live, so its CLI registers edo there;
/// the hooks go into `settings.json`.
fn claude(env: &Env, dir: &Path, remove: bool) -> Result<(), Error> {
    let claude = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        (env.run)("claude", &args).map_err(|e| Error::new("harness", e))
    };
    // Also takes out an earlier registration, so installing again replaces it; there may be none.
    let _ = claude(&["mcp", "remove", "--scope", "user", "edo"]);
    if !remove {
        claude(&["mcp", "add", "--scope", "user", "edo", "--", &env.exe.to_string_lossy(), "mcp"])?;
    }
    let groups: &[(&str, &str)] =
        if remove { &[] } else { &[("PreToolUse", "Read|Edit|Write"), ("PostToolUse", "Bash")] };
    edit_json(&dir.join("settings.json"), |settings| set_hooks(settings, "ClaudeCode", &env.exe, groups))
}

/// Replaces edo's hooks under `hooks.<event>` with one group per `(event, matcher)`, leaving other hooks alone.
fn set_hooks(settings: &mut Value, harness: &str, exe: &Path, groups: &[(&str, &str)]) -> Result<(), Error> {
    let Some(settings) = settings.as_object_mut() else {
        return Err(Error::new("settings", "settings are not a JSON object"));
    };
    let events = settings.entry("hooks").or_insert_with(|| json!({}));
    let events = events.as_object_mut().ok_or_else(|| Error::new("settings", "hooks is not a JSON object"))?;
    for groups in events.values_mut().filter_map(Value::as_array_mut) {
        groups.retain_mut(|group| match group.get_mut("hooks").and_then(Value::as_array_mut) {
            Some(hooks) => {
                hooks.retain(|h| !is_edo_hook(h["command"].as_str().unwrap_or(""), harness));
                !hooks.is_empty()
            }
            None => true,
        });
    }
    events.retain(|_, groups| groups.as_array().is_none_or(|g| !g.is_empty()));
    let command = |event: &str| format!("\"{}\" hook {harness}:{event}", exe.to_string_lossy().replace('\\', "/"));
    for (event, matcher) in groups {
        let group = json!({"matcher": matcher, "hooks": [{"type": "command", "command": command(event)}]});
        let list = events.entry(*event).or_insert_with(|| json!([]));
        list.as_array_mut().ok_or_else(|| Error::new("settings", format!("hooks.{event} is not a list")))?.push(group);
    }
    if events.is_empty() {
        settings.remove("hooks");
    }
    Ok(())
}

/// A hook edo added: `"<path>/edo" hook <harness>:<event>`, whichever edo it points at.
fn is_edo_hook(command: &str, harness: &str) -> bool {
    let (program, rest) = match command.strip_prefix('"') {
        Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
        None => command.split_once(' ').unwrap_or((command, "")),
    };
    Path::new(program).file_stem().is_some_and(|s| s == "edo")
        && rest.trim_start().starts_with(&format!("hook {harness}:"))
}

/// Rewrites a JSON settings file through `edit`, keeping the previous version as `<file>.bak`.
/// A file that does not parse is left alone.
fn edit_json(path: &Path, edit: impl FnOnce(&mut Value) -> Result<(), Error>) -> Result<(), Error> {
    let io = |e: std::io::Error| Error::new("io", format!("{}: {e}", path.display()));
    let before = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io(e)),
    };
    let mut value: Value = match &before {
        Some(text) => {
            serde_json::from_str(text).map_err(|e| Error::new("settings", format!("{}: {e}", path.display())))?
        }
        None => json!({}),
    };
    edit(&mut value)?;
    let after = serde_json::to_string_pretty(&value).expect("JSON value") + "\n";
    if before.as_deref() == Some(after.as_str()) {
        return Ok(());
    }
    if let Some(before) = &before {
        std::fs::write(path.with_extension("json.bak"), before).map_err(io)?;
    }
    std::fs::write(path, after).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A home with `~/.claude`, and a stand-in for outside programs that records each call.
    struct Fixture {
        home: tempfile::TempDir,
        calls: RefCell<Vec<String>>,
    }

    impl Fixture {
        fn new() -> Fixture {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir(home.path().join(".claude")).unwrap();
            Fixture { home, calls: RefCell::default() }
        }

        fn init(&self, exe: &str, names: &[&str], remove: bool) -> Vec<Value> {
            let run = |program: &str, args: &[String]| {
                self.calls.borrow_mut().push(format!("{program} {}", args.join(" ")));
                Ok(())
            };
            let env = Env { home: self.home.path().to_path_buf(), exe: PathBuf::from(exe), run: &run };
            let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
            init(&env, &names, remove)
        }

        fn settings_path(&self) -> PathBuf {
            self.home.path().join(".claude/settings.json")
        }

        fn settings(&self) -> Value {
            serde_json::from_str(&std::fs::read_to_string(self.settings_path()).unwrap()).unwrap()
        }

        fn write_settings(&self, v: Value) {
            std::fs::write(self.settings_path(), serde_json::to_string_pretty(&v).unwrap()).unwrap();
        }

        /// Every hook command under an event.
        fn commands(&self, event: &str) -> Vec<String> {
            let settings = self.settings();
            let groups = settings["hooks"][event].as_array().cloned().unwrap_or_default();
            let hooks = groups.iter().flat_map(|g| g["hooks"].as_array().cloned().unwrap_or_default());
            hooks.map(|h| h["command"].as_str().unwrap().to_string()).collect()
        }
    }

    const EDO: &str = "/opt/edo/bin/edo";

    #[test]
    fn claude_mcp_is_added_at_user_scope_with_absolute_path() {
        // Given: ~/.claude のあるホーム
        let f = Fixture::new();
        // When: Claude Code に入れる
        f.init(EDO, &["ClaudeCode"], false);
        // Then: claude mcp add --scope user に edo の絶対パスと mcp が渡る
        assert!(f.calls.borrow().contains(&format!("claude mcp add --scope user edo -- {EDO} mcp")), "{:?}", f.calls);
    }

    #[test]
    fn claude_hooks_go_into_settings_with_absolute_path() {
        // Given: ~/.claude のあるホーム
        let f = Fixture::new();
        // When: Claude Code に入れる
        f.init(EDO, &["ClaudeCode"], false);
        // Then: 実行前は Read、Edit、Write に、実行後は Bash に、edo の絶対パスの hook が入る
        let settings = f.settings();
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "Read|Edit|Write");
        assert_eq!(settings["hooks"]["PostToolUse"][0]["matcher"], "Bash");
        assert_eq!(f.commands("PreToolUse"), [format!("\"{EDO}\" hook ClaudeCode:PreToolUse")]);
        assert_eq!(f.commands("PostToolUse"), [format!("\"{EDO}\" hook ClaudeCode:PostToolUse")]);
    }

    #[test]
    fn other_settings_and_hooks_stay() {
        // Given: ほかの設定とほかの hook がある settings.json
        let f = Fixture::new();
        let rtk = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]});
        f.write_settings(json!({"model": "opus", "hooks": {"PreToolUse": [rtk.clone()]}}));
        // When: Claude Code に入れる
        f.init(EDO, &["ClaudeCode"], false);
        // Then: ほかの設定とほかの hook はそのまま残る
        let settings = f.settings();
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["hooks"]["PreToolUse"][0], rtk);
    }

    #[test]
    fn installing_again_does_not_duplicate_hooks() {
        // Given: 一度入れたホーム
        let f = Fixture::new();
        f.init(EDO, &["ClaudeCode"], false);
        // When: もう一度入れる
        f.init(EDO, &["ClaudeCode"], false);
        // Then: hook は重複しない
        assert_eq!(f.commands("PreToolUse").len(), 1);
        assert_eq!(f.commands("PostToolUse").len(), 1);
    }

    #[test]
    fn installing_from_another_edo_replaces_hook_path() {
        // Given: 一度入れたホーム
        let f = Fixture::new();
        f.init(EDO, &["ClaudeCode"], false);
        // When: 別の場所の edo で入れ直す
        f.init("/usr/local/bin/edo", &["ClaudeCode"], false);
        // Then: hook のパスが置き換わる
        assert_eq!(f.commands("PreToolUse"), ["\"/usr/local/bin/edo\" hook ClaudeCode:PreToolUse"]);
    }

    #[test]
    fn previous_settings_are_kept_as_bak() {
        // Given: settings.json があるホーム
        let f = Fixture::new();
        f.write_settings(json!({"model": "opus"}));
        let before = std::fs::read_to_string(f.settings_path()).unwrap();
        // When: Claude Code に入れる
        f.init(EDO, &["ClaudeCode"], false);
        // Then: 書き換える前の settings.json が .bak に残る
        let bak = std::fs::read_to_string(f.home.path().join(".claude/settings.json.bak")).unwrap();
        assert_eq!(bak, before);
    }

    #[test]
    fn remove_drops_edo_hooks_and_keeps_others() {
        // Given: ほかの hook と edo の hook がある settings.json
        let f = Fixture::new();
        let rtk = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]});
        f.write_settings(json!({"hooks": {"PreToolUse": [rtk.clone()]}}));
        f.init(EDO, &["ClaudeCode"], false);
        // When: 外す
        f.init(EDO, &["ClaudeCode"], true);
        // Then: edo の hook が消え、ほかの hook は残る
        assert_eq!(f.settings()["hooks"], json!({"PreToolUse": [rtk]}));
    }

    #[test]
    fn remove_takes_mcp_out_with_claude_mcp_remove() {
        // Given: ~/.claude のあるホーム
        let f = Fixture::new();
        // When: 外す
        f.init(EDO, &["ClaudeCode"], true);
        // Then: claude mcp remove --scope user で MCP が外れ、add は呼ばれない
        let calls = f.calls.borrow();
        assert!(calls.contains(&"claude mcp remove --scope user edo".to_string()), "{calls:?}");
        assert!(!calls.iter().any(|c| c.contains("mcp add")), "{calls:?}");
    }

    #[test]
    fn omitted_harnesses_mean_those_with_settings_dirs() {
        // Given: ~/.claude だけがあるホーム
        let f = Fixture::new();
        // When: ハーネスを省いて入れる
        let reports = f.init(EDO, &[], false);
        // Then: 設定ディレクトリがある Claude Code だけが対象になる
        let harnesses: Vec<&str> = reports.iter().map(|r| r["harness"].as_str().unwrap()).collect();
        assert_eq!(harnesses, ["ClaudeCode"]);
    }

    #[test]
    fn named_harness_without_settings_dir_is_error() {
        // Given: ~/.claude のないホーム
        let f = Fixture::new();
        std::fs::remove_dir(f.home.path().join(".claude")).unwrap();
        // When: Claude Code を指定して入れる
        let reports = f.init(EDO, &["ClaudeCode"], false);
        // Then: そのハーネスはエラーになる
        assert_eq!(reports[0]["ok"], false);
        assert_eq!(reports[0]["error"], "not_found");
    }

    #[test]
    fn report_is_json_array_per_harness() {
        // Given: ~/.claude のあるホーム
        let f = Fixture::new();
        // When: 入れる
        let reports = f.init(EDO, &["ClaudeCode"], false);
        // Then: ハーネスごとの報告になる
        assert_eq!(reports, [json!({"ok": true, "harness": "ClaudeCode", "installed": true})]);
    }
}
