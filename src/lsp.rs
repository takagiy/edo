//! A language package's `[package.lsp]`: how to start its language server and where its project is.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use edo_lsp_client::{Client, DiagnosticSeverity, Unavailable};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::command::Written;
use crate::diagnostic::{Diagnostic, new_errors};

/// How long a write call waits for servers' diagnostics, shared by the files it writes.
pub const WAIT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Lsp {
    /// The server's program and arguments, run without a shell.
    pub command: Vec<String>,
    /// Files or directories whose nearest ancestor directory is the project root.
    #[serde(default)]
    pub root_markers: Vec<String>,
    /// LSP's `languageId`; loading fills in the package name when it is left out.
    pub language_id: Option<String>,
    pub distribution: Option<Distribution>,
}

/// Where the server comes from when it is not already on PATH.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Distribution {
    /// `mise exec <tools> -- <command>`, which installs the tools on first use.
    Mise { tools: Vec<String> },
}

impl Lsp {
    /// The command that starts the server: through `mise exec` when mise distributes it and `mise` is found,
    /// otherwise `command` as is, for a server already on PATH.
    pub fn launch(&self, mise: bool) -> Vec<String> {
        match &self.distribution {
            Some(Distribution::Mise { tools }) if mise => {
                let exec = ["mise", "exec"].iter().map(|s| s.to_string()).chain(tools.iter().cloned());
                exec.chain(["--".to_string()]).chain(self.command.iter().cloned()).collect()
            }
            _ => self.command.clone(),
        }
    }

    /// The project root for `file`: its nearest ancestor directory holding a root marker, or its directory.
    pub fn root(&self, file: &Path) -> PathBuf {
        let dir = file.parent().unwrap_or(file);
        let holds_marker = |d: &&Path| self.root_markers.iter().any(|m| d.join(m).exists());
        dir.ancestors().find(holds_marker).unwrap_or(dir).to_path_buf()
    }

    /// The server's command, and whether it can start here: on PATH, or distributed by mise with mise on PATH.
    pub fn status(&self) -> Value {
        let mise = matches!(self.distribution, Some(Distribution::Mise { .. })) && on_path("mise");
        let found = mise || self.command.first().is_some_and(|program| on_path(program));
        json!({"command": self.command, "found": found})
    }
}

/// The language servers an MCP session started, with their languages, kept for later writes. A server that
/// takes changes to its workspace folders takes every project root of its language; others take one each.
#[derive(Default)]
pub struct Servers {
    clients: Vec<(String, Client)>,
}

impl Servers {
    /// LSP's warnings on a write, starting the language's server for the file's project when needed:
    /// `new_lsp_errors`, or `lsp: starting` when the diagnostics did not come in time, or `lsp: stopped` when
    /// the server exited, which drops it so that the next write starts it again.
    /// A language without `[package.lsp]`, or whose server cannot be started, gets none. Waits until `deadline`.
    pub fn check(&mut self, written: &Written, deadline: Instant) -> Map<String, Value> {
        let (Some(lsp), Ok(path)) = (&written.lang.lsp, std::path::absolute(written.path)) else { return Map::new() };
        let wait = || deadline.saturating_duration_since(Instant::now());
        let found = match self.server(&written.lang.name, lsp, &lsp.root(&path), wait()) {
            Ok(Some(at)) => warnings(&mut self.clients[at].1, &path, written, wait()).map_err(|e| (at, e)),
            Ok(None) => return Map::new(),
            Err(failed) => Err(failed),
        };
        let status = match found {
            Ok(found) => return found,
            Err((_, Unavailable::Late)) => "starting",
            Err((at, Unavailable::Closed)) => {
                self.clients.remove(at);
                "stopped"
            }
        };
        Map::from_iter([("lsp".to_string(), json!(status))])
    }

    /// Where the server of `language` that takes `root` is: one that has it, one that takes it as another
    /// workspace folder, or one started for it; `None` when none can be started. Fails with the place of a
    /// server that could not say whether it takes `root`.
    fn server(
        &mut self,
        language: &str,
        lsp: &Lsp,
        root: &Path,
        wait: Duration,
    ) -> Result<Option<usize>, (usize, Unavailable)> {
        if let Some(at) = self.clients.iter().position(|(l, c)| l == language && c.serves(root)) {
            return Ok(Some(at));
        }
        for (at, (l, client)) in self.clients.iter_mut().enumerate() {
            if l == language && client.add_folder(root, wait).map_err(|e| (at, e))? {
                return Ok(Some(at));
            }
        }
        let language_id = lsp.language_id.as_deref().unwrap_or(language);
        let Ok(client) = Client::start(&lsp.launch(on_path("mise")), root, language_id) else { return Ok(None) };
        self.clients.push((language.to_string(), client));
        Ok(Some(self.clients.len() - 1))
    }
}

