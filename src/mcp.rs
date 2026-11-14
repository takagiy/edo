use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::command::{self, Mode, WriteRequest, Written};
use crate::language::Languages;
use crate::lsp::Servers;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
// Unknown arguments are rejected by `Numbered`, which takes every argument the others do not.
#[schemars(extend("additionalProperties" = false))]
pub struct ReadParams {
    /// File path or glob.
    pub path: Option<String>,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M), or several separated by commas.
    pub scope: Option<String>,
    /// More to read, given by number as `path_0`, `scope_0` and so on, after `path`.
    #[serde(flatten)]
    pub numbered: Numbered<ReadTarget>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadTarget {
    /// File path or glob.
    pub path: String,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M), or several separated by commas.
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
// Unknown arguments are rejected by `Numbered`, which takes every argument the others do not.
#[schemars(extend("additionalProperties" = false))]
pub struct WriteParams {
    /// File path or glob.
    pub path: Option<String>,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M), or several separated by commas.
    pub scope: Option<String>,
    /// Code to find. Omit to target the whole `scope`, or the whole file without `scope`.
    pub before: Option<String>,
    /// Code to write. Empty deletes the target.
    pub after: Option<String>,
    /// Edit every match instead of failing as ambiguous.
    #[serde(default)]
    pub all_matches: bool,
    /// Where to put `after`; it replaces each match by default.
    #[serde(default)]
    pub mode: Mode,
    /// More edits, given by number as `path_0`, `before_0` and so on, after the one given by `path`.
    #[serde(flatten)]
    pub numbered: Numbered<EditParams>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditParams {
    /// File path or glob.
    pub path: String,
    /// Identifier path (Foo.bar or Foo::bar) or line range (N or N-M), or several separated by commas.
    pub scope: Option<String>,
    /// Code to find. Omit to target the whole `scope`, or the whole file without `scope`.
    pub before: Option<String>,
    /// Code to write. Empty deletes the target.
    pub after: String,
    /// Edit every match instead of failing as ambiguous.
    #[serde(default)]
    pub all_matches: bool,
    /// Where to put `after`; it replaces each match by default.
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

/// Items given by number, as `path_0`, `scope_0` and so on, each with the fields of `T`. A client that writes
/// string arguments without JSON, such as Claude Code, can then pass code without escaping its line breaks and quotes.
#[derive(Debug)]
pub struct Numbered<T>(Vec<T>);

/// The properties of `T`'s schema: its fields by name.
fn fields<T: schemars::JsonSchema>() -> Map<String, Value> {
    let schema = schemars::schema_for!(T);
    schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default()
}

impl<'de, T: serde::de::DeserializeOwned + schemars::JsonSchema> Deserialize<'de> for Numbered<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let fields = fields::<T>();
        let mut items: BTreeMap<u64, Map<String, Value>> = BTreeMap::new();
        for (key, value) in Map::<String, Value>::deserialize(d)? {
            let numbered = key.rsplit_once('_').and_then(|(field, n)| Some((field, n.parse().ok()?)));
            let Some((field, n)) = numbered else {
                let names: Vec<String> = fields.keys().map(|f| format!("`{f}`")).collect();
                let expected =
                    format!("expected one of {}, or one of them with a number as in `path_0`", names.join(", "));
                return Err(D::Error::custom(format!("unknown field `{key}`, {expected}")));
            };
            // A client sends an argument missing from `properties` as a string, so `true` comes as "true".
            let boolean = fields.get(field).is_some_and(|s| s["type"] == "boolean");
            let value = match value {
                Value::String(s) if boolean && (s == "true" || s == "false") => Value::Bool(s == "true"),
                value => value,
            };
            items.entry(n).or_default().insert(field.to_string(), value);
        }
        let item = |(n, fields)| {
            serde_json::from_value(Value::Object(fields))
                .map_err(|e| D::Error::custom(format!("in the arguments ending in _{n}: {e}")))
        };
        Ok(Numbered(items.into_iter().map(item).collect::<Result<_, _>>()?))
    }
}

impl<T: schemars::JsonSchema> schemars::JsonSchema for Numbered<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("Numbered{}", T::schema_name()).into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let item = T::json_schema(generator);
        let fields = item.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let patterns: Map<String, Value> =
            fields.into_iter().map(|(name, s)| (format!("^{name}_[0-9]+$"), s)).collect();
        schemars::json_schema!({ "type": "object", "patternProperties": patterns, "additionalProperties": false })
    }
}

