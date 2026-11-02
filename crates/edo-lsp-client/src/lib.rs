//! A small LSP client: it starts a language server, keeps each document in step with the text it is asked
//! about, and returns the diagnostics the server publishes for that text.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lsp_server::{Message, Notification, Request, Response};
use lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidOpenTextDocumentParams, InitializeParams,
    PublishDiagnosticsClientCapabilities, PublishDiagnosticsParams, TextDocumentClientCapabilities,
    TextDocumentContentChangeEvent, TextDocumentItem, Url, VersionedTextDocumentIdentifier, WorkspaceFolder,
};
use serde_json::Value;

pub use lsp_types::{Diagnostic, DiagnosticSeverity};

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

const INITIALIZE: i32 = 1;

/// What the reading thread passes on from the server.
enum Event {
    Ready,
    Published(PublishDiagnosticsParams),
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
        let root = Url::from_directory_path(root).ok();
        let diagnostics = PublishDiagnosticsClientCapabilities { version_support: Some(true), ..Default::default() };
        let text_document =
            TextDocumentClientCapabilities { publish_diagnostics: Some(diagnostics), ..Default::default() };
        #[allow(deprecated)] // root_uri, for servers that predate workspace folders
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root.clone(),
            workspace_folders: root.map(|uri| vec![WorkspaceFolder { uri, name: "root".into() }]),
            capabilities: ClientCapabilities { text_document: Some(text_document), ..Default::default() },
            ..Default::default()
        };
        send(&writer, Request::new(INITIALIZE.into(), "initialize".into(), params));
        Client {
            writer,
            events: received,
            ready: false,
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
        while !self.ready {
            self.next(deadline)?;
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
        loop {
            if let Some(diagnostics) = &self.documents[&key].diagnostics {
                return Ok(diagnostics.clone());
            }
            self.next(deadline)?;
        }
    }

    /// Takes in the next event from the server, waiting until `deadline`.
    fn next(&mut self, deadline: Instant) -> Result<(), Unavailable> {
        let event =
            self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => Unavailable::Late,
                mpsc::RecvTimeoutError::Disconnected => Unavailable::Closed,
            })?;
        match event {
            Event::Ready => self.ready = true,
            Event::Published(published) => {
                // Diagnostics for an older version of the text are about a text no longer asked about.
                if let Some(document) = key(&published.uri).and_then(|k| self.documents.get_mut(&k))
                    && published.version.is_none_or(|v| v == document.version)
                {
                    document.diagnostics = Some(published.diagnostics);
                }
            }
        }
        Ok(())
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

/// A document's identity: its file path with the drive letter lowercased, as Windows rust-analyzer echoes
/// `file:///C:/...` back as `file:///c:/...`; going through the path also undoes `%3A` for `:`.
fn key(uri: &Url) -> Option<String> {
    let path = uri.to_file_path().ok()?.to_string_lossy().into_owned();
    Some(match path.as_bytes() {
        [drive, b':', ..] => format!("{}{}", drive.to_ascii_lowercase() as char, &path[1..]),
        _ => path,
    })
}
/// Reads the server's messages: answers its requests, sends `initialized` after the `initialize` answer,
/// and passes on readiness and diagnostics.
fn read(mut reader: impl BufRead, writer: &Writer, events: &Sender<Event>) {
    while let Ok(Some(message)) = Message::read(&mut reader) {
        let event = match message {
            Message::Response(r) if r.id == INITIALIZE.into() => {
                send(writer, Notification::new("initialized".into(), serde_json::json!({})));
                Event::Ready
            }
            Message::Request(r) => {
                send(writer, Response::new_ok(r.id.clone(), answer(&r)));
                continue;
            }
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                match serde_json::from_value(n.params) {
                    Ok(published) => Event::Published(published),
                    Err(_) => continue,
                }
            }
            _ => continue,
        };
        if events.send(event).is_err() {
            return;
        }
    }
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
