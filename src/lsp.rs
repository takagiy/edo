//! A language package's `[package.lsp]`: how to start its language server and where its project is.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use edo_lsp_client::{Client, DiagnosticSeverity, FormatError, Unavailable};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::command::Written;
use crate::diagnostic::{Diagnostic, new_errors};
use crate::language::Languages;

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
    /// What is still owed to the agent about each file written, by absolute path.
    pending: HashMap<PathBuf, Pending>,
}

/// What edo still owes the agent about a file it wrote: the formatting its server was not ready for, and the
/// errors the write added that come after it.
struct Pending {
    /// The path as the write was given it, for the report.
    file: String,
    /// The file's text as edo left it; a file holding something else changed since, and is left alone.
    text: String,
    format: bool,
    /// The errors before the write, at their lines in `text`; `None` when the server did not give them in time.
    before: Option<Vec<Diagnostic>>,
    /// The errors the write added that the agent has heard of.
    told: Vec<Diagnostic>,
}

impl Pending {
    fn new(written: &Written) -> Pending {
        let file = written.path.to_string_lossy().into_owned();
        Pending { file, text: written.after.into(), format: false, before: None, told: vec![] }
    }
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
            Ok(Some(at)) => {
                let client = &mut self.clients[at].1;
                let found = added_errors(client, &path, written, wait());
                // A save starts the checks a server runs only on saved files, such as cargo check; their
                // diagnostics come later, for the next call's news.
                client.saved(&path, written.after);
                let pending = self.pending.entry(path.clone()).or_insert_with(|| Pending::new(written));
                pending.text = written.after.into();
                (pending.before, pending.told) = match &found {
                    Ok((before, added)) => (Some(before.clone()), added.clone()),
                    Err(_) => (None, vec![]),
                };
                found.map_err(|e| (at, e))
            }
            Ok(None) => return Map::new(),
            Err(failed) => Err(failed),
        };
        let status = match found {
            Ok((_, added)) => return reported(&added),
            Err((_, Unavailable::Late)) => "starting",
            Err((at, Unavailable::Closed)) => {
                self.clients.remove(at);
                "stopped"
            }
        };
        Map::from_iter([("lsp".to_string(), json!(status))])
    }

    /// What happened since the last call to the files edo wrote, for the agent to hear once: the formatting their
    /// server was not ready for at the write, done now that it is, and the errors the write added that came after
    /// it. Returns the report, `None` when nothing happened, and the files formatting rewrote.
    pub fn news(&mut self, langs: &Languages) -> (Option<Value>, Vec<String>) {
        let (mut formatted, mut refused, mut late_errors, mut rewritten) = (vec![], vec![], vec![], vec![]);
        let paths: Vec<PathBuf> = self.pending.keys().cloned().collect();
        for path in paths {
            let on_disk = std::fs::read_to_string(&path).ok();
            if on_disk.as_deref() != Some(self.pending[&path].text.as_str()) {
                self.pending.remove(&path);
                continue;
            }
            let Some((lang, lsp)) = langs.for_path(&path).and_then(|l| Some((l, l.lsp.as_ref()?))) else { continue };
            let root = lsp.root(&path);
            let Some(at) = self.clients.iter().position(|(l, c)| *l == lang.name && c.serves(&root)) else { continue };
            let (client, pending) = (&mut self.clients[at].1, self.pending.get_mut(&path).expect("listed"));
            if pending.format && client.is_ready() {
                pending.format = false;
                let (tab_size, spaces) = indentation(&pending.text);
                match client.format(&path, &pending.text, tab_size, spaces, WAIT) {
                    Ok(text) if text != pending.text && std::fs::write(&path, &text).is_ok() => {
                        client.saved(&path, &text);
                        let hunks = crate::diagnostic::hunks(&pending.text, &text);
                        let changed = hunks.iter().filter(|h| !h.new.is_empty());
                        let lines: Vec<String> =
                            changed.map(|h| format!("{}-{}", h.new.start, h.new.end - 1)).collect();
                        formatted.push(json!({"file": pending.file, "lines": lines}));
                        rewritten.push(pending.file.clone());
                        let moved = |errors: &[Diagnostic]| -> Vec<Diagnostic> {
                            let at = |e: &Diagnostic| crate::diagnostic::moved(&hunks, e.line);
                            errors
                                .iter()
                                .filter_map(|e| Some(Diagnostic { line: at(e)?, message: e.message.clone() }))
                                .collect()
                        };
                        pending.before = pending.before.as_deref().map(moved);
                        pending.told = moved(&pending.told);
                        pending.text = text;
                    }
                    Ok(_) => {}
                    Err(FormatError::Failed(message)) => {
                        refused.push(json!({"file": pending.file, "message": message}))
                    }
                    Err(FormatError::Unavailable(_)) => pending.format = true,
                }
            }
            if let Some(before) = &pending.before {
                let added = new_errors(before, errors(client.known(&path)), Some);
                let new: Vec<Diagnostic> = added.into_iter().filter(|e| !pending.told.contains(e)).collect();
                if !new.is_empty() {
                    let found: Vec<Value> = new.iter().map(|e| json!({"line": e.line, "message": e.message})).collect();
                    late_errors.push(json!({"file": pending.file, "errors": found}));
                    pending.told.extend(new);
                }
            }
        }
        let news: Map<String, Value> =
            [("formatted", formatted), ("format_refused", refused), ("new_lsp_errors", late_errors)]
                .into_iter()
                .filter(|(_, found)| !found.is_empty())
                .map(|(key, found)| (key.to_string(), found.into()))
                .collect();
        ((!news.is_empty()).then(|| news.into()), rewritten)
    }

    /// `written.after` formatted by the language's server, indented as the file is, waiting until `deadline`; as is
    /// when the language has no server, or the server's message when it refused. A server not ready yet formats the
    /// file for a later call's news.
    pub fn format(&mut self, written: &Written, deadline: Instant) -> Result<String, String> {
        let as_is = || Ok(written.after.to_string());
        let (Some(lsp), Ok(path)) = (&written.lang.lsp, std::path::absolute(written.path)) else { return as_is() };
        let wait = || deadline.saturating_duration_since(Instant::now());
        let Ok(Some(at)) = self.server(&written.lang.name, lsp, &lsp.root(&path), wait()) else { return as_is() };
        let (tab_size, spaces) = indentation(written.after);
        let found = self.clients[at].1.format(&path, written.after, tab_size, spaces, wait());
        let pending = self.pending.entry(path).or_insert_with(|| Pending::new(written));
        pending.format = found == Err(FormatError::Unavailable(Unavailable::Late));
        match found {
            Ok(text) => Ok(text),
            Err(FormatError::Failed(message)) => Err(message),
            Err(FormatError::Unavailable(_)) => as_is(),
        }
    }
    /// The errors the servers have sent so far for each of `files`, without waiting, as
    /// `{"lsp_errors": [{"file", "errors": [{"line", "message"}]}]}`; `None` when none of them has any.
    /// They come from checks a write started, such as cargo check, which take longer than a write waits.
    pub fn read_errors(&mut self, langs: &Languages, files: &[String]) -> Option<Value> {
        let mut found = vec![];
        let mut seen = std::collections::HashSet::new();
        for file in files {
            let (Some(lang), Ok(path)) = (langs.for_path(Path::new(file)), std::path::absolute(file)) else { continue };
            // A file read in several ranges comes several times.
            if !seen.insert(path.clone()) {
                continue;
            }
            let mut known: Vec<Diagnostic> = vec![];
            for (_, client) in self.clients.iter_mut().filter(|(l, _)| *l == lang.name) {
                for e in errors(client.known(&path)) {
                    if !known.contains(&e) {
                        known.push(e);
                    }
                }
            }
            if !known.is_empty() {
                let errors: Vec<Value> = known.iter().map(|e| json!({"line": e.line, "message": e.message})).collect();
                found.push(json!({"file": file, "errors": errors}));
            }
        }
        (!found.is_empty()).then(|| json!({ "lsp_errors": found }))
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
    added_errors(client, path, written, wait).map(|(_, added)| reported(&added))
}