/// Whether a part of `scope` is a line range. A definition named with a comma is split here too, but its
/// parts are not line ranges unless the name is digits.
fn has_line_range(scope: &str) -> bool {
    crate::scope::parts(scope, &[]).into_iter().any(|p| crate::scope::parse_lines(p).is_some())
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

    #[tool(description = "Read a source file. Function and method bodies are hidden or shown as `...`; pass \
    `scope` to see one in full.\n\n\
        To read several files or scopes in one call, number them: `path_0`, `scope_0`, `path_1`, and so on.")]
    fn read(&self, Parameters(p): Parameters<ReadParams>) -> CallToolResult {
        let first = p.path.as_deref().map(|path| command::ReadItem { path, scope: p.scope.as_deref() });
        let rest = p.numbered.0.iter().map(|r| command::ReadItem { path: &r.path, scope: r.scope.as_deref() });
        let items: Vec<command::ReadItem> = first.into_iter().chain(rest).collect();
        if items.is_empty() {
            return error(crate::Error::new("usage", "give path, or numbered ones such as path_0"));
        }
        let command::Read { mut text, files, ok } = command::read(&self.langs, &items);
        files.iter().for_each(|f| self.remember(f));
        // Errors from checks earlier writes started, which come too late for those writes to wait for.
        let lock = |s: &Arc<Mutex<Servers>>| {
            s.lock().unwrap_or_else(std::sync::PoisonError::into_inner).read_errors(&self.langs, &files)
        };
        if let Some(errors) = self.lsp.as_ref().and_then(lock) {
            text.push_str(&format!("{}\n", crate::report(&errors)));
        }
        let text = vec![ContentBlock::text(text)];
        if ok { CallToolResult::success(text) } else { CallToolResult::error(text) }
    }

    #[tool(description = "Edit or create a source file.\n\n\
        Pass `scope` to replace a definition or a line range. Pass `before` to replace the code that matches it on \
        the AST, searched within `scope` if given.\n\n\
        A successful write needs no read-back.\n\n\
        To make several edits in one call, number them: `path_0`, `before_0`, `after_0`, `path_1`, and so on. All \
        are matched against the files before any is written.")]
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
        let edits: Vec<&EditParams> = first.iter().chain(&p.numbered.0).collect();
        if edits.is_empty() {
            return error(crate::Error::new(
                "usage",
                "give path and after, or numbered ones such as path_0 and after_0",
            ));
        }
        let reqs: Vec<WriteRequest> = edits.iter().map(|e| e.request()).collect();
        // Line numbers in a line range scope may be wrong in a file that changed since it was last read.
        let stale: Vec<std::path::PathBuf> = reqs
            .iter()
            .filter(|r| r.scope.is_some_and(has_line_range) && self.is_stale(r.path))
            .filter_map(|r| std::path::absolute(r.path).ok())
            .collect();
        // shortcut: waits for diagnostics on the runtime's only thread; fine while one agent writes at a time.
        // A check that panicked leaves the lock poisoned; a client cut off mid-request recovers on its next one.
        // The files of one call share one wait, so a glob over many files waits no longer than one file.
        let deadline = std::time::Instant::now() + crate::lsp::WAIT;
        let mut check = |written: &Written| {
            let servers = self.lsp.as_ref();
            servers.map_or_else(Map::new, |s| {
                s.lock().unwrap_or_else(std::sync::PoisonError::into_inner).check(written, deadline)
            })
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
    async fn read_tool_reads_numbered_items_after_path() {
        // Given: 関数が 3 つあるファイル
        let (_dir, path) = file("fn a() {}\nfn b() {}\nfn c() {}\n");
        let client = connect().await;
        // When: path と scope に加えて、_1 と _0 の番号付きの引数で読む
        let args = json!({"path": path, "scope": "a", "path_1": path, "scope_1": "c", "path_0": path, "scope_0": "b"});
        let (text, _) = tool(&client, "read", args).await;
        // Then: path の分、_0 の分、_1 の分の順に、それぞれパスの行の後に並ぶ
        assert_eq!(text, format!("{path}\n1\tfn a() {{}}\n{path}\n2\tfn b() {{}}\n{path}\n3\tfn c() {{}}\n"));
    }

    #[tokio::test]
    async fn read_tool_with_failing_item_returns_the_rest_as_error() {
        // Given: 関数が 1 つあるファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        // When: ない定義と、ファイル全体を、1 回で読む
        let args = json!({"path": path, "scope": "zzz", "path_0": path});
        let (text, is_error) = tool(&client, "read", args).await;
        // Then: ファイル全体の内容と not_found のエラーが返り、isError が立つ
        assert!(is_error, "{text}");
        assert!(text.contains("error=not_found") && text.contains("1\tfn a() {}"), "{text}");
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
    async fn unknown_argument_is_an_error_naming_it() {
        // Given: 関数が 1 つあるファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        // When: 綴りを誤った引数 (befor) を付けて write を呼ぶ
        let (error, is_error) = tool(&client, "write", json!({"path": path, "befor": "a", "after": "b"})).await;
        // Then: その名前を挙げたエラーになり、ファイルはそのまま
        assert!(is_error && error.contains("befor"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {}\n");
    }

    #[tokio::test]
    async fn unknown_field_of_numbered_argument_is_an_error_naming_it() {
        // Given: 関数が 1 つあるファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        // When: 知らないフィールド名に番号を付けた引数 (lines_0, range_0) で呼ぶ
        let args = json!({"path_0": path, "after_0": "x", "lines_0": "1"});
        let (write, write_failed) = tool(&client, "write", args).await;
        let (read, read_failed) = tool(&client, "read", json!({"path_0": path, "range_0": "1"})).await;
        // Then: どちらもその名前を挙げたエラーになり、ファイルはそのまま
        assert!(write_failed && write.contains("lines"), "{write}");
        assert!(read_failed && read.contains("range"), "{read}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {}\n");
    }

    #[tokio::test]
    async fn edits_and_reads_arrays_are_unknown_arguments() {
        // Given: 関数が 1 つあるファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        // When: 配列の edits と reads で呼ぶ
        let args = json!({"path": path, "after": "x", "edits": [{"path": path, "after": "y"}]});
        let (write, write_failed) = tool(&client, "write", args).await;
        let (read, read_failed) = tool(&client, "read", json!({"path": path, "reads": [{"path": path}]})).await;
        // Then: どちらもその名前を挙げたエラーになり、ファイルはそのまま
        assert!(write_failed && write.contains("edits"), "{write}");
        assert!(read_failed && read.contains("reads"), "{read}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {}\n");
    }

    #[tokio::test]
    async fn numbered_arguments_are_edits_after_path_in_number_order() {
        // Given: 呼び出しが 3 つある関数
        let (_dir, path) = file("fn a() {\n    x();\n    y();\n    z();\n}\n");
        let client = connect().await;
        // When: path の分に加えて、_1 と _0 の番号付きの引数で編集を渡す
        let args = json!({
            "path": path, "before": "x();", "after": "u();",
            "path_1": path, "before_1": "z();", "after_1": "w();",
            "path_0": path, "before_0": "y();", "after_0": "v();",
        });
        let (text, is_error) = tool(&client, "write", args).await;
        // Then: すべて書かれ、replaced は path の分、_0 の分、_1 の分の順に並ぶ
        assert!(!is_error, "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    u();\n    v();\n    w();\n}\n");
        let replaced: Vec<&str> = text.split("replaced=").nth(1).unwrap_or_default().split(',').collect();
        assert_eq!(replaced, ["2-2", "3-3", "4-4"], "{text}");
    }

    #[tokio::test]
    async fn numbered_edit_without_after_is_an_error_naming_its_number() {
        // Given: 関数が 1 つあるファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        // When: after_0 のない _0 の編集を渡す
        let (text, is_error) = tool(&client, "write", json!({"path_0": path, "before_0": "a"})).await;
        // Then: その番号を挙げたエラーになり、ファイルはそのまま
        assert!(is_error && text.contains("_0") && text.contains("after"), "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {}\n");
    }

    #[tokio::test]
    async fn numbered_all_matches_takes_true_as_a_string() {
        // Given: 同じ呼び出しが 2 つある関数
        let (_dir, path) = file("fn a() {\n    x();\n    x();\n}\n");
        let client = connect().await;
        // When: 型を変換しないクライアントのように、all_matches_0 を文字列の "true" で渡す
        let args = json!({"path_0": path, "before_0": "x();", "after_0": "y();", "all_matches_0": "true"});
        let (text, is_error) = tool(&client, "write", args).await;
        // Then: 真偽値の true と同じく、両方が書き換わる
        assert!(!is_error, "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn a() {\n    y();\n    y();\n}\n");
    }

    #[tokio::test]
    async fn schemas_show_numbered_names_of_each_field_as_patterns() {
        // Given: MCP サーバーにつないだクライアント
        let client = connect().await;
        // When: read と write のスキーマを見る
        let tools = client.list_all_tools().await.unwrap();
        let fields =
            [("read", &["path", "scope"][..]), ("write", &["path", "scope", "before", "after", "all_matches", "mode"])];
        for (name, fields) in fields {
            let tool = tools.iter().find(|t| t.name == name).unwrap();
            let schema = Value::Object(tool.input_schema.as_ref().clone());
            let patterns = schema["patternProperties"].as_object().cloned().unwrap_or_default();
            // Then: 各フィールドに、その名前の番号付きの形を示すパターンがあり、ほかの名前は許さない
            for field in fields {
                assert!(patterns.keys().any(|p| p.contains(field)), "{name} {field}: {patterns:?}");
            }
            assert_eq!(schema["additionalProperties"], json!(false), "{name}: {schema}");
        }
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
    async fn comma_scope_with_line_range_to_file_changed_since_read_warns_stale() {
        // Given: read した後に外で書き換えられたファイル
        let (_dir, path) = file("fn a() {}\n");
        let client = connect().await;
        tool(&client, "read", json!({"path": path})).await;
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        // When: 名前と行範囲を , で区切ったスコープに書く
        let args = json!({"path": path, "scope": "a, 2", "before": "fn", "after": "pub fn", "all_matches": true});
        let (text, _) = tool(&client, "write", args).await;
        // Then: stale の警告が付く
        assert!(text.contains("warnings=(stale=\"modified since last read\")"), "{text}");
    }

    #[tokio::test]
    async fn line_range_write_to_file_read_by_glob_and_changed_warns_stale() {
        // Given: glob で read した後に外で書き換えられたファイル
        let (dir, path) = file("fn a() {}\n");
        let client = connect().await;
        let glob = format!("{}/*.rs", dir.path().to_string_lossy().replace('\\', "/"));
        tool(&client, "read", json!({"path": glob})).await;
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