/// The errors a write added by the server's diagnostics, as `new_lsp_errors`. Without diagnostics for the
/// text before the write, the text after it is not asked about.
pub fn warnings(
    client: &mut Client,
    path: &Path,
    written: &Written,
    wait: Duration,
) -> Result<Map<String, Value>, Unavailable> {
    let errors = |diagnostics: Vec<edo_lsp_client::Diagnostic>| -> Vec<Diagnostic> {
        let errors = diagnostics.into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR));
        errors.map(|d| Diagnostic { line: d.range.start.line as usize + 1, message: d.message }).collect()
    };
    let before = errors(client.diagnostics(path, written.before, wait)?);
    let after = errors(client.diagnostics(path, written.after, wait)?);
    let added = new_errors(&before, after, written.hunks);
    if added.is_empty() {
        return Ok(Map::new());
    }
    let added: Vec<Value> = added.iter().map(|e| json!({"line": e.line, "message": e.message})).collect();
    Ok(Map::from_iter([("new_lsp_errors".to_string(), added.into())]))
}

/// Whether `program` is on PATH, with the extensions PATHEXT lists on Windows.
pub fn on_path(program: &str) -> bool {
    let extensions: Vec<String> = std::env::var("PATHEXT").map_or(vec![], |e| e.split(';').map(String::from).collect());
    let names: Vec<String> =
        std::iter::once(String::new()).chain(extensions).map(|e| format!("{program}{e}")).collect();
    let dirs = std::env::var_os("PATH").map_or(vec![], |p| std::env::split_paths(&p).collect());
    dirs.iter().any(|dir| names.iter().any(|name| dir.join(name).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::Hunk;
    use crate::language::Language;

    fn lsp(distribution: Option<Distribution>) -> Lsp {
        Lsp {
            command: vec!["typescript-language-server".into(), "--stdio".into()],
            root_markers: vec!["package.json".into()],
            language_id: Some("typescript".into()),
            distribution,
        }
    }

    fn mise() -> Option<Distribution> {
        Some(Distribution::Mise { tools: vec!["node".into(), "npm:typescript-language-server".into()] })
    }

    #[test]
    fn mise_distribution_starts_through_mise_exec_when_mise_is_found() {
        // Given / When: mise で配られるサーバーを、mise がある環境で起動する
        let command = lsp(mise()).launch(true);
        // Then: mise exec <tools> -- <command> で起動する
        assert_eq!(
            command,
            ["mise", "exec", "node", "npm:typescript-language-server", "--", "typescript-language-server", "--stdio"]
        );
    }

    #[test]
    fn without_mise_command_starts_as_is() {
        // Given / When: mise で配られるサーバーを、mise のない環境で起動する
        let command = lsp(mise()).launch(false);
        // Then: command をそのまま起動する
        assert_eq!(command, ["typescript-language-server", "--stdio"]);
    }

    #[test]
    fn root_is_nearest_ancestor_with_root_marker() {
        // Given: package.json のあるディレクトリの、さらに下のファイル
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app/src")).unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("app/package.json"), "{}").unwrap();
        // When: ルートを求める
        let root = lsp(None).root(&dir.path().join("app/src/a.ts"));
        // Then: いちばん近い package.json のあるディレクトリになる
        assert_eq!(root, dir.path().join("app"));
    }

    #[test]
    fn root_without_marker_is_file_directory() {
        // Given: どの親にも root_markers のないファイル
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut lsp = lsp(None);
        lsp.root_markers = vec!["no-such-marker".into()];
        // When: ルートを求める
        let root = lsp.root(&dir.path().join("src/a.ts"));
        // Then: ファイルのあるディレクトリになる
        assert_eq!(root, dir.path().join("src"));
    }

    fn fake_server(silent: bool) -> Client {
        fake_server_with(silent, json!({}))
    }

    /// A language server on another thread that answers initialize with `capabilities` and publishes, for each
    /// line of a document, an error `bad` where the line holds ERR and a warning `meh` where it holds WARN; or,
    /// when `silent`, publishes nothing.
    fn fake_server_with(silent: bool, capabilities: Value) -> Client {
        use lsp_server::{Message, Notification, Response};
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (server_reads, client_writes) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let (mut reader, mut writer) = (std::io::BufReader::new(server_reads), server_writes);
            while let Ok(Some(message)) = Message::read(&mut reader) {
                let reply: Message = match message {
                    Message::Request(r) => Response::new_ok(r.id, json!({ "capabilities": capabilities })).into(),
                    Message::Notification(n) if n.method.starts_with("textDocument/did") && !silent => {
                        let text = n.params["contentChanges"][0]["text"].as_str();
                        let text = text.or(n.params["textDocument"]["text"].as_str()).unwrap_or("").to_string();
                        let diagnostics: Vec<Value> = text
                            .lines()
                            .enumerate()
                            .filter_map(|(i, l)| match l {
                                "ERR" => Some((i, 1, "bad")),
                                "WARN" => Some((i, 2, "meh")),
                                _ => None,
                            })
                            .map(|(i, severity, message)| {
                                let at = json!({"line": i, "character": 0});
                                json!({"range": {"start": at, "end": at}, "severity": severity, "message": message})
                            })
                            .collect();
                        let params = json!({"uri": n.params["textDocument"]["uri"], "diagnostics": diagnostics});
                        Notification::new("textDocument/publishDiagnostics".into(), params).into()
                    }
                    _ => continue,
                };
                if reply.write(&mut writer).is_err() {
                    return;
                }
            }
        });
        Client::connect(std::io::BufReader::new(client_reads), client_writes, &std::env::temp_dir(), "rust")
    }

    /// What the LSP check adds to the report for a write from `before` to `after` through `hunks`.
    fn check(client: &mut Client, before: &str, after: &str, hunks: &[Hunk]) -> Value {
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let path = std::env::temp_dir().join("a.rs");
        let written = Written { lang, path: &path, before, after, hunks };
        Value::Object(warnings(client, &path, &written, Duration::from_secs(5)).unwrap())
    }

    #[test]
    fn error_added_by_write_is_new_lsp_error() {
        // Given: エラーのない文書と、2 行目にエラーの出る行を足す書き込み
        let mut client = fake_server(false);
        // When: LSP の警告を求める
        let found = check(&mut client, "a\n", "a\nERR\n", &[Hunk { old: 2..2, new: 2..3 }]);
        // Then: new_lsp_errors に、行と文で入る
        assert_eq!(found, json!({"new_lsp_errors": [{"line": 2, "message": "bad"}]}));
    }

    #[test]
    fn error_present_before_write_is_not_reported() {
        // Given: 1 行目にエラーのある文書と、その後ろに行を足す書き込み
        let mut client = fake_server(false);
        // When: LSP の警告を求める
        let found = check(&mut client, "ERR\nb\n", "ERR\nb\nc\n", &[Hunk { old: 3..3, new: 3..4 }]);
        // Then: もとからあったエラーは報告されない
        assert_eq!(found, json!({}));
    }

    #[test]
    fn diagnostics_other_than_errors_are_not_reported() {
        // Given: 警告の出る行を足す書き込み
        let mut client = fake_server(false);
        // When: LSP の警告を求める
        let found = check(&mut client, "a\n", "a\nWARN\n", &[Hunk { old: 2..2, new: 2..3 }]);
        // Then: 警告は報告されない
        assert_eq!(found, json!({}));
    }

    #[test]
    fn diagnostics_not_in_time_are_lsp_starting() {
        // Given: 診断を送らないサーバー
        let (servers, found) = check_through_servers(fake_server(true));
        // Then: lsp が starting になり、サーバーはそのまま残る
        assert_eq!(found, json!({"lsp": "starting"}));
        assert_eq!(servers.clients.len(), 1);
    }

    #[test]
    fn exited_server_is_lsp_stopped_and_dropped() {
        // Given: つないだとたんに終了したサーバー
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (_, client_writes) = std::io::pipe().unwrap();
        drop(server_writes);
        let client =
            Client::connect(std::io::BufReader::new(client_reads), client_writes, &std::env::temp_dir(), "rust");
        // When: LSP の警告を求める
        let (servers, found) = check_through_servers(client);
        // Then: lsp が stopped になり、次の書き込みで起動し直すよう、サーバーは捨てられる
        assert_eq!(found, json!({"lsp": "stopped"}));
        assert!(servers.clients.is_empty());
    }

    #[test]
    fn text_after_write_is_not_asked_without_diagnostics_before_it() {
        // Given: 診断を送らないサーバー
        let mut client = fake_server(true);
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let path = std::env::temp_dir().join("a.rs");
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[] };
        // When: 待つ時間を 300ms にして LSP の警告を求める
        let start = std::time::Instant::now();
        let found = warnings(&mut client, &path, &written, Duration::from_millis(300));
        // Then: 届かなかったと答え、書いた後の文書のぶんは待たない
        assert_eq!(found, Err(Unavailable::Late));
        assert!(start.elapsed() < Duration::from_millis(550), "took {:?}", start.elapsed());
    }

    /// Checks a write of a.rs through `Servers` that already hold `client` as a.rs's server.
    fn check_through_servers(client: Client) -> (Servers, Value) {
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let path = std::path::absolute(std::env::temp_dir().join("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), client));
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[] };
        let found = Value::Object(servers.check(&written, Instant::now() + WAIT));
        (servers, found)
    }

    /// A write of `<dir>/a.rs`, whose root is `dir` as no Cargo.toml is above it.
    fn write_in<'a>(lang: &'a Language, dir: &'a Path) -> Written<'a> {
        Written { lang, path: dir.join("a.rs").leak(), before: "a\n", after: "b\n", hunks: &[] }
    }

    #[test]
    fn file_of_another_root_is_checked_by_server_that_takes_folders() {
        // Given: あるルートの Rust のサーバーで、workspace folder の変更に対応するもの
        let folders = json!({"workspace": {"workspaceFolders": {"supported": true, "changeNotifications": true}}});
        let (first, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server_with(false, folders)));
        servers.check(&write_in(lang, first.path()), Instant::now() + WAIT);
        // When: 別のルートの Rust のファイルを書く
        let found = servers.check(&write_in(lang, other.path()), Instant::now() + WAIT);
        // Then: そのサーバーにフォルダーを足して確かめ、サーバーは起動しない
        assert_eq!(Value::Object(found), json!({}));
        assert_eq!(servers.clients.len(), 1);
        assert!(servers.clients[0].1.serves(&std::path::absolute(other.path()).unwrap()));
    }

    #[test]
    fn file_of_another_root_starts_its_own_server_without_one_that_takes_folders() {
        // Given: workspace folder の変更に対応しない、あるルートのサーバーと、すぐに終わるコマンドをサーバーにする言語
        let rust = crate::language::Languages::builtin();
        let rust = rust.for_path(Path::new("a.rs")).unwrap();
        let exits = if cfg!(windows) { vec!["cmd", "/c", "exit"] } else { vec!["true"] };
        let lsp = Lsp { command: exits.into_iter().map(String::from).collect(), distribution: None, ..lsp(None) };
        let query = tree_sitter::Query::new(&rust.grammar, "").unwrap();
        let lang = Language {
            name: rust.name.clone(),
            extensions: vec![],
            grammar: rust.grammar.clone(),
            query,
            lsp: Some(lsp),
        };
        let (first, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server(false)));
        servers.check(&write_in(&lang, first.path()), Instant::now() + WAIT);
        // When: 別のルートのファイルを書く
        let found = servers.check(&write_in(&lang, other.path()), Instant::now() + WAIT);
        // Then: そのルートのサーバーを起動し（すぐに終わるので stopped になる）、前のサーバーはフォルダーを足されない
        assert_eq!(Value::Object(found), json!({"lsp": "stopped"}));
        assert_eq!(servers.clients.len(), 1);
        assert!(!servers.clients[0].1.serves(&std::path::absolute(other.path()).unwrap()));
    }

    #[test]
    fn language_without_lsp_section_gets_nothing() {
        // Given: [package.lsp] のない言語（Markdown）のファイルの書き込み
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.md")).unwrap();
        let path = std::env::temp_dir().join("a.md");
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[] };
        // When: 起動したサーバーの一覧に確かめさせる
        let found = Servers::default().check(&written, Instant::now() + WAIT);
        // Then: 何も付かない
        assert!(found.is_empty(), "{found:?}");
    }
}
