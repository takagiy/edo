use std::path::Path;

use crate::edit::{self, Place};
use crate::language::{Language, Languages};
use crate::matcher;
use crate::outline::{Definition, outline};
use crate::scope::Hit;
use crate::{Error, Skipped};
use crate::{scope, view};
use serde_json::{Value, json};

/// A source file parsed with its language.
struct Source<'a> {
    lang: &'a Language,
    text: String,
    defs: Vec<Definition>,
}

fn language<'a>(langs: &'a Languages, path: &Path) -> Result<&'a Language, Error> {
    langs.for_path(path).ok_or_else(|| Error::new("unsupported_language", path.display().to_string()))
}

impl<'a> Source<'a> {
    fn new(lang: &'a Language, text: String) -> Self {
        let tree = lang.parser().parse(&text, None).expect("parse");
        let defs = outline(lang, &tree, &text);
        Source { lang, text, defs }
    }
}

fn open<'a>(langs: &'a Languages, path: &Path) -> Result<Source<'a>, Error> {
    Ok(Source::new(language(langs, path)?, read_text(path)?))
}

/// Reads a UTF-8 text file; rewriting anything else could corrupt it.
fn read_text(path: &Path) -> Result<String, Error> {
    let shown = path.display();
    let bytes = std::fs::read(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Error::new("not_found", format!("{shown}: no such file")),
        _ => Error::new("io", format!("{shown}: {e}")),
    })?;
    if bytes.contains(&0) {
        return Err(Error::new("binary", format!("{shown}: binary file")));
    }
    String::from_utf8(bytes).map_err(|_| Error::new("encoding", format!("{shown}: not UTF-8")))
}

fn is_glob(path: &str) -> bool {
    path.contains(['*', '?', '['])
}

/// Files matching a glob, skipping what `.gitignore` excludes, as `/`-separated paths in order.
fn expand(pattern: &str) -> Result<Vec<String>, Error> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::new("usage", e.to_string()))?
        .compile_matcher();
    // Walk from the longest directory prefix without glob characters.
    let base_len = pattern[..pattern.find(['*', '?', '[']).unwrap()].rfind('/').map_or(0, |i| i + 1);
    let base = if base_len == 0 { "." } else { &pattern[..base_len] };
    let mut out: Vec<String> = ignore::WalkBuilder::new(base)
        .require_git(false)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .map(|e| e.path().to_string_lossy().replace('\\', "/"))
        .map(|p| if base_len == 0 { p.trim_start_matches("./").to_string() } else { p })
        .filter(|p| glob.is_match(p))
        .collect();
    out.sort();
    Ok(out)
}

/// Splits glob matches into files with a language and counts of the rest by extension;
/// a broad glob such as `**/*` also picks up files edo has no language for.
fn supported(langs: &Languages, files: Vec<String>) -> (Vec<String>, Skipped) {
    let mut skipped = Skipped::new();
    let files = files
        .into_iter()
        .filter(|f| {
            let ok = langs.for_path(Path::new(f)).is_some();
            if !ok {
                let ext = Path::new(f).extension().map_or(String::new(), |e| e.to_string_lossy().into_owned());
                *skipped.entry(ext).or_default() += 1;
            }
            ok
        })
        .collect();
    (files, skipped)
}

/// Reads a file (or glob) as an outline, or the definitions/lines selected by `scope`.
pub fn read(langs: &Languages, path: &str, scope: Option<&str>) -> Result<String, Error> {
    if !is_glob(path) {
        return read_one(langs, path, scope);
    }
    let files = expand(path)?;
    if files.is_empty() {
        return Err(Error::new("not_found", "no file matched"));
    }
    let (files, skipped) = supported(langs, files);
    if files.is_empty() {
        let mut err = Error::new("unsupported_language", "no matched file has a language package");
        err.skipped = skipped;
        return Err(err);
    }
    let mut out = String::new();
    for file in files {
        match read_one(langs, &file, scope) {
            Ok(text) => out.push_str(&format!("{file}\n{text}")),
            // Like write, a file that the scope does not resolve in is left out.
            Err(e) if e.code == "not_found" => {}
            Err(e) => return Err(e),
        }
    }
    if out.is_empty() {
        let mut err = Error::new("not_found", format!("no matched file defines {}", scope.unwrap_or_default()));
        err.skipped = skipped;
        return Err(err);
    }
    if !skipped.is_empty() {
        out.push_str(&format!("{}\n", skipped_line(&skipped)));
    }
    Ok(out)
}

