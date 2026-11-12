//! A small LSP client: it starts a language server, keeps each document in step with the text it is asked
//! about, and returns the diagnostics the server publishes for that text.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, RequestId, Response};
use lsp_types::{
    ClientCapabilities, DiagnosticClientCapabilities, DiagnosticWorkspaceClientCapabilities,
    DidChangeTextDocumentParams, DidChangeWorkspaceFoldersParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    InitializeParams, PublishDiagnosticsClientCapabilities, PublishDiagnosticsParams, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem, Url, VersionedTextDocumentIdentifier,
    WorkspaceClientCapabilities, WorkspaceFolder, WorkspaceFoldersChangeEvent,
};
use serde_json::Value;

pub use lsp_types::{Diagnostic, DiagnosticSeverity};

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

const INITIALIZE: i32 = 1;

/// What the reading thread passes on from the server.
enum Event {
    /// The server answered `initialize`; `pulls` when it answers diagnostic requests, and `folders` when it
    /// takes changes to its workspace folders.
    Ready {
        pulls: bool,
        folders: bool,
    },
    Published(PublishDiagnosticsParams),
    /// The answer to a diagnostic request: its items, or `None` when it holds none to take.
    Answered(RequestId, Option<Vec<Diagnostic>>),
    /// The server says the diagnostics it gave are out of date, as after it loads the project.
    Refresh,
    /// The server registered diagnostic requests under this id, or took the registration back.
    Registered(String),
    Unregistered(String),
}

/// Why there are no diagnostics to give.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Unavailable {
    /// They did not come in time; the server may still be starting or checking.
    Late,
    /// The server has exited.
    Closed,
}
/// A document as the server has it: the text last sent, its version, and its diagnostics once published.
struct Document {
    version: i32,
    text: String,
    diagnostics: Option<Vec<Diagnostic>>,
}

pub struct Client {
    writer: Writer,
    events: Receiver<Event>,
    ready: bool,
    /// The server takes changes to its workspace folders, so one server can take several projects.
    takes_folders: bool,
    /// The workspace folders the server was given: the one it started in first.
    folders: Vec<PathBuf>,
    /// The registrations under which the server answers diagnostic requests, so they are asked for, not waited
    /// for: `initialize` for its initialize answer, and the ids it registered later.
    pull_ids: HashSet<String>,
    /// Diagnostic requests not answered yet: the document and version each asks about.
    asked: HashMap<RequestId, (String, i32)>,
    next_id: i32,
    language_id: String,
    /// Keyed by `key`, since a server may spell a document's URI differently from the client.
    documents: HashMap<String, Document>,
    /// The server process, killed with the client.
    child: Option<Child>,
}

impl Client {
    /// Starts `command` (a program and its arguments) in `root` and connects to it over its stdio.
    pub fn start(command: &[String], root: &Path, language_id: &str) -> io::Result<Client> {
        let (program, args) =
            command.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let (stdin, stdout) = (child.stdin.take().expect("piped"), child.stdout.take().expect("piped"));
        let mut client = Client::connect(io::BufReader::new(stdout), stdin, root, language_id);
        client.child = Some(child);
        Ok(client)
    }