/// The errors before a write, moved to their lines after it, and the errors the write added.
fn added_errors(
    client: &mut Client,
    path: &Path,
    written: &Written,
    wait: Duration,
) -> Result<(Vec<Diagnostic>, Vec<Diagnostic>), Unavailable> {
    let before = errors(client.diagnostics(path, written.before, wait)?);
    let after = errors(client.diagnostics(path, written.after, wait)?);
    let moved =
        |l| crate::diagnostic::moved(written.hunks, l).and_then(|l| crate::diagnostic::moved(written.formatted, l));
    let before: Vec<Diagnostic> =
        before.into_iter().filter_map(|d| Some(Diagnostic { line: moved(d.line)?, message: d.message })).collect();
    let added = new_errors(&before, after, Some);
    Ok((before, added))
}

/// `new_lsp_errors` for the errors a write added, or nothing when it added none.
fn reported(added: &[Diagnostic]) -> Map<String, Value> {
    if added.is_empty() {
        return Map::new();
    }
    let added: Vec<Value> = added.iter().map(|e| json!({"line": e.line, "message": e.message})).collect();
    Map::from_iter([("new_lsp_errors".to_string(), added.into())])
}

/// LSP formatting options for a file indented as `text` is: columns per level, and whether by spaces.
fn indentation(text: &str) -> (u32, bool) {
    match crate::edit::indent_unit(&crate::edit::indents(text.lines())) {
        Some(unit) if unit == "\t" => (4, false),
        Some(unit) => (unit.len() as u32, true),
        None => (4, true),
    }
}

