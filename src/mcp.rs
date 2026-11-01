use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::command::{self, Mode, WriteRequest};
use crate::language::Languages;

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
    /// Code to find; omit to target the whole scope (or the whole file).
    pub before: Option<String>,
    /// Replacement code. Empty deletes.
    pub after: String,
    /// Replace every match instead of failing as ambiguous.
    #[serde(default)]
    pub all_matches: bool,
    /// What to do with each match (or the whole target without `before`). Defaults to replace-matches.
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
    #[allow(dead_code, reason = "used by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Server {
    pub fn new(langs: Languages) -> Self {
        Server { langs: Arc::new(langs), stamps: Default::default(), tool_router: Self::tool_router() }
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
        elided to save tokens: lines fully inside a body are omitted (watch for gaps in the line numbers) and a body \
        sharing a line with other code is shown as `...`. Docs and signatures stay visible. Pass `scope` to see more: \
        an identifier such as `Foo.bar` or `Foo::bar` (a function shows its full body; a class shows its members with \
        their bodies elided), or a line range `N` / `N-M` (shown in full). `path` may be a glob such as \
        `src/**/*.kt`; each file then starts with its path on its own line, and files without a language package \
        are skipped and counted by extension on a last `{\"skipped\":...}` line. A single file without a \
        language package is an `unsupported_language` error.")]
    fn read(&self, Parameters(p): Parameters<ReadParams>) -> CallToolResult {
        match command::read(&self.langs, &p.path, p.scope.as_deref()) {
            Ok(text) => {
                self.remember(&p.path);
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Err(e) => error(e),
        }
    }

    #[tool(description = "Edit a source file. `before` is matched against the file's syntax tree, ignoring \
        indentation, line breaks and spacing (and, per language, quote style, optional semicolons and trailing commas); \
        it may start or end in the middle of a token. It must match exactly once in `scope` unless `all_matches` is \
        set. `after` is re-indented to fit where it lands, so its indentation does not need to match the file. Without \
        `before`, the whole `scope` (a definition with its docs, or a line range) is replaced; without both, the whole \
        file is written (a missing file is created). An empty `after` deletes. `mode` \
        `insert-before-matches` / `insert-after-matches` puts `after` on new lines before/after each match \
        instead of replacing it. Returns a JSON array with one report per file: \
        `{\"ok\":true,\"replaced\":[[first,last]]}`, or `ambiguous` with candidates (re-run with a candidate's line \
        range as `scope`), or `not_found` with the closest candidate. `warnings` reports syntax errors the edit \
        introduced and line-range edits of files changed since your last read.")]
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
        match command::write(&self.langs, &req) {
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
        let running = Server::new(langs).serve(rmcp::transport::stdio()).await?;
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

    async fn tool(client: &rmcp::service::RunningService<rmcp::RoleClient, Client>, name: &str, args: Value) -> (String, bool) {
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