    /// Talks LSP over `reader` and `writer`, sending `initialize` for the project at `root` right away.
    pub fn connect(
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
        root: &Path,
        language_id: &str,
    ) -> Client {
        let writer: Writer = Arc::new(Mutex::new(Box::new(writer)));
        let (events, received) = mpsc::channel();
        let replies = writer.clone();
        std::thread::spawn(move || read(reader, &replies, &events));
        let folders = vec![root.to_path_buf()];
        let folder = workspace_folder(root);
        let diagnostics = PublishDiagnosticsClientCapabilities { version_support: Some(true), ..Default::default() };
        let text_document = TextDocumentClientCapabilities {
            publish_diagnostics: Some(diagnostics),
            diagnostic: Some(DiagnosticClientCapabilities { dynamic_registration: Some(true), ..Default::default() }),
            ..Default::default()
        };
        #[allow(deprecated)] // root_uri, for servers that predate workspace folders
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: folder.as_ref().map(|f| f.uri.clone()),
            workspace_folders: folder.map(|f| vec![f]),
            capabilities: ClientCapabilities {
                text_document: Some(text_document),
                workspace: Some(WorkspaceClientCapabilities {
                    workspace_folders: Some(true),
                    diagnostic: Some(DiagnosticWorkspaceClientCapabilities { refresh_support: Some(true) }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        send(&writer, Request::new(INITIALIZE.into(), "initialize".into(), params));
        Client {
            writer,
            events: received,
            ready: false,
            takes_folders: false,
            folders,
            pull_ids: HashSet::new(),
            asked: HashMap::new(),
            next_id: INITIALIZE + 1,
            language_id: language_id.into(),
            documents: HashMap::new(),
            child: None,
        }
    }

    /// The diagnostics the server publishes for `path` holding `text`, waiting up to `wait` for them.
    /// The text last asked about is remembered with its diagnostics, so asking again costs nothing.
    pub fn diagnostics(&mut self, path: &Path, text: &str, wait: Duration) -> Result<Vec<Diagnostic>, Unavailable> {
        let deadline = Instant::now() + wait;
        // Only a relative path has no file URI, and callers ask about absolute ones.
        let key = Url::from_file_path(path).ok().and_then(|uri| key(&uri).map(|k| (uri, k)));
        let (uri, key) = key.ok_or(Unavailable::Late)?;
        self.wait_ready(deadline)?;
        // What came since the last question, such as a refresh, goes in before memory answers.
        while let Ok(event) = self.events.try_recv() {
            self.take(event);
        }
        match self.documents.get_mut(&key) {
            Some(document) if document.text == text => {}
            Some(document) => {
                document.version += 1;
                document.text = text.into();
                document.diagnostics = None;
                let change = TextDocumentContentChangeEvent { range: None, range_length: None, text: text.into() };
                let params = DidChangeTextDocumentParams {
                    text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: document.version },
                    content_changes: vec![change],
                };
                send(&self.writer, Notification::new("textDocument/didChange".into(), params));
            }
            None => {
                let item = TextDocumentItem::new(uri.clone(), self.language_id.clone(), 1, text.into());
                let params = DidOpenTextDocumentParams { text_document: item };
                send(&self.writer, Notification::new("textDocument/didOpen".into(), params));
                self.documents.insert(key.clone(), Document { version: 1, text: text.into(), diagnostics: None });
            }
        }
        // Asked in the loop, as a server may register diagnostic requests, or refresh, while the client waits.
        loop {
            if let Some(diagnostics) = &self.documents[&key].diagnostics {
                return Ok(diagnostics.clone());
            }
            self.ask(&key, &uri);
            self.next(deadline)?;
        }
    }

    /// Whether the server takes `root`: the folder it started in, or one added since.
    pub fn serves(&self, root: &Path) -> bool {
        self.folders.iter().any(|f| f == root)
    }

    /// Adds `root` to the server's workspace folders, waiting up to `wait` for the server to start;
    /// `false` when the server does not take changes to its folders.
    pub fn add_folder(&mut self, root: &Path, wait: Duration) -> Result<bool, Unavailable> {
        self.wait_ready(Instant::now() + wait)?;
        if self.serves(root) {
            return Ok(true);
        }
        let Some(folder) = workspace_folder(root).filter(|_| self.takes_folders) else { return Ok(false) };
        let event = WorkspaceFoldersChangeEvent { added: vec![folder], removed: vec![] };
        let params = DidChangeWorkspaceFoldersParams { event };
        send(&self.writer, Notification::new("workspace/didChangeWorkspaceFolders".into(), params));
        self.folders.push(root.to_path_buf());
        Ok(true)
    }

    fn wait_ready(&mut self, deadline: Instant) -> Result<(), Unavailable> {
        while !self.ready {
            self.next(deadline)?;
        }
        Ok(())
    }
    /// Asks a server that answers diagnostic requests about the document's current version, once.
    fn ask(&mut self, key: &str, uri: &Url) {
        let version = self.documents[key].version;
        if self.pull_ids.is_empty() || self.asked.values().any(|(k, v)| (k.as_str(), *v) == (key, version)) {
            return;
        }
        let id = RequestId::from(self.next_id);
        self.next_id += 1;
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        send(&self.writer, Request::new(id.clone(), "textDocument/diagnostic".into(), params));
        self.asked.insert(id, (key.to_string(), version));
    }

    /// Takes in the next event from the server, waiting until `deadline`.
    fn next(&mut self, deadline: Instant) -> Result<(), Unavailable> {
        let event =
            self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => Unavailable::Late,
                mpsc::RecvTimeoutError::Disconnected => Unavailable::Closed,
            })?;
        self.take(event);
        Ok(())
    }

    fn take(&mut self, event: Event) {
        let (key, version, diagnostics) = match event {
            Event::Ready { pulls, folders } => {
                (self.ready, self.takes_folders) = (true, folders);
                if pulls {
                    self.pull_ids.insert("initialize".into());
                }
                return;
            }
            Event::Registered(id) => {
                self.pull_ids.insert(id);
                return;
            }
            Event::Unregistered(id) => {
                self.pull_ids.remove(&id);
                return;
            }
            Event::Refresh => {
                self.documents.values_mut().for_each(|document| document.diagnostics = None);
                return;
            }
            Event::Published(published) => (key(&published.uri), published.version, Some(published.diagnostics)),
            Event::Answered(id, diagnostics) => match self.asked.remove(&id) {
                Some((key, version)) => (Some(key), Some(version), diagnostics),
                None => return,
            },
        };
        // Diagnostics for an older version of the text are about a text no longer asked about.
        if let (Some(document), Some(diagnostics)) = (key.and_then(|k| self.documents.get_mut(&k)), diagnostics)
            && version.is_none_or(|v| v == document.version)
        {
            document.diagnostics = Some(diagnostics);
        }
    }
}

impl Drop for Client {
    // shortcut: kills only the program started, so a server started through a wrapper (mise exec) may
    // outlive it on Windows; kill the process tree if servers pile up.
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
        }
    }
}