/// The errors among a server's diagnostics, by line (counting from 1) and message.
fn errors(diagnostics: Vec<edo_lsp_client::Diagnostic>) -> Vec<Diagnostic> {
    let errors = diagnostics.into_iter().filter(|d| d.severity == Some(DiagnosticSeverity::ERROR));
    errors.map(|d| Diagnostic { line: d.range.start.line as usize + 1, message: d.message }).collect()
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
    /// when `silent`, publishes nothing. On a save it publishes the error `saved`, as a check would. It formats a
    /// document by adding a first line with the options it was asked with, and refuses one holding REFUSE.
    fn fake_server_with(silent: bool, capabilities: Value) -> Client {
        gated_server(silent, capabilities, None)
    }

    /// Like `fake_server_with`, but answers initialize only once `gate` opens, when there is one.
    fn gated_server(silent: bool, capabilities: Value, gate: Option<std::sync::mpsc::Receiver<()>>) -> Client {
        use lsp_server::{Message, Notification, Response};
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (server_reads, client_writes) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let (mut reader, mut writer) = (std::io::BufReader::new(server_reads), server_writes);
            let mut document = String::new();
            while let Ok(Some(message)) = Message::read(&mut reader) {
                if let Message::Notification(n) = &message {
                    let text = n.params["contentChanges"][0]["text"].as_str();
                    document = text.or(n.params["textDocument"]["text"].as_str()).unwrap_or(&document).to_string();
                }
                let reply: Message = match message {
                    Message::Request(r) if r.method == "textDocument/formatting" && document.contains("REFUSE") => {
                        Response::new_err(r.id, -32603, "cannot format".into()).into()
                    }
                    Message::Request(r) if r.method == "textDocument/formatting" => {
                        let (o, at) = (&r.params["options"], json!({"line": 0, "character": 0}));
                        let line = format!("// {} {}\n", o["tabSize"], o["insertSpaces"]);
                        Response::new_ok(r.id, json!([{"range": {"start": at, "end": at}, "newText": line}])).into()
                    }
                    Message::Request(r) => {
                        if let Some(gate) = &gate {
                            let _ = gate.recv();
                        }
                        Response::new_ok(r.id, json!({ "capabilities": capabilities })).into()
                    }
                    Message::Notification(n) if n.method == "textDocument/didSave" => {
                        let at = json!({"line": 0, "character": 0});
                        let saved = json!({"range": {"start": at, "end": at}, "severity": 1, "message": "saved"});
                        let params = json!({"uri": n.params["textDocument"]["uri"], "diagnostics": [saved]});
                        Notification::new("textDocument/publishDiagnostics".into(), params).into()
                    }
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
        let written = Written { lang, path: &path, before, after, hunks, formatted: &[] };
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
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[], formatted: &[] };
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
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[], formatted: &[] };
        let found = Value::Object(servers.check(&written, Instant::now() + WAIT));
        (servers, found)
    }

    /// A write of `<dir>/a.rs`, whose root is `dir` as no Cargo.toml is above it.
    fn write_in<'a>(lang: &'a Language, dir: &'a Path) -> Written<'a> {
        Written { lang, path: dir.join("a.rs").leak(), before: "a\n", after: "b\n", hunks: &[], formatted: &[] }
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
    fn written_file_is_told_saved_to_its_server() {
        // Given: 保存の通知を求め、保存されるとそのファイルにエラー saved を送る、Rust のサーバー
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server_with(false, json!({"textDocumentSync": {"save": true}}))));
        // When: そのサーバーのフォルダーのファイルを書く
        let temp = std::env::temp_dir();
        let written = write_in(lang, &temp);
        servers.check(&written, Instant::now() + WAIT);
        // Then: そのサーバーに保存が伝わり、saved が届く
        let path = std::path::absolute(written.path).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let saved = loop {
            let known: Vec<String> = servers.clients[0].1.known(&path).into_iter().map(|d| d.message).collect();
            if known.contains(&"saved".to_string()) || Instant::now() > deadline {
                break known;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(saved.contains(&"saved".to_string()), "{saved:?}");
    }

    #[test]
    fn formatting_server_formats_with_files_indentation() {
        // Given: 整形する Rust のサーバー
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server_with(false, json!({"documentFormattingProvider": true}))));
        // When: 2 スペースで字下げしたファイルを書く前に整形させる
        let after = "fn a() {\n  x();\n}\n";
        let temp = std::env::temp_dir();
        let written = Written { after, ..write_in(lang, &temp) };
        let formatted = servers.format(&written, Instant::now() + WAIT);
        // Then: 幅 2 のスペースで整形させた内容が返る
        assert_eq!(formatted, Ok(format!("// 2 true\n{after}")));
    }

    #[test]
    fn refused_formatting_returns_servers_message() {
        // Given: 整形する Rust のサーバー
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server_with(false, json!({"documentFormattingProvider": true}))));
        // When: サーバーが整形を断るファイルを整形させる
        let temp = std::env::temp_dir();
        let written = Written { after: "REFUSE\n", ..write_in(lang, &temp) };
        let formatted = servers.format(&written, Instant::now() + WAIT);
        // Then: サーバーのメッセージが返る
        assert_eq!(formatted, Err("cannot format".to_string()));
    }

    /// A file of `text` straight in the temporary directory, which the fake servers take as their folder.
    fn temp_file(name: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// The news, asked for until it holds `key` or a few seconds pass, as servers answer on their own time.
    fn news_with(servers: &mut Servers, key: &str) -> (Option<Value>, Vec<String>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let news = servers.news(&crate::language::Languages::builtin());
            if news.0.as_ref().is_some_and(|n| !n[key].is_null()) || Instant::now() > deadline {
                return news;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn file_written_before_server_was_ready_is_formatted_at_next_news() {
        // Given: 整形するが、まだ準備ができていない Rust のサーバーで書いたファイル
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let (open, gate) = std::sync::mpsc::channel();
        let mut servers = Servers::default();
        let capabilities = json!({"documentFormattingProvider": true});
        servers.clients.push((lang.name.clone(), gated_server(false, capabilities, Some(gate))));
        let path = temp_file("edo-news-format.rs", "fn a() {}\n");
        let written = Written { lang, path: &path, before: "", after: "fn a() {}\n", hunks: &[], formatted: &[] };
        let soon = Instant::now() + Duration::from_millis(50);
        assert_eq!(servers.format(&written, soon), Ok("fn a() {}\n".to_string()));
        servers.check(&written, soon);
        // When: サーバーの準備ができてから、知らせを求める
        open.send(()).unwrap();
        let (news, rewritten) = news_with(&mut servers, "formatted");
        // Then: ファイルが整形され、変わった 1 行目と、書き換えたファイルが返る
        let file = path.to_string_lossy().into_owned();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "// 4 true\nfn a() {}\n");
        assert_eq!(news.map(|n| n["formatted"].clone()), Some(json!([{"file": file, "lines": ["1-1"]}])));
        assert_eq!(rewritten, [file]);
    }

    #[test]
    fn file_changed_before_news_is_not_formatted() {
        // Given: 準備ができていないサーバーで書いた後に、外で書き換えられたファイル
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let (open, gate) = std::sync::mpsc::channel();
        let mut servers = Servers::default();
        let capabilities = json!({"documentFormattingProvider": true});
        servers.clients.push((lang.name.clone(), gated_server(false, capabilities, Some(gate))));
        let path = temp_file("edo-news-changed.rs", "fn a() {}\n");
        let written = Written { lang, path: &path, before: "", after: "fn a() {}\n", hunks: &[], formatted: &[] };
        let soon = Instant::now() + Duration::from_millis(50);
        servers.format(&written, soon).unwrap();
        servers.check(&written, soon);
        std::fs::write(&path, "fn b() {}\n").unwrap();
        // When: サーバーの準備ができてから、知らせを求める
        open.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let (news, rewritten) = servers.news(&langs);
        // Then: 整形されず、何も返らない
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn b() {}\n");
        assert_eq!((news, rewritten), (None, vec![]));
    }

    #[test]
    fn errors_a_write_added_that_came_after_it_are_news_once() {
        // Given: 保存されるとエラー saved を送るサーバーで書いたファイル
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server_with(false, json!({"textDocumentSync": {"save": true}}))));
        let path = temp_file("edo-news-errors.rs", "b\n");
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[], formatted: &[] };
        servers.check(&written, Instant::now() + WAIT);
        // When: 知らせを 2 回求める
        let (first, _) = news_with(&mut servers, "new_lsp_errors");
        let (second, _) = servers.news(&langs);
        // Then: 1 回目にだけ、そのエラーが返る
        let file = path.to_string_lossy().into_owned();
        let errors = json!([{"file": file, "errors": [{"line": 1, "message": "saved"}]}]);
        assert_eq!(first.map(|n| n["new_lsp_errors"].clone()), Some(errors));
        assert_eq!(second, None);
    }

    #[test]
    fn errors_known_for_files_read_are_one_line() {
        // Given: Rust のサーバーが、書き込んだファイルの 1 行目にエラー bad を送ってきた状態
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server(false)));
        let path = std::env::temp_dir().join("a.rs");
        let written = Written { lang, path: &path, before: "a\n", after: "ERR\n", hunks: &[], formatted: &[] };
        servers.check(&written, Instant::now() + WAIT);
        // When: そのファイルと、エラーの届いていないファイルと、サーバーのない言語のファイルを読む
        let files = [path.clone(), std::env::temp_dir().join("b.rs"), std::env::temp_dir().join("c.md")];
        let files: Vec<String> = files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
        let found = servers.read_errors(&langs, &files);
        // Then: エラーのあるファイルだけが、行と文で並ぶ
        let file = files[0].clone();
        assert_eq!(found, Some(json!({"lsp_errors": [{"file": file, "errors": [{"line": 1, "message": "bad"}]}]})));
    }

    #[test]
    fn errors_of_file_read_twice_are_returned_once() {
        // Given: Rust のサーバーが、書き込んだファイルの 1 行目にエラー bad を送ってきた状態
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        let mut servers = Servers::default();
        servers.clients.push((lang.name.clone(), fake_server(false)));
        let path = std::env::temp_dir().join("a.rs");
        let written = Written { lang, path: &path, before: "a\n", after: "ERR\n", hunks: &[], formatted: &[] };
        servers.check(&written, Instant::now() + WAIT);
        // When: そのファイルを 2 回読む（別の範囲を読んだときのように）
        let file = path.to_string_lossy().into_owned();
        let found = servers.read_errors(&langs, &[file.clone(), file.clone()]);
        // Then: そのファイルのエラーは 1 回だけ並ぶ
        assert_eq!(found, Some(json!({"lsp_errors": [{"file": file, "errors": [{"line": 1, "message": "bad"}]}]})));
    }
    #[test]
    fn language_without_lsp_section_gets_nothing() {
        // Given: [package.lsp] のない言語（Markdown）のファイルの書き込み
        let langs = crate::language::Languages::builtin();
        let lang = langs.for_path(Path::new("a.md")).unwrap();
        let path = std::env::temp_dir().join("a.md");
        let written = Written { lang, path: &path, before: "a\n", after: "b\n", hunks: &[], formatted: &[] };
        // When: 起動したサーバーの一覧に確かめさせる
        let found = Servers::default().check(&written, Instant::now() + WAIT);
        // Then: 何も付かない
        assert!(found.is_empty(), "{found:?}");
    }
}
