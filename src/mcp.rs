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
    pub path: Option<String>,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M).
    pub scope: Option<String>,
    /// More to read in the same call, after `path`.
    #[serde(default)]
    pub reads: Vec<ReadTarget>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadTarget {
    /// File path or glob.
    pub path: String,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M).
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    /// File path or glob.
    pub path: Option<String>,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M).
    pub scope: Option<String>,
    /// Code to find. Omit to target the whole `scope` (or the whole file).
    pub before: Option<String>,
    /// Code to write. Empty deletes the target.
    pub after: Option<String>,
    /// Edit every match instead of failing as ambiguous.
    #[serde(default)]
    pub all_matches: bool,
    /// What to do with each match (or with the whole target when `before` is omitted).
    #[serde(default)]
    pub mode: Mode,
    /// More edits in the same call, after the one given by `path`.
    #[serde(default)]
    pub edits: Vec<EditParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditParams {
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

impl EditParams {
    fn request(&self) -> WriteRequest<'_> {
        WriteRequest {
            path: &self.path,
            scope: self.scope.as_deref(),
            before: self.before.as_deref(),
            after: &self.after,
            all_matches: self.all_matches,
            place: self.mode.into(),
        }
    }
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
    /// Kept by absolute path, so that two spellings of one file share a stamp.
    stamps: Arc<Mutex<HashMap<std::path::PathBuf, Stamp>>>,
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
        if let (Ok(key), Some(s)) = (std::path::absolute(path), stamp(path)) {
            self.stamps.lock().unwrap().insert(key, s);
        }
    }

    /// Line numbers from an earlier read may be wrong if the file changed since.
    fn is_stale(&self, path: &str) -> bool {
        let Ok(key) = std::path::absolute(path) else { return false };
        self.stamps.lock().unwrap().get(&key).is_some_and(|s| stamp(path).as_ref() != Some(s))
    }

    #[tool(description = "Read a source file with line numbers (`N<TAB>line`). Function and method bodies are \
        hidden: they leave gaps in the line numbers, or show as `...` when they share a line with other code.\n\n\
        To see more, pass `scope`: an identifier such as `Foo.bar` or `Foo::bar` (a function shows in full; a class \
        shows its members with bodies hidden), or a line range `N` or `N-M` (shown in full). `path` may be a glob.\n\n\
        To read several things in one call, list them in `reads` as `{path, scope}`; each one's text follows its \
        path.")]
    fn read(&self, Parameters(p): Parameters<ReadParams>) -> CallToolResult {
        let first = p.path.as_deref().map(|path| command::ReadItem { path, scope: p.scope.as_deref() });
        let rest = p.reads.iter().map(|r| command::ReadItem { path: &r.path, scope: r.scope.as_deref() });
        let items: Vec<command::ReadItem> = first.into_iter().chain(rest).collect();
        if items.is_empty() {
            return error(crate::Error::new("usage", "give path, or reads"));
        }
        match command::read(&self.langs, &items) {
            Ok(text) => {
                items.iter().for_each(|i| self.remember(i.path));
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
        On success, `replaced` gives the line ranges that now hold `after`; there is no need to read the file back.\n\n\
        To make several edits in one call, list them in `edits` with the same fields. All of them are matched \
        against the files as they are now, and if any fails, nothing is written.")]
    fn write(&self, Parameters(p): Parameters<WriteParams>) -> CallToolResult {
        let first = match (p.path, p.after) {
            (Some(path), Some(after)) => Some(EditParams {
                path,
                scope: p.scope,
                before: p.before,
                after,
                all_matches: p.all_matches,
                mode: p.mode,
            }),
            (None, None) => None,
            _ => return error(crate::Error::new("usage", "give path and after together")),
        };
        let edits: Vec<&EditParams> = first.iter().chain(&p.edits).collect();
        if edits.is_empty() {
            return error(crate::Error::new("usage", "give path and after, or edits"));
        }
        let reqs: Vec<WriteRequest> = edits.iter().map(|e| e.request()).collect();
        // Line numbers in a line range scope may be wrong in a file that changed since it was last read.
        let stale: Vec<std::path::PathBuf> = reqs
            .iter()
            .filter(|r| r.scope.and_then(crate::scope::parse_lines).is_some() && self.is_stale(r.path))
            .filter_map(|r| std::path::absolute(r.path).ok())
            .collect();
        // shortcut: waits for diagnostics on the runtime's only thread; fine while one agent writes at a time.
        // A check that panicked leaves the lock poisoned; a client cut off mid-request recovers on its next one.
        let mut check = |written: &Written| {
            self.lsp
                .as_ref()
                .map_or_else(Map::new, |s| s.lock().unwrap_or_else(std::sync::PoisonError::into_inner).check(written))
        };
        match command::write(&self.langs, &reqs, &mut check) {
            Ok(mut reports) => {
                for r in reports.iter_mut().filter(|r| r["ok"] == true) {
                    let file = r["file"].as_str().unwrap_or_default().to_string();
                    if std::path::absolute(&file).is_ok_and(|f| stale.contains(&f)) {
                        r["warnings"]["stale"] = json!("modified since last read");
                    }
                    self.remember(&file);
                }
                let ok = !reports.iter().any(|r| r["ok"] == false);
                let text = vec![ContentBlock::text(crate::report(&reports.into()))];
                if ok { CallToolResult::success(text) } else { CallToolResult::error(text) }
            }
            Err(e) => error(e),
        }
    }
}

fn error(e: crate::Error) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(crate::report(&e.to_json()))])
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
    async fn read_tool_reads_each_of_reads_after_path() {
        // Given: 関数が 2 つあるファイル
        let (_dir, path) = file("fn a() {}\nfn b() {}\n");
        let client = connect().await;
        // When: path と scope に加えて、reads にもう 1 つ渡して read を呼ぶ
        let args = json!({"path": path, "scope": "a", "reads": [{"path": path, "scope": "b"}]});
        let (text, _) = tool(&client, "read", args).await;
        // Then: path の分、reads の分の順に、それぞれパスの行の後に並ぶ
        assert_eq!(text, format!("{path}\n1\tfn a() {{}}\n{path}\n2\tfn b() {{}}\n"));
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
    async fn write_tool_writes_each_of_edits_after_path() {
        // Given: 呼び出しが 2 つある関数
        let (_dir, path) = file("fn a() {\n    x();\n    y();\n}\n");
        let client = connect().await;
        // When: path と before と after に加えて、edits にもう 1 つ渡して write を呼ぶ
        let args = json!({"path": path, "before": "x();", "after": "v();", "edits": [{"path": path, "before": "y();", "after": "w();"}]});
        let (text, _) = tool(&client, "write", args).await;
        // Then: どちらも書かれ、replaced は path の分、edits の分の順に並ぶ
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    v();\n    w();\n}\n");
        assert_eq!(text, format!("ok=true file={path} replaced=2-2,3-3"));
    }

    #[tokio::test]
    async fn write_tool_returns_same_report_as_cli() {
        // Given: 呼び出しのある関数
        let (_dir, path) = file("fn a() { x(); }\n");
        let client = connect().await;
        // When: write を呼ぶ
        let (text, _) = tool(&client, "write", json!({"path": path, "before": "x();", "after": "y();"})).await;
        // Then: CLI と同じ MISON の報告が返る
        assert_eq!(text, format!("ok=true file={path} replaced=1-1"));
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
        assert!(text.contains("warnings=(stale=\"modified since last read\")"), "{text}");
    }

    #[tokio::test]
    async fn stale_warning_does_not_depend_on_how_path_is_spelled() {
        // Given: read した後に外で書き換えられたファイル
        let (dir, path) = file("fn a() {}\n");
        let client = connect().await;
        tool(&client, "read", json!({"path": path})).await;
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        // When: 同じファイルを別の書き方のパスで、行範囲のスコープで書く
        let other = format!("{}/./a.rs", dir.path().to_string_lossy().replace('\\', "/"));
        let (text, _) = tool(&client, "write", json!({"path": other, "scope": "2", "after": "fn c() {}"})).await;
        // Then: stale の警告が付く
        assert!(text.contains("warnings=(stale=\"modified since last read\")"), "{text}");
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
        assert!(!text.contains("warnings"), "{text}");
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
        assert!(!text.contains("warnings"), "{text}");
    }
}