/// `root` as a workspace folder, named after its last component.
fn workspace_folder(root: &Path) -> Option<WorkspaceFolder> {
    let name = root.file_name().map_or("root".into(), |n| n.to_string_lossy().into_owned());
    Url::from_directory_path(root).ok().map(|uri| WorkspaceFolder { uri, name })
}

/// A document's identity: its file path with the drive letter lowercased, as Windows rust-analyzer echoes
/// `file:///C:/...` back as `file:///c:/...`; going through the path also undoes `%3A` for `:`.
fn key(uri: &Url) -> Option<String> {
    let path = uri.to_file_path().ok()?.to_string_lossy().into_owned();
    Some(match path.as_bytes() {
        [drive, b':', ..] => format!("{}{}", drive.to_ascii_lowercase() as char, &path[1..]),
        _ => path,
    })
}
/// Reads the server's messages and passes on what they mean.
fn read(mut reader: impl BufRead, writer: &Writer, events: &Sender<Event>) {
    while let Ok(Some(message)) = Message::read(&mut reader) {
        for event in events_of(message, writer) {
            if events.send(event).is_err() {
                return;
            }
        }
    }
}

/// What a message from the server means to the client. Answers the server's requests, and sends `initialized`
/// after the `initialize` answer.
fn events_of(message: Message, writer: &Writer) -> Vec<Event> {
    match message {
        Message::Response(r) if r.id == INITIALIZE.into() => {
            send(writer, Notification::new("initialized".into(), serde_json::json!({})));
            let none = Value::Null;
            let capabilities = r.response_result.as_ref().map_or(&none, |result| &result["capabilities"]);
            // `changeNotifications` is `true`, or the id the server registers the notification under.
            let changes = &capabilities["workspace"]["workspaceFolders"]["changeNotifications"];
            let folders = changes == true || changes.is_string();
            vec![Event::Ready { pulls: !capabilities["diagnosticProvider"].is_null(), folders }]
        }
        // Only diagnostic requests are sent after initialize. A full report holds `items`; an error or an
        // unchanged report, which needs a previous result id never sent, holds none.
        Message::Response(r) => {
            let items = r.response_result.ok().and_then(|result| serde_json::from_value(result["items"].clone()).ok());
            vec![Event::Answered(r.id, items)]
        }
        Message::Request(r) => {
            send(writer, Response::new_ok(r.id.clone(), answer(&r)));
            match r.method.as_str() {
                "workspace/diagnostic/refresh" => vec![Event::Refresh],
                "client/registerCapability" => {
                    diagnostic_ids(&r.params["registrations"]).map(Event::Registered).collect()
                }
                // LSP spells the list `unregisterations`.
                "client/unregisterCapability" => {
                    diagnostic_ids(&r.params["unregisterations"]).map(Event::Unregistered).collect()
                }
                _ => vec![],
            }
        }
        Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
            serde_json::from_value(n.params).map(Event::Published).into_iter().collect()
        }
        _ => vec![],
    }
}