/// `{"skipped":{...}}`, the note a glob ends with when it skipped files.
fn skipped_line(skipped: &Skipped) -> Value {
    json!({"skipped": crate::skipped_json(skipped)})
}

/// What `write` does with each match (or the whole target without `before`), for both the CLI and MCP.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Replace each match with `after`.
    #[default]
    ReplaceMatches,
    /// Put `after` on new lines before each match.
    InsertBeforeMatches,
    /// Put `after` on new lines after each match.
    InsertAfterMatches,
}

impl From<Mode> for Place {
    fn from(mode: Mode) -> Place {
        match mode {
            Mode::ReplaceMatches => Place::Replace,
            Mode::InsertBeforeMatches => Place::InsertBefore,
            Mode::InsertAfterMatches => Place::InsertAfter,
        }
    }
}

#[derive(Debug, Default)]
pub struct WriteRequest<'a> {
    pub path: &'a str,
    pub scope: Option<&'a str>,
    pub before: Option<&'a str>,
    pub after: &'a str,
    pub all_matches: bool,
    pub place: Place,
}

/// Writes `after` into each matched file and reports per file.
pub fn write(langs: &Languages, req: &WriteRequest) -> Result<Vec<Value>, Error> {
    if !is_glob(req.path) {
        return Ok(vec![write_one(langs, req.path, req)?]);
    }
    let (files, skipped) = supported(langs, expand(req.path)?);
    let mut reports: Vec<Value> = files
        .iter()
        .map(|f| {
            write_one(langs, f, req)
                .unwrap_or_else(|e| json!({"ok": false, "file": f, "error": e.code, "message": e.message}))
        })
        .filter(|r| r["error"] != "not_found")
        .collect();
    if reports.is_empty() {
        let mut err = Error::new("not_found", "no file matched");
        err.skipped = skipped;
        return Ok(vec![err.to_json()]);
    }
    if !skipped.is_empty() {
        reports.push(skipped_line(&skipped));
    }
    Ok(reports)
}

fn write_one(langs: &Languages, path: &str, req: &WriteRequest) -> Result<Value, Error> {
    let file = Path::new(path);
    // Writing a whole file that does not exist yet is writing over an empty one.
    let creating = !file.exists() && req.scope.is_none() && req.before.is_none();
    let lang = language(langs, file)?;
    let src = Source::new(lang, if creating { String::new() } else { read_text(file)? });
    let tree = src.lang.parser().parse(&src.text, None).expect("parse");
    let leaves = matcher::leaves(src.lang, &tree, &src.text);
    let regions = match req.scope {
        Some(s) => scope::resolve(s, &src.defs, &src.text)?,
        None => vec![Hit { range: 0..src.text.len(), def: None }],
    };
    let mut closest: Option<matcher::Closest> = None;
    let targets: Vec<Hit> = match req.before {
        Some(before) => regions
            .iter()
            .flat_map(|r| {
                let found = matcher::find(&src.text, &leaves, r.range.clone(), before);
                closest = closest.take().or(found.closest);
                found.matches
            })
            .map(|range| Hit { range, def: None })
            .collect(),
        None => regions,
    };
    if targets.is_empty() {
        let mut report = json!({"ok": false, "file": path, "error": "not_found"});
        if let Some(c) = closest {
            let t = &src.text;
            let mismatch = json!({"line": line_of(t, c.mismatch), "expected": c.expected, "found": c.found});
            report["closest"] =
                json!({"range": [line_of(t, c.range.start), line_of(t, c.range.end)], "mismatch": mismatch});
        }
        return Ok(report);
    }
    if targets.len() > 1 && !req.all_matches {
        let matches: Vec<Value> = targets.iter().map(|t| candidate(&src, t)).collect();
        return Ok(json!({"ok": false, "file": path, "error": "ambiguous", "matches": matches}));
    }
    // Apply from the last target so earlier ranges stay valid, then shift each one's lines by the edits before it.
    let mut text = src.text.clone();
    let mut done: Vec<(edit::Outcome, isize)> = vec![];
    for t in targets.iter().rev() {
        let out = edit::edit(src.lang, &text, t.range.clone(), req.after, req.place);
        let grown = out.text.matches('\n').count() as isize - text.matches('\n').count() as isize;
        text = out.text.clone();
        done.push((out, grown));
    }
    done.reverse();
    let mut shift = 0;
    let mut replaced = vec![];
    let mut errors = vec![];
    for (out, grown) in done {
        let at = |line: usize| (line as isize + shift) as usize;
        replaced.push(json!([at(out.lines.0), at(out.lines.1)]));
        errors.extend(out.new_syntax_errors.into_iter().map(|e| json!({"line": at(e.line), "text": e.text})));
        shift += grown;
    }
    let io = |e: std::io::Error| Error::new("io", format!("{path}: {e}"));
    std::fs::create_dir_all(file.parent().unwrap_or(Path::new("."))).map_err(io)?;
    std::fs::write(path, &text).map_err(io)?;
    let mut report = json!({"ok": true, "file": path, "replaced": replaced});
    if !errors.is_empty() {
        report["warnings"] = json!({"new_syntax_errors": errors});
    }
    Ok(report)
}

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count() + 1
}

