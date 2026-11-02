use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Map, json};

use crate::command::{self, Mode, WriteRequest, Written};
use crate::language::Languages;
use crate::lsp::Servers;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    /// File path or glob.
    pub path: String,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M).
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    /// File path or glob.
    pub path: String,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M).
    pub scope: Option<String>,
    /// Code to find. Omit to target the whole `scope` (or the whole file).
    pub before: Option<String>,
    /// Code to write. Empty deletes the target.
    pub after: String,
    /// Edit every match instead of failing as ambiguous.
    #[serde(default)]
    pub all_matches: bool,
    /// What to do with each match (or with the whole target when `before` is omitted).
    #[serde(default)]
    pub mode: Mode,
}

/// What a file looked like when it was last read or written through this server.
type Stamp = (Option<SystemTime>, u64);

fn stamp(path: &str) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

#[derive(Clone)]
pub struct Server {
    langs: Arc<Languages>,
    stamps: Arc<Mutex<HashMap<String, Stamp>>>,
    /// The language servers started for writes; only `serve` has them, so tests start none.
    lsp: Option<Arc<Mutex<Servers>>>,
    #[allow(dead_code, reason = "used by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Server {
    pub fn new(langs: Languages) -> Self {
        Server { langs: Arc::new(langs), stamps: Default::default(), lsp: None, tool_router: Self::tool_router() }
    }

    fn remember(&self, path: &str) {
        if let Some(s) = stamp(path) {
            self.stamps.lock().unwrap().insert(path.to_string(), s);
        }
    }

    /// Line numbers from an earlier read may be wrong if the file changed since.
    fn is_stale(&self, path: &str) -> bool {
        self.stamps.lock().unwrap().get(path).is_some_and(|s| stamp(path).as_ref() != Some(s))
    }

    #[tool(description = "Read a source file with line numbers (`N<TAB>line`). Function and method bodies are \
        hidden: they leave gaps in the line numbers, or show as `...` when they share a line with other code.\n\n\
        To see more, pass `scope`: an identifier such as `Foo.bar` or `Foo::bar` (a function shows in full; a class \
        shows its members with bodies hidden), or a line range `N` or `N-M` (shown in full). `path` may be a glob.")]
    fn read(&self, Parameters(p): Parameters<ReadParams>) -> CallToolResult {
        match command::read(&self.langs, &p.path, p.scope.as_deref()) {
            Ok(text) => {
                self.remember(&p.path);
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Err(e) => error(e),
        }
    }

    #[tool(description = "Edit a source file: find `before` and replace it with `after`, or insert `after` next \
        to it (see `mode`).\n\n\
        `before` is matched ignoring indentation, line breaks and spacing, and must match exactly once in `scope` \
        unless `all_matches` is set; if it is ambiguous, re-run with a candidate's line range as `scope`. Without \
        `before`, the target is the whole `scope` (a definition with its docs, or a line range), or the whole file \
        without `scope` (created if missing).\n\n\
        `after` is re-indented to fit, so its indentation does not need to match the file. An empty `after` deletes \
        the target.\n\n\
        On success, `replaced` gives the line ranges that now hold `after`; there is no need to read the file back.")]
    fn write(&self, Parameters(p): Parameters<WriteParams>) -> CallToolResult {
        let req = WriteRequest {
            path: &p.path,
            scope: p.scope.as_deref(),
            before: p.before.as_deref(),
            after: &p.after,
            all_matches: p.all_matches,
            place: p.mode.into(),
        };
        let stale = p.scope.as_deref().and_then(crate::scope::parse_lines).is_some() && self.is_stale(&p.path);
        // shortcut: waits for diagnostics on the runtime's only thread; fine while one agent writes at a time.
        let mut check =
            |written: &Written| self.lsp.as_ref().map_or_else(Map::new, |s| s.lock().unwrap().check(written));
        match command::write(&self.langs, &req, &mut check) {
            Ok(mut reports) => {
                for r in reports.iter_mut().filter(|r| r["ok"] == true) {
                    if stale {
                        r["warnings"]["stale"] = json!("modified since last read");
                    }
                    self.remember(r["file"].as_str().unwrap_or_default());
                }
                let ok = !reports.iter().any(|r| r["ok"] == false);
                let text = vec![ContentBlock::text(serde_json::Value::from(reports).to_string())];
                if ok { CallToolResult::success(text) } else { CallToolResult::error(text) }
            }
            Err(e) => error(e),
        }
    }
}

fn error(e: crate::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_json().to_string())])
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("edo", env!("CARGO_PKG_VERSION")))
    }
}