/// The ids of the registrations of diagnostic requests among `registrations`.
fn diagnostic_ids(registrations: &Value) -> impl Iterator<Item = String> + '_ {
    let diagnostic = |r: &&Value| r["method"] == "textDocument/diagnostic";
    registrations.as_array().into_iter().flatten().filter(diagnostic).filter_map(|r| r["id"].as_str().map(String::from))
}

/// `workspace/configuration` asks for a value per item; there are no settings to give, so each is null.
/// Every other request is answered with null too, which servers take as "nothing to report".
fn answer(request: &Request) -> Value {
    match request.method.as_str() {
        "workspace/configuration" => {
            Value::Array(vec![Value::Null; request.params["items"].as_array().map_or(0, Vec::len)])
        }
        _ => Value::Null,
    }
}

fn send(writer: &Writer, message: impl Into<Message>) {
    // A server that died shows up as diagnostics that never arrive.
    let _ = message.into().write(&mut *writer.lock().unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_server::{Notification, Request, Response};
    use serde_json::{Value, json};
    use std::io::BufReader;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// What the fake server sends back for each message from the client.
    type Handler = Box<dyn FnMut(&Message) -> Vec<Message> + Send>;

    fn initialize_answer(message: &Message) -> Option<Message> {
        match message {
            Message::Request(r) if r.method == "initialize" => {
                Some(Response::new_ok(r.id.clone(), json!({"capabilities": {}})).into())
            }
            _ => None,
        }
    }

    fn publish(uri: &Value, message: &Value) -> Message {
        let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
        let diagnostic = json!({"range": range, "severity": 1, "message": message});
        Notification::new("textDocument/publishDiagnostics".into(), json!({"uri": uri, "diagnostics": [diagnostic]}))
            .into()
    }

    /// Answers initialize at once and, for each opened or changed document, publishes one diagnostic whose
    /// message is the document's text.
    fn echo() -> Handler {
        Box::new(|m| {
            let Message::Notification(n) = m else { return initialize_answer(m).into_iter().collect() };
            match n.method.as_str() {
                "textDocument/didOpen" => {
                    vec![publish(&n.params["textDocument"]["uri"], &n.params["textDocument"]["text"])]
                }
                "textDocument/didChange" => {
                    vec![publish(&n.params["textDocument"]["uri"], &n.params["contentChanges"][0]["text"])]
                }
                _ => vec![],
            }
        })
    }

    fn method(message: &Message) -> String {
        match message {
            Message::Request(r) => r.method.clone(),
            Message::Notification(n) => n.method.clone(),
            Message::Response(r) => format!("response {}", r.response_result.clone().unwrap_or_default()),
        }
    }

    /// A client talking to a fake server on another thread, and what the server received, in order.
    fn fake(mut handler: Handler) -> (Client, Arc<Mutex<Vec<String>>>) {
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (server_reads, client_writes) = std::io::pipe().unwrap();
        let received = Arc::new(Mutex::new(vec![]));
        let log = received.clone();
        std::thread::spawn(move || {
            let (mut reader, mut writer) = (BufReader::new(server_reads), server_writes);
            while let Ok(Some(message)) = Message::read(&mut reader) {
                log.lock().unwrap().push(method(&message));
                for out in handler(&message) {
                    if out.write(&mut writer).is_err() {
                        return;
                    }
                }
            }
        });
        let client = Client::connect(BufReader::new(client_reads), client_writes, &std::env::temp_dir(), "rust");
        (client, received)
    }

    fn doc() -> PathBuf {
        std::env::temp_dir().join("a.rs")
    }

    const LONG: Duration = Duration::from_secs(5);
    const SHORT: Duration = Duration::from_millis(200);

    fn messages(diagnostics: Result<Vec<Diagnostic>, Unavailable>) -> Result<Vec<String>, Unavailable> {
        diagnostics.map(|d| d.into_iter().map(|d| d.message).collect())
    }

    #[test]
    fn connecting_sends_initialize_and_then_initialized() {
        // Given: 偽のサーバーにつないだクライアント
        let (mut client, received) = fake(echo());
        // When: 診断を問い合わせる
        let _ = client.diagnostics(&doc(), "x", LONG);
        // Then: サーバーは initialize、initialized の順に受け取っている
        assert_eq!(received.lock().unwrap()[..2], ["initialize", "initialized"]);
    }

    #[test]
    fn diagnostics_before_initialize_answer_are_none() {
        // Given: initialize に答えないサーバー
        let (mut client, _) = fake(Box::new(|_| vec![]));
        // When / Then: 診断の問い合わせには、届かなかったと答える
        assert_eq!(messages(client.diagnostics(&doc(), "x", SHORT)), Err(Unavailable::Late));
    }

    #[test]
    fn first_document_is_opened_and_its_diagnostics_returned() {
        // Given: 偽のサーバーにつないだクライアント
        let (mut client, received) = fake(echo());
        // When: 初めての文書の診断を問い合わせる
        let found = messages(client.diagnostics(&doc(), "x", LONG));
        // Then: didOpen で渡し、その文書に届いた診断を返す
        assert_eq!(found, Ok(vec!["x".to_string()]));
        assert!(received.lock().unwrap().contains(&"textDocument/didOpen".to_string()));
    }

    #[test]
    fn same_text_is_answered_from_memory() {
        // Given: 一度問い合わせた文書
        let (mut client, received) = fake(echo());
        let _ = client.diagnostics(&doc(), "x", LONG);
        // When: 同じ内容でもう一度問い合わせる
        let found = messages(client.diagnostics(&doc(), "x", LONG));
        // Then: サーバーに送らずに、覚えている診断を返す
        assert_eq!(found, Ok(vec!["x".to_string()]));
        let sent = received.lock().unwrap().iter().filter(|m| m.starts_with("textDocument/did")).count();
        assert_eq!(sent, 1);
    }

    #[test]
    fn changed_text_is_sent_with_did_change() {
        // Given: 一度問い合わせた文書
        let (mut client, received) = fake(echo());
        let _ = client.diagnostics(&doc(), "x", LONG);
        // When: 違う内容で問い合わせる
        let found = messages(client.diagnostics(&doc(), "y", LONG));
        // Then: didChange で渡し、新しい診断を返す
        assert_eq!(found, Ok(vec!["y".to_string()]));
        assert!(received.lock().unwrap().contains(&"textDocument/didChange".to_string()));
    }

    #[test]
    fn no_diagnostics_in_time_is_none() {
        // Given: initialize には答えるが、診断を送らないサーバー
        let (mut client, _) = fake(Box::new(|m| initialize_answer(m).into_iter().collect()));
        // When / Then: 待つ時間のうちに届かないので、届かなかったと答える
        assert_eq!(messages(client.diagnostics(&doc(), "x", SHORT)), Err(Unavailable::Late));
    }

    #[test]
    fn diagnostics_from_exited_server_are_closed() {
        // Given: つないだとたんに終了したサーバー
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (_, client_writes) = std::io::pipe().unwrap();
        drop(server_writes);
        let mut client = Client::connect(BufReader::new(client_reads), client_writes, &std::env::temp_dir(), "rust");
        // When / Then: 診断の問い合わせには、閉じたと答える
        assert_eq!(messages(client.diagnostics(&doc(), "x", LONG)), Err(Unavailable::Closed));
    }

    #[test]
    fn diagnostics_of_another_document_are_not_returned() {
        // Given: 開かれた文書ではなく、別の文書の診断を送るサーバー
        let (mut client, _) = fake(Box::new(|m| match m {
            Message::Notification(n) if n.method == "textDocument/didOpen" => {
                vec![publish(&json!("file:///other.rs"), &json!("other"))]
            }
            _ => initialize_answer(m).into_iter().collect(),
        }));
        // When / Then: 問い合わせた文書の診断としては返さない
        assert_eq!(messages(client.diagnostics(&doc(), "x", SHORT)), Err(Unavailable::Late));
    }

    #[test]
    fn diagnostics_for_uri_with_other_drive_letter_case_are_the_documents() {
        // Given: 開かれた文書の URI のドライブ名を小文字にして診断を送るサーバー（Windows の rust-analyzer と同じ）
        let (mut client, _) = fake(Box::new(|m| match m {
            Message::Notification(n) if n.method == "textDocument/didOpen" => {
                let uri = n.params["textDocument"]["uri"].as_str().unwrap();
                let lowered = match uri.strip_prefix("file:///") {
                    Some(rest) if rest.as_bytes().get(1) == Some(&b':') => {
                        format!("file:///{}{}", rest[..1].to_ascii_lowercase(), &rest[1..])
                    }
                    _ => uri.to_string(),
                };
                vec![publish(&json!(lowered), &json!("x"))]
            }
            _ => initialize_answer(m).into_iter().collect(),
        }));
        // When: 診断を問い合わせる
        let found = messages(client.diagnostics(&doc(), "x", LONG));
        // Then: その文書の診断として受け取る
        assert_eq!(found, Ok(vec!["x".to_string()]));
    }

    /// Answers initialize with `diagnosticProvider` only when the client says it can pull diagnostics, as
    /// pyright does, never publishes, and answers the `n`th diagnostic request with the message "n".
    fn puller() -> Handler {
        let mut answered = 0;
        Box::new(move |m| match m {
            Message::Request(r) if r.method == "initialize" => {
                let pulls = !r.params["capabilities"]["textDocument"]["diagnostic"].is_null();
                let capabilities = if pulls { json!({"diagnosticProvider": {}}) } else { json!({}) };
                vec![Response::new_ok(r.id.clone(), json!({ "capabilities": capabilities })).into()]
            }
            Message::Request(r) if r.method == "textDocument/diagnostic" => {
                answered += 1;
                let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
                let items = json!([{"range": range, "severity": 1, "message": answered.to_string()}]);
                vec![Response::new_ok(r.id.clone(), json!({"kind": "full", "items": items})).into()]
            }
            _ => vec![],
        })
    }

    /// Like `puller`, but says nothing of diagnostics in its initialize answer: it registers diagnostic
    /// requests after `initialized` with the id "a", as pyright does, and with `unregister` takes "a" back
    /// before it answers the first request.
    fn registering(unregister: bool) -> Handler {
        let mut puller = puller();
        let a = json!([{"id": "a", "method": "textDocument/diagnostic"}]);
        let mut registered = true;
        Box::new(move |m| match m {
            Message::Request(r) if r.method == "initialize" => {
                vec![Response::new_ok(r.id.clone(), json!({"capabilities": {}})).into()]
            }
            Message::Notification(n) if n.method == "initialized" => {
                vec![Request::new(9.into(), "client/registerCapability".into(), json!({"registrations": a})).into()]
            }
            Message::Request(r) if r.method == "textDocument/diagnostic" && unregister && registered => {
                registered = false;
                let params = json!({"unregisterations": a});
                let mut out = vec![Request::new(10.into(), "client/unregisterCapability".into(), params).into()];
                out.extend(puller(m));
                out
            }
            _ => puller(m),
        })
    }

    #[test]
    fn server_that_registers_diagnostic_requests_later_is_asked() {
        // Given: initialize の後で、診断の要求を登録するサーバー
        let (mut client, _) = fake(registering(false));
        // When: 診断を問い合わせる
        let found = messages(client.diagnostics(&doc(), "x", LONG));
        // Then: 要求を送り、その答えを返す
        assert_eq!(found, Ok(vec!["1".to_string()]));
    }

    #[test]
    fn server_that_took_back_its_registration_is_not_asked() {
        // Given: 最初の要求に答える前に、診断の要求の登録を取り消すサーバー
        let (mut client, received) = fake(registering(true));
        let _ = client.diagnostics(&doc(), "x", LONG);
        // When: 違う内容で問い合わせる
        let _ = client.diagnostics(&doc(), "y", SHORT);
        // Then: 2 回目の要求は送らない
        let asked = received.lock().unwrap().iter().filter(|m| *m == "textDocument/diagnostic").count();
        assert_eq!(asked, 1);
    }

    #[test]
    fn server_that_answers_diagnostic_requests_is_asked() {
        // Given: 診断を送ってこず、診断の要求に答えるサーバー
        let (mut client, received) = fake(puller());
        // When: 文書を開いて問い合わせ、違う内容でもう一度問い合わせる
        let first = messages(client.diagnostics(&doc(), "x", LONG));
        let second = messages(client.diagnostics(&doc(), "y", LONG));
        // Then: そのたびに textDocument/diagnostic を送り、その答えを返す
        assert_eq!((first, second), (Ok(vec!["1".to_string()]), Ok(vec!["2".to_string()])));
        let asked = received.lock().unwrap().iter().filter(|m| *m == "textDocument/diagnostic").count();
        assert_eq!(asked, 2);
    }

    #[test]
    fn same_text_is_not_asked_again() {
        // Given: 診断の要求に答えるサーバーに、一度問い合わせた文書
        let (mut client, received) = fake(puller());
        let _ = client.diagnostics(&doc(), "x", LONG);
        // When: 同じ内容でもう一度問い合わせる
        let found = messages(client.diagnostics(&doc(), "x", LONG));
        // Then: 要求を送らずに、覚えている答えを返す
        assert_eq!(found, Ok(vec!["1".to_string()]));
        let asked = received.lock().unwrap().iter().filter(|m| *m == "textDocument/diagnostic").count();
        assert_eq!(asked, 1);
    }

    #[test]
    fn refresh_drops_remembered_diagnostics() {
        // Given: 診断の要求に答え、最初の答えのすぐ後に workspace/diagnostic/refresh を送るサーバー
        let mut puller = puller();
        let mut refreshed = false;
        let (mut client, _) = fake(Box::new(move |m| {
            let mut out = puller(m);
            if !refreshed && matches!(m, Message::Request(r) if r.method == "textDocument/diagnostic") {
                refreshed = true;
                out.push(Request::new(8.into(), "workspace/diagnostic/refresh".into(), json!(null)).into());
            }
            out
        }));
        let _ = client.diagnostics(&doc(), "x", LONG);
        std::thread::sleep(SHORT);
        // When: refresh が届いた後に、同じ内容で問い合わせる
        let found = messages(client.diagnostics(&doc(), "x", LONG));
        // Then: 覚えている答えを捨てて、要求し直した答えを返す
        assert_eq!(found, Ok(vec!["2".to_string()]));
    }

    #[test]
    fn server_without_diagnostic_provider_is_not_asked() {
        // Given: 診断を送ってくる、診断の要求に答えないサーバー
        let (mut client, received) = fake(echo());
        // When: 診断を問い合わせる
        let _ = client.diagnostics(&doc(), "x", LONG);
        // Then: textDocument/diagnostic は送らない
        assert!(!received.lock().unwrap().contains(&"textDocument/diagnostic".to_string()));
    }

    #[test]
    fn answer_for_older_text_is_not_returned() {
        // Given: 診断の要求に、次の要求が来てから、前の要求の分とまとめて答えるサーバー
        let mut waiting: Vec<lsp_server::RequestId> = vec![];
        let (mut client, _) = fake(Box::new(move |m| match m {
            Message::Request(r) if r.method == "textDocument/diagnostic" => {
                waiting.push(r.id.clone());
                if waiting.len() < 2 {
                    return vec![];
                }
                let range = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
                let answer = |id: &lsp_server::RequestId, text: &str| -> Message {
                    let items = json!([{"range": range, "severity": 1, "message": text}]);
                    Response::new_ok(id.clone(), json!({"kind": "full", "items": items})).into()
                };
                vec![answer(&waiting[0], "x"), answer(&waiting[1], "y")]
            }
            Message::Request(r) if r.method == "initialize" => {
                vec![Response::new_ok(r.id.clone(), json!({"capabilities": {"diagnosticProvider": {}}})).into()]
            }
            _ => vec![],
        }));
        // When: x の答えが来ないうちに、y で問い合わせる
        assert_eq!(messages(client.diagnostics(&doc(), "x", SHORT)), Err(Unavailable::Late));
        let found = messages(client.diagnostics(&doc(), "y", LONG));
        // Then: x への答えは捨て、y への答えを返す
        assert_eq!(found, Ok(vec!["y".to_string()]));
    }
    /// Like `echo`, but answers initialize saying it takes changes to its workspace folders, when the client
    /// says it has them; gopls gives a registration id for `changeNotifications` instead of `true`.
    fn foldering(change_notifications: Value) -> Handler {
        let mut echo = echo();
        Box::new(move |m| match m {
            Message::Request(r) if r.method == "initialize" => {
                let folders = !r.params["capabilities"]["workspace"]["workspaceFolders"].is_null();
                let workspace =
                    json!({"workspaceFolders": {"supported": true, "changeNotifications": change_notifications}});
                let capabilities = if folders { json!({ "workspace": workspace }) } else { json!({}) };
                vec![Response::new_ok(r.id.clone(), json!({ "capabilities": capabilities })).into()]
            }
            _ => echo(m),
        })
    }

    /// What the fake server received, once it has taken in everything sent before: a diagnostics question is
    /// answered only after the messages before it.
    fn all_received(client: &mut Client, received: &Mutex<Vec<String>>) -> Vec<String> {
        let _ = client.diagnostics(&std::env::temp_dir().join("round-trip.rs"), "x", LONG);
        received.lock().unwrap().clone()
    }

    #[test]
    fn folder_is_added_to_server_that_takes_folder_changes() {
        for change_notifications in [json!(true), json!("workspace/didChangeWorkspaceFolders")] {
            // Given: workspace folder の変更に対応するサーバー
            let (mut client, received) = fake(foldering(change_notifications));
            let other = std::env::temp_dir().join("other");
            // When: フォルダーを足す
            let added = client.add_folder(&other, LONG);
            // Then: didChangeWorkspaceFolders を送り、起動したときのフォルダーと足したフォルダーを受け持つ
            assert_eq!(added, Ok(true));
            assert!(all_received(&mut client, &received).contains(&"workspace/didChangeWorkspaceFolders".to_string()));
            assert!(client.serves(&other) && client.serves(&std::env::temp_dir()));
        }
    }

    #[test]
    fn folder_is_not_added_to_server_without_folder_changes() {
        // Given: workspace folder の変更に対応しないサーバー
        let (mut client, received) = fake(echo());
        let other = std::env::temp_dir().join("other");
        // When: フォルダーを足す
        let added = client.add_folder(&other, LONG);
        // Then: 足せないと答え、受け持たず、何も送らない
        assert_eq!(added, Ok(false));
        assert!(!client.serves(&other));
        assert!(!all_received(&mut client, &received).contains(&"workspace/didChangeWorkspaceFolders".to_string()));
    }

    #[test]
    fn folder_served_already_is_not_sent_again() {
        // Given: workspace folder の変更に対応するサーバー
        let (mut client, received) = fake(foldering(json!(true)));
        let other = std::env::temp_dir().join("other");
        // When: 起動したときのフォルダー、別のフォルダー、同じ別のフォルダーの順に足す
        let added: Vec<_> =
            [std::env::temp_dir(), other.clone(), other].iter().map(|f| client.add_folder(f, LONG)).collect();
        // Then: どれも足せたと答え、通知は別のフォルダーを足したときの 1 回だけ
        assert_eq!(added, [Ok(true), Ok(true), Ok(true)]);
        let all = all_received(&mut client, &received);
        let sent = all.iter().filter(|m| *m == "workspace/didChangeWorkspaceFolders").count();
        assert_eq!(sent, 1);
    }

    #[test]
    fn server_requests_are_answered() {
        // Given: initialized を受けると、項目が 2 つの workspace/configuration を要求するサーバー
        let (mut client, received) = fake(Box::new(|m| match m {
            Message::Notification(n) if n.method == "initialized" => {
                let params = json!({"items": [{"section": "a"}, {"section": "b"}]});
                vec![Request::new(7.into(), "workspace/configuration".into(), params).into()]
            }
            _ => initialize_answer(m).into_iter().collect(),
        }));
        // When: 診断を問い合わせて、やりとりを進める
        let _ = client.diagnostics(&doc(), "x", SHORT);
        // Then: 項目の数だけ null を並べた応答を返す
        assert!(
            received.lock().unwrap().contains(&"response [null,null]".to_string()),
            "{:?}",
            received.lock().unwrap()
        );
    }
}