/// Describes one of several targets so the agent can tell them apart and pick one by its range.
fn candidate(src: &Source, target: &Hit) -> Value {
    let text = &src.text;
    let range = &target.range;
    let (parents, signature) = match target.def {
        Some(i) => {
            let mut chain = vec![];
            let mut p = src.defs[i].parent;
            while let Some(j) = p {
                chain.insert(0, src.defs[j].header(text));
                p = src.defs[j].parent;
            }
            (chain, src.defs[i].header(text))
        }
        None => {
            let enclosing = src.defs.iter().filter(|d| d.node.start <= range.start && range.end <= d.node.end);
            (
                enclosing.map(|d| d.header(text)).collect(),
                text[range.clone()].lines().next().unwrap_or("").trim().to_string(),
            )
        }
    };
    let last = line_of(text, range.end.max(range.start + 1) - 1);
    json!({"range": [line_of(text, range.start), last], "parents": parents, "signature": signature})
}

fn read_one(langs: &Languages, path: &str, scope: Option<&str>) -> Result<String, Error> {
    let src = open(langs, Path::new(path))?;
    let _ = src.lang;
    match scope {
        None => Ok(view::render(&src.text, &src.defs, 0..src.text.len(), true)),
        Some(scope) => Ok(view::render_hits(&src.text, &src.defs, &scope::resolve(scope, &src.defs, &src.text)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn at(dir: &tempfile::TempDir, name: &str) -> String {
        dir.path().join(name).to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn reading_file_returns_outline() {
        // Given: 関数のある Rust のファイル
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n}\n")]);
        // When: ファイルを読む
        let out = read(&Languages::builtin(), &at(&dir, "a.rs"), None).unwrap();
        // Then: ボディを省略したアウトラインが返る
        assert_eq!(out, "1\tfn a() {\n3\t}\n");
    }

    #[test]
    fn reading_glob_prefixes_each_file_with_its_path() {
        // Given: 2 つの Rust のファイル
        let dir = dir_with(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
        // When: glob で読む
        let out = read(&Languages::builtin(), &at(&dir, "src/*.rs"), None).unwrap();
        // Then: ファイルごとにパスの行が前に付く
        assert_eq!(
            out,
            format!("{}\n1\tfn a() {{}}\n{}\n1\tfn b() {{}}\n", at(&dir, "src/a.rs"), at(&dir, "src/b.rs"))
        );
    }

    fn read_error(files: &[(&str, &[u8])], name: &str) -> &'static str {
        let dir = tempfile::tempdir().unwrap();
        for (n, bytes) in files {
            std::fs::write(dir.path().join(n), bytes).unwrap();
        }
        read(&Languages::builtin(), &at(&dir, name), None).unwrap_err().code
    }

    #[test]
    fn reading_missing_file_is_not_found() {
        // Given: ファイルのないディレクトリ / When: 存在しないファイルを読む / Then: not_found になる
        assert_eq!(read_error(&[], "a.rs"), "not_found");
    }

    #[test]
    fn file_with_nul_is_binary_error() {
        // Given: NUL を含むファイル / When: 読む / Then: binary エラーになる
        assert_eq!(read_error(&[("a.rs", b"fn a() {}\0")], "a.rs"), "binary");
    }

    #[test]
    fn non_utf8_file_is_encoding_error() {
        // Given: UTF-8 でないファイル / When: 読む / Then: encoding エラーになる
        assert_eq!(read_error(&[("a.rs", b"// \xff\xfe\n")], "a.rs"), "encoding");
    }

    #[test]
    fn file_without_language_package_is_unsupported_language() {
        // Given: 言語パッケージのない拡張子のファイル / When: 読む / Then: unsupported_language になる
        assert_eq!(read_error(&[("a.yaml", b"a: 1\n")], "a.yaml"), "unsupported_language");
    }

    fn content(dir: &tempfile::TempDir, name: &str) -> String {
        std::fs::read_to_string(dir.path().join(name)).unwrap()
    }

    #[test]
    fn before_matching_once_is_written_and_reported_ok() {
        // Given: 呼び出しが 1 つある関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n}\n")]);
        let path = at(&dir, "a.rs");
        // When: その呼び出しを before にして書く
        let req = WriteRequest { path: &path, before: Some("x();"), after: "y();", ..Default::default() };
        let reports = write(&Languages::builtin(), &req).unwrap();
        // Then: 書き込まれ、ok が報告される
        assert_eq!(content(&dir, "a.rs"), "fn a() {\n    y();\n}\n");
        assert_eq!(reports, [serde_json::json!({"ok": true, "file": path, "replaced": [[2, 2]]})]);
    }

    fn write_in(dir: &tempfile::TempDir, req: WriteRequest) -> Vec<Value> {
        let path = req.path.to_string();
        let req = WriteRequest { path: &path, ..req };
        let _ = dir;
        write(&Languages::builtin(), &req).unwrap()
    }

    #[test]
    fn before_matching_several_places_reports_ambiguous_and_writes_nothing() {
        // Given: 同じ呼び出しが 2 つある関数
        let src = "fn a() {\n    x();\n    x();\n}\n";
        let dir = dir_with(&[("a.rs", src)]);
        // When: その呼び出しを before にして書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "a.rs"), before: Some("x();"), after: "y();", ..Default::default() },
        );
        // Then: ambiguous が報告され、ファイルは変わらない
        assert_eq!(reports[0]["error"], "ambiguous");
        assert_eq!(content(&dir, "a.rs"), src);
    }

    #[test]
    fn ambiguous_candidates_have_range_parents_and_header() {
        // Given: Foo の中と Bar の中に同名のメソッド bar がある Kotlin のファイル
        let src = "data object Foo {\n  companion object {\n    fun bar(x: Int) {\n      x()\n    }\n  }\n}\nclass Bar {\n  fun bar(s: String) {\n  }\n}\n";
        let dir = dir_with(&[("a.kt", src)]);
        // When: スコープ bar に before なしで書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "a.kt"), scope: Some("bar"), after: "fun bar() {}", ..Default::default() },
        );
        // Then: 候補ごとに範囲と親と見出しが報告される
        assert_eq!(
            reports[0]["matches"],
            serde_json::json!([
                {"range": [3, 5], "parents": ["data object Foo", "companion object"], "signature": "fun bar(x: Int)"},
                {"range": [9, 10], "parents": ["class Bar"], "signature": "fun bar(s: String)"},
            ])
        );
    }

    #[test]
    fn before_matching_nowhere_reports_not_found_with_closest() {
        // Given: 引数が 1 の呼び出しがある関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    hoge(1);\n}\n")]);
        // When: 引数の違う before で書く
        let reports = write_in(
            &dir,
            WriteRequest {
                path: &at(&dir, "a.rs"),
                before: Some("fn a() {\n hoge(42);"),
                after: "x",
                ..Default::default()
            },
        );
        // Then: not_found と最も近い候補が報告される
        assert_eq!(reports[0]["error"], "not_found");
        assert_eq!(
            reports[0]["closest"],
            serde_json::json!({"range": [1, 2], "mismatch": {"line": 2, "expected": "hoge(42);", "found": "hoge(1);"}})
        );
    }

    #[test]
    fn all_matches_replaces_every_match() {
        // Given: 同じ呼び出しが 2 つある関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n    x();\n}\n")]);
        // When: all_matches を付けて、2 行の after で書く
        let req = WriteRequest {
            path: &at(&dir, "a.rs"),
            before: Some("x();"),
            after: "y();\nz();",
            all_matches: true,
            ..Default::default()
        };
        let reports = write_in(&dir, req);
        // Then: どちらも置き換わり、書き換えた後の行がそれぞれ報告される
        assert_eq!(content(&dir, "a.rs"), "fn a() {\n    y();\n    z();\n    y();\n    z();\n}\n");
        assert_eq!(reports[0]["replaced"], serde_json::json!([[2, 3], [4, 5]]));
    }

    #[test]
    fn syntax_error_introduced_by_write_is_reported_as_warning() {
        // Given: 正しい関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n}\n")]);
        // When: 閉じ括弧の足りない after で書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "a.rs"), before: Some("x();"), after: "y(;", ..Default::default() },
        );
        // Then: 書き込まれ、構文エラーが警告として報告される
        assert_eq!(reports[0]["ok"], true);
        assert_eq!(reports[0]["warnings"], serde_json::json!({"new_syntax_errors": [{"line": 2, "text": "y(;"}]}));
    }

    #[test]
    fn without_before_whole_scope_is_replaced() {
        // Given: 関数が 2 つあるファイル
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n}\nfn b() {}\n")]);
        // When: スコープ a に before なしで書く
        write_in(
            &dir,
            WriteRequest { path: &at(&dir, "a.rs"), scope: Some("a"), after: "fn a() {}", ..Default::default() },
        );
        // Then: 関数 a 全体が置き換わる
        assert_eq!(content(&dir, "a.rs"), "fn a() {}\nfn b() {}\n");
    }

    #[test]
    fn without_scope_and_before_whole_file_is_replaced() {
        // Given: 関数のあるファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: スコープも before もなしで書く
        write_in(&dir, WriteRequest { path: &at(&dir, "a.rs"), after: "fn b() {}\n", ..Default::default() });
        // Then: ファイル全体が置き換わる
        assert_eq!(content(&dir, "a.rs"), "fn b() {}\n");
    }

    #[test]
    fn writing_missing_file_creates_it_with_parent_directories() {
        // Given: 空のディレクトリ
        let dir = dir_with(&[]);
        // When: 存在しないディレクトリの中のファイルに書く
        write_in(&dir, WriteRequest { path: &at(&dir, "src/new/a.rs"), after: "fn a() {}\n", ..Default::default() });
        // Then: 親ディレクトリごとファイルが作られる
        assert_eq!(content(&dir, "src/new/a.rs"), "fn a() {}\n");
    }

    #[test]
    fn glob_write_leaves_files_without_match_out_of_report() {
        // Given: 呼び出しのあるファイルとないファイル
        let dir = dir_with(&[("a.rs", "fn a() { x(); }\n"), ("b.rs", "fn b() {}\n")]);
        // When: glob で書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "*.rs"), before: Some("x();"), after: "y();", ..Default::default() },
        );
        // Then: 一致したファイルだけが報告される
        let files: Vec<_> = reports.iter().map(|r| r["file"].as_str().unwrap().to_string()).collect();
        assert_eq!(files, [at(&dir, "a.rs")]);
    }

    #[test]
    fn glob_write_matching_no_file_is_not_found() {
        // Given: 呼び出しのないファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: glob で書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "*.rs"), before: Some("x();"), after: "y();", ..Default::default() },
        );
        // Then: not_found になる
        assert_eq!(reports, [serde_json::json!({"ok": false, "error": "not_found", "message": "no file matched"})]);
    }

    #[test]
    fn glob_skips_files_listed_in_gitignore() {
        // Given: .gitignore で target/ を除外したディレクトリ
        let dir = dir_with(&[(".gitignore", "target/\n"), ("a.rs", "fn a() {}\n"), ("target/b.rs", "fn b() {}\n")]);
        // When: ** の glob で読む
        let out = read(&Languages::builtin(), &at(&dir, "**/*.rs"), None).unwrap();
        // Then: 除外されたファイルは含まれない
        assert!(!out.contains("b.rs"), "{out}");
        assert!(out.contains("a.rs"));
    }

    #[test]
    fn glob_read_skips_files_without_language_package() {
        // Given: Rust のファイルと JSON のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("b.json", "{}")]);
        // When: 両方に一致する glob で読む
        let out = read(&Languages::builtin(), &at(&dir, "*"), None).unwrap();
        // Then: 言語パッケージのない JSON は飛ばされ、Rust のファイルは読める
        assert!(out.starts_with(&format!("{}\n1\tfn a() {{}}\n", at(&dir, "a.rs"))), "{out}");
        assert!(!out.contains("b.json"), "{out}");
    }

    #[test]
    fn glob_read_matching_no_file_is_not_found() {
        // Given: Rust のファイルだけがあるディレクトリ
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: どのファイルにも一致しない glob で読む
        let err = read(&Languages::builtin(), &at(&dir, "*.kt"), None).unwrap_err();
        // Then: not_found になる
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn reading_scope_without_definition_is_not_found_naming_scope() {
        // Given: 関数 a だけがある Rust のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: スコープ b で読む
        let err = read(&Languages::builtin(), &at(&dir, "a.rs"), Some("b")).unwrap_err();
        // Then: スコープの名前を挙げた not_found になる
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("b"), "{}", err.message);
    }

    #[test]
    fn glob_read_leaves_files_without_scope_definition_out() {
        // Given: 関数 a を持つファイルと、関数 b を持つファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        // When: 両方に一致する glob を、スコープ a で読む
        let out = read(&Languages::builtin(), &at(&dir, "*.rs"), Some("a")).unwrap();
        // Then: a を持つファイルだけが出る
        assert_eq!(out, format!("{}\n1\tfn a() {{}}\n", at(&dir, "a.rs")));
    }

    #[test]
    fn glob_read_with_scope_no_file_defines_is_not_found() {
        // Given: 関数 a だけを持つファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: glob を、どのファイルにもないスコープ b で読む
        let err = read(&Languages::builtin(), &at(&dir, "*.rs"), Some("b")).unwrap_err();
        // Then: not_found になる
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn glob_read_of_only_unsupported_files_is_unsupported_language_with_counts() {
        // Given: 言語パッケージのないファイルだけがあるディレクトリ
        let dir = dir_with(&[("a.json", "{}"), ("b.json", "{}"), ("c.yaml", "a: 1\n")]);
        // When: glob で読む
        let err = read(&Languages::builtin(), &at(&dir, "*"), None).unwrap_err();
        // Then: unsupported_language になり、飛ばした拡張子ごとの件数を持つ
        assert_eq!(err.code, "unsupported_language");
        assert_eq!(
            err.to_json()["skipped"],
            serde_json::json!({"reason": "unsupported_language", "extensions": {"json": 2, "yaml": 1}})
        );
    }

    #[test]
    fn glob_read_skipping_some_files_ends_with_counts_as_json_line() {
        // Given: Rust のファイルと JSON のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("b.json", "{}")]);
        // When: glob で読む
        let out = read(&Languages::builtin(), &at(&dir, "*"), None).unwrap();
        // Then: 出力の最後の行に、飛ばした拡張子ごとの件数が JSON で付く
        let last = serde_json::json!({"skipped": {"reason": "unsupported_language", "extensions": {"json": 1}}});
        assert_eq!(out, format!("{}\n1\tfn a() {{}}\n{last}\n", at(&dir, "a.rs")));
    }

    #[test]
    fn glob_write_leaves_unsupported_files_out_of_report() {
        // Given: 呼び出しのある Rust のファイルと JSON のファイル
        let dir = dir_with(&[("a.rs", "fn a() { x(); }\n"), ("b.json", "{}")]);
        // When: 両方に一致する glob で書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "*"), before: Some("x();"), after: "y();", ..Default::default() },
        );
        // Then: JSON のファイルは報告に含まれない
        let files: Vec<_> = reports.iter().filter_map(|r| r["file"].as_str()).map(String::from).collect();
        assert_eq!(files, [at(&dir, "a.rs")]);
    }

    #[test]
    fn glob_write_matching_no_file_reports_skipped_counts_with_not_found() {
        // Given: 呼び出しのない Rust のファイルと JSON のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("b.json", "{}")]);
        // When: 両方に一致する glob で書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "*"), before: Some("x();"), after: "y();", ..Default::default() },
        );
        // Then: not_found に飛ばした拡張子ごとの件数が付く
        assert_eq!(reports[0]["error"], "not_found");
        assert_eq!(
            reports[0]["skipped"],
            serde_json::json!({"reason": "unsupported_language", "extensions": {"json": 1}})
        );
    }

    #[test]
    fn glob_write_skipping_some_files_ends_report_with_counts() {
        // Given: 呼び出しのある Rust のファイルと JSON のファイル
        let dir = dir_with(&[("a.rs", "fn a() { x(); }\n"), ("b.json", "{}")]);
        // When: 両方に一致する glob で書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "*"), before: Some("x();"), after: "y();", ..Default::default() },
        );
        // Then: 報告の最後に、飛ばした拡張子ごとの件数が付く
        let last = serde_json::json!({"skipped": {"reason": "unsupported_language", "extensions": {"json": 1}}});
        assert_eq!(reports.last(), Some(&last));
    }
}