/// Serves MCP over stdio until the client disconnects.
pub fn serve(langs: Languages) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let server = Server { lsp: Some(Default::default()), ..Server::new(langs) };
        let running = server.serve(rmcp::transport::stdio()).await?;
        running.waiting().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolRequestParams, ClientConfig};
    use serde_json::Value;

    use super::*;

    #[derive(Clone, Default)]
    struct Client;

    impl rmcp::ClientHandler for Client {
        fn get_info(&self) -> ClientConfig {
            ClientConfig::default()
        }
    }

    /// Connects a client to a fresh server over in-memory pipes.
    async fn connect() -> rmcp::service::RunningService<rmcp::RoleClient, Client> {
        let (client_write, server_read) = tokio::io::duplex(1 << 16);
        let (server_write, client_read) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let running = Server::new(Languages::builtin()).serve((server_read, server_write)).await.unwrap();
            let _ = running.waiting().await;
        });
        Client.serve((client_read, client_write)).await.unwrap()
    }

    #[tokio::test]
    async fn tool_list_has_read_and_write() {
        // Given: MCP サーバーにつないだクライアント
        let client = connect().await;
        // When: ツールの一覧を求める
        let tools = client.list_all_tools().await.unwrap();
        // Then: read と write が返る
        let names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(names, ["read", "write"]);
    }

    fn file(text: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, text).unwrap();
        (dir, path.to_string_lossy().replace('\\', "/"))
    }

    async fn tool(
        client: &rmcp::service::RunningService<rmcp::RoleClient, Client>,
        name: &str,
        args: Value,
    ) -> (String, bool) {
        let params = CallToolRequestParams::new(name.to_string()).with_arguments(args.as_object().unwrap().clone());
        let result = client.call_tool(params).await.unwrap();
        let text = result.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default();
        (text, result.is_error == Some(true))
    }

    #[tokio::test]
    async fn read_tool_takes_path_and_scope_and_returns_text() {
        // Given: 関数が 2 つあるファイル
        let (_dir, path) = file("fn a() {\n    x();\n}\nfn b() {}\n");
        let client = connect().await;
        // When: path と scope を渡して read を呼ぶ
        let (text, _) = tool(&client, "read", json!({"path": path, "scope": "a"})).await;
        // Then: その定義のテキストが返る
        assert_eq!(text, "1\tfn a() {\n2\t    x();\n3\t}\n");
    }

    #[tokio::test]
    async fn write_tool_mode_inserts_after_match() {
        // Given: 呼び出しのある関数
        let (_dir, path) = file("fn a() {\n    x();\n}\n");
        let client = connect().await;
        // When: mode に insert-after-matches を渡して write を呼ぶ
        let args = json!({"path": path, "before": "x();", "after": "y();", "mode": "insert-after-matches"});
        tool(&client, "write", args).await;
        // Then: 一致の後ろの行に入る
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    x();\n    y();\n}\n");
    }

    #[tokio::test]
    async fn write_tool_returns_same_json_as_cli() {
        // Given: 呼び出しのある関数
        let (_dir, path) = file("fn a() { x(); }\n");
        let client = connect().await;
        // When: write を呼ぶ
        let (text, _) = tool(&client, "write", json!({"path": path, "before": "x();", "after": "y();"})).await;
        // Then: CLI と同じ JSON が返る
        assert_eq!(text, json!([{"ok": true, "file": path, "replaced": [[1, 1]]}]).to_string());
    }

    #[tokio::test]
    async fn failed_call_sets_is_error() {
        // Given: MCP サーバーにつないだクライアント
        let client = connect().await;
        // When: 存在しないファイルを読む
        let (_, is_error) = tool(&client, "read", json!({"path": "missing.rs"})).await;
        // Then: isError が立つ
        assert!(is_error);
    }

    #[tokio::test]
    async fn path_accepts_windows_absolute_path() {
        // Given: ドライブレター付きの絶対パスで指せるファイル
        let dir = tempfile::tempdir().unwrap();
        let path = std::path::absolute(dir.path().join("a.rs")).unwrap();
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let client = connect().await;
        // When: 絶対パスをそのまま path に渡して読む
        let (text, _) = tool(&client, "read", json!({"path": path.to_string_lossy()})).await;
        // Then: 読める
        assert_eq!(text, "1\tfn a() {}\n");
    }

    fn report(text: &str) -> Value {
        serde_json::from_str::<Value>(text).unwrap()[0].clone()
    }

    #[tokio::test]
    async fn line_range_write_to_file_changed_since_read_warns_stale() {
        // Given: read した後に外で書き換えられたファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        tool(&client, "read", json!({"path": path})).await;
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        // When: 行範囲のスコープで書く
        let (text, _) = tool(&client, "write", json!({"path": path, "scope": "2", "after": "fn c() {}"})).await;
        // Then: stale の警告が付く
        assert_eq!(report(&text)["warnings"]["stale"], "modified since last read");
    }

    #[tokio::test]
    async fn line_range_write_after_own_write_has_no_stale_warning() {
        // Given: read した後に edo 自身の write で書き換えたファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        tool(&client, "read", json!({"path": path})).await;
        tool(&client, "write", json!({"path": path, "scope": "1", "after": "fn a() {}\nfn b() {}"})).await;
        // When: 行範囲のスコープでもう一度書く
        let (text, _) = tool(&client, "write", json!({"path": path, "scope": "2", "after": "fn c() {}"})).await;
        // Then: stale の警告は付かない
        assert!(report(&text).get("warnings").is_none(), "{text}");
    }

    #[tokio::test]
    async fn identifier_scope_write_has_no_stale_warning() {
        // Given: read した後に外で書き換えられたファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        tool(&client, "read", json!({"path": path})).await;
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        // When: 識別子のスコープで書く
        let (text, _) = tool(&client, "write", json!({"path": path, "scope": "b", "after": "fn c() {}"})).await;
        // Then: stale の警告は付かない
        assert!(report(&text).get("warnings").is_none(), "{text}");
    }
}
