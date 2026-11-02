//! A language package's `[package.lsp]`: how to start its language server and where its project is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use edo_lsp_client::{Client, DiagnosticSeverity, Unavailable};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::command::Written;
use crate::diagnostic::{Diagnostic, new_errors};

/// How long a write waits for a server's diagnostics on one text.
const WAIT: Duration = Duration::from_secs(2);

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

/// The language servers an MCP session started, one per language and project root, kept for later writes.
#[derive(Default)]
pub struct Servers {
    clients: HashMap<(String, PathBuf), Client>,
}

impl Servers {
    /// LSP's warnings on a write, starting the language's server for the file's project when needed:
    /// `new_lsp_errors`, or `lsp: starting` when the diagnostics did not come in time, or `lsp: stopped` when
    /// the server exited, which drops it so that the next write starts it again.
    /// A language without `[package.lsp]`, or whose server cannot be started, gets none.
    pub fn check(&mut self, written: &Written) -> Map<String, Value> {
        let (Some(lsp), Ok(path)) = (&written.lang.lsp, std::path::absolute(written.path)) else { return Map::new() };
        let root = lsp.root(&path);
        let key = (written.lang.name.clone(), root.clone());
        if !self.clients.contains_key(&key) {
            let language_id = lsp.language_id.as_deref().unwrap_or(&written.lang.name);
            let Ok(client) = Client::start(&lsp.launch(on_path("mise")), &root, language_id) else { return Map::new() };
            self.clients.insert(key.clone(), client);
        }
        let status = match warnings(self.clients.get_mut(&key).expect("just started"), &path, written, WAIT) {
            Ok(found) => return found,
            Err(Unavailable::Late) => "starting",
            Err(Unavailable::Closed) => {
                self.clients.remove(&key);
                "stopped"
            }
        };
        Map::from_iter([("lsp".to_string(), json!(status))])
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

    /// A language server on another thread that publishes, for each line of a document, an error `bad` where the
    /// line holds ERR and a warning `meh` where it holds WARN; or, when `silent`, publishes nothing.
    fn fake_server(silent: bool) -> Client {
        use lsp_server::{Message, Notification, Response};
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (server_reads, client_writes) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let (mut reader, mut writer) = (std::io::BufReader::new(server_reads), server_writes);
            while let Ok(Some(message)) = Message::read(&mut reader) {
                let reply: Message = match message {
                    Message::Request(r) => Response::new_ok(r.id, json!({"capabilities": {}})).into(),
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
        let root = lang.lsp.as_ref().unwrap().root(&path);
        let mut servers = Servers::default();
        servers.clients.insert((lang.name.clone(), root), client);
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[] };
        let found = Value::Object(servers.check(&written));
        (servers, found)
    }

    #[test]
    fn language_without_lsp_section_gets_nothing() {
        // Given: [package.lsp] のない言語（Markdown）のファイルの書き込み
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.md")).unwrap();
        let path = std::env::temp_dir().join("a.md");
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[] };
        // When: 起動したサーバーの一覧に確かめさせる
        let found = Servers::default().check(&written);
        // Then: 何も付かない
        assert!(found.is_empty(), "{found:?}");
    }
}
