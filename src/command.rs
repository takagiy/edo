use std::ops::Range;
use std::path::Path;

use crate::diagnostic::{Hunk, new_errors};
use crate::edit::{self, Place};
use crate::language::{Language, Languages};
use crate::matcher;
use crate::outline::{Definition, outline};
use crate::scope::Hit;
use crate::{Error, Skipped};
use crate::{scope, view};
use serde_json::{Map, Value, json};

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

/// `*` and `?` make a glob. `[` and `]` do not: paths such as `app/[id]/page.tsx` name a file.
fn is_glob(path: &str) -> bool {
    path.contains(['*', '?'])
}

/// Files matching a glob, skipping what `.gitignore` excludes, as `/`-separated paths in order.
fn expand(pattern: &str) -> Result<Vec<String>, Error> {
    // Windows paths cannot hold `\`, so there it is a separator; elsewhere it escapes as globset reads it.
    let pattern = &if cfg!(windows) { pattern.replace('\\', "/") } else { pattern.to_string() };
    let glob = globset::GlobBuilder::new(&pattern.replace('[', "\\[").replace(']', "\\]"))
        .literal_separator(true)
        .backslash_escape(true)
        .build()
        .map_err(|e| Error::new("usage", e.to_string()))?
        .compile_matcher();
    // Walk from the longest directory prefix without glob characters.
    let base_len = pattern[..pattern.find(['*', '?']).unwrap()].rfind('/').map_or(0, |i| i + 1);
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

/// One thing to read: a file or glob, and the scope in it if any.
#[derive(Debug, Clone, Copy)]
pub struct ReadItem<'a> {
    pub path: &'a str,
    pub scope: Option<&'a str>,
}

/// Reads each item in turn, with a panic reported as an `internal` error. With several items, each one's
/// text follows its path, as a glob's files do, and the first failure fails the whole read.
/// Returns the text and the files read, in order.
pub fn read(langs: &Languages, items: &[ReadItem]) -> Result<(String, Vec<String>), Error> {
    let several = items.len() > 1;
    guarded(|| {
        let (mut out, mut files) = (String::new(), vec![]);
        for (i, item) in items.iter().enumerate() {
            let (text, read) =
                read_files(langs, item.path, item.scope).map_err(|e| Error { item: several.then_some(i + 1), ..e })?;
            if several && !is_glob(item.path) {
                out.push_str(&format!("{}\n", item.path));
            }
            out.push_str(&text);
            files.extend(read);
        }
        Ok((out, files))
    })
}

/// Writes every request, with a panic reported as an `internal` error. All requests are matched against the
/// files as they were before the call, and if any is not found, is ambiguous or overlaps another, nothing is written and the failures are reported.
pub fn write(langs: &Languages, reqs: &[WriteRequest], check: Check) -> Result<Vec<Value>, Error> {
    guarded(|| write_requests(langs, reqs, check))
}

/// Runs `f`, turning a panic into an `internal` error so that a bug answers the agent instead of leaving
/// the MCP call waiting forever.
fn guarded<T>(f: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let message = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref().cloned());
        Err(Error::new("internal", message.unwrap_or_else(|| "edo panicked".into())))
    })
}

/// Reads a file (or glob) as an outline, or the definitions/lines selected by `scope`, with the files read.
fn read_files(langs: &Languages, path: &str, scope: Option<&str>) -> Result<(String, Vec<String>), Error> {
    if !is_glob(path) {
        return Ok((read_one(langs, path, scope)?, vec![path.to_string()]));
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
    let (mut out, mut read) = (String::new(), vec![]);
    for file in files {
        match read_one(langs, &file, scope) {
            Ok(text) => {
                out.push_str(&format!("{file}\n{text}"));
                read.push(file);
            }
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
        out.push_str(&format!("{}\n", crate::report(&skipped_line(&skipped))));
    }
    Ok((out, read))
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

/// A file as a write changed it, for a check that adds warnings to its report.
pub struct Written<'a> {
    pub lang: &'a Language,
    pub path: &'a Path,
    pub before: &'a str,
    pub after: &'a str,
    pub hunks: &'a [Hunk],
}

/// Adds warnings to a written file's report, such as the errors a language server found.
pub type Check<'a> = &'a mut dyn FnMut(&Written) -> Map<String, Value>;

/// Finds every request's targets, then writes each file once; reports per file, or only the failures.
fn write_requests(langs: &Languages, reqs: &[WriteRequest], check: Check) -> Result<Vec<Value>, Error> {
    let several = reqs.len() > 1;
    let mut plans: Vec<Plan> = vec![];
    let mut failures: Vec<Value> = vec![];
    let mut skipped = Skipped::new();
    for (item, req) in reqs.iter().enumerate() {
        let glob = is_glob(req.path);
        let (files, missed) =
            if glob { supported(langs, expand(req.path)?) } else { (vec![req.path.into()], Skipped::new()) };
        let mut found = false;
        let mut reports = vec![];
        for file in files {
            let at = match plans.iter().position(|p| p.path == file) {
                Some(at) => Ok(at),
                None => Plan::open(langs, file, req).map(|p| {
                    plans.push(p);
                    plans.len() - 1
                }),
            };
            match at.and_then(|at| plans[at].find(req, item)) {
                Ok(()) => found = true,
                // A glob writes the files that `before` and `scope` are found in.
                Err(report) if glob && report["error"] == "not_found" => {}
                Err(report) => reports.push(report),
            }
        }
        if glob && !found && reports.is_empty() {
            reports.push(Error { skipped: missed.clone(), ..Error::new("not_found", "no file matched") }.to_json());
        }
        for (ext, n) in missed {
            *skipped.entry(ext).or_default() += n;
        }
        if several {
            reports.iter_mut().for_each(|r| r["item"] = (item + 1).into());
        }
        failures.extend(reports);
    }
    plans.retain(|p| !p.targets.is_empty());
    failures.extend(plans.iter_mut().filter_map(|p| p.overlap(several)));
    if !failures.is_empty() {
        return Ok(failures);
    }
    // A file that cannot be written fails alone: the files before it are written, and their reports say so.
    let mut reports: Vec<Value> =
        plans.iter().map(|p| p.write(check).unwrap_or_else(|e| failure(&p.path, e))).collect();
    if !skipped.is_empty() {
        reports.push(skipped_line(&skipped));
    }
    Ok(reports)
}

/// One place in a file to put a request's `after`.
struct Target<'a> {
    range: Range<usize>,
    after: &'a str,
    place: Place,
    /// The request's index.
    item: usize,
}

/// A file to write: its text before the call and the targets the requests found in it.
struct Plan<'a> {
    path: String,
    src: Source<'a>,
    targets: Vec<Target<'a>>,
}

impl<'a> Plan<'a> {
    /// Opens `path` for `req`; writing a whole file that does not exist yet is writing over an empty one.
    fn open(langs: &'a Languages, path: String, req: &WriteRequest) -> Result<Plan<'a>, Value> {
        let file = Path::new(&path);
        let creating = !file.exists() && req.scope.is_none() && req.before.is_none();
        let src = language(langs, file)
            .and_then(|lang| Ok(Source::new(lang, if creating { String::new() } else { read_text(file)? })))
            .map_err(|e| failure(&path, e))?;
        Ok(Plan { path, src, targets: vec![] })
    }

    /// Adds the targets `req` finds in the file, or reports why it found none, or too many.
    fn find(&mut self, req: &WriteRequest<'a>, item: usize) -> Result<(), Value> {
        let (src, path) = (&self.src, &self.path);
        let regions = match req.scope {
            Some(s) => scope::resolve(s, &src.defs, &src.text).map_err(|e| failure(path, e))?,
            None => vec![Hit { range: 0..src.text.len(), def: None }],
        };
        let mut closest: Option<matcher::Closest> = None;
        let hits: Vec<Hit> = match req.before {
            Some(before) => {
                let tree = src.lang.parser().parse(&src.text, None).expect("parse");
                let leaves = matcher::leaves(src.lang, &tree, &src.text);
                regions
                    .iter()
                    .flat_map(|r| {
                        let found = matcher::find(&src.text, &leaves, r.range.clone(), before);
                        closest = closest.take().or(found.closest);
                        found.matches
                    })
                    .map(|range| Hit { range, def: None })
                    .collect()
            }
            None => regions,
        };
        if hits.is_empty() {
            let mut report = json!({"ok": false, "file": path, "error": "not_found"});
            if let Some(c) = closest {
                let t = &src.text;
                let mismatch = json!({"line": line_of(t, c.mismatch), "expected": c.expected, "found": c.found});
                report["closest"] =
                    json!({"range": lines(line_of(t, c.range.start), line_of(t, c.range.end)), "mismatch": mismatch});
            }
            return Err(report);
        }
        if hits.len() > 1 && !req.all_matches {
            let matches: Vec<Value> = hits.iter().map(|t| candidate(src, t)).collect();
            return Err(json!({"ok": false, "file": path, "error": "ambiguous", "matches": matches}));
        }
        let target = |h: Hit| Target { range: h.range, after: req.after, place: req.place, item };
        self.targets.extend(hits.into_iter().map(target));
        Ok(())
    }

    /// Sorts the targets by place, and reports two that write over the same text, or at the same point.
    fn overlap(&mut self, several: bool) -> Option<Value> {
        self.targets.sort_by_key(|t| t.range.start);
        let (a, b) = self
            .targets
            .windows(2)
            .map(|w| (&w[0], &w[1]))
            .find(|(a, b)| b.range.start < a.range.end.max(a.range.start + 1))?;
        let message = format!("items {} and {} write over the same text", a.item + 1, b.item + 1);
        let mut report = json!({"ok": false, "file": self.path, "error": "overlap", "message": message});
        if several {
            report["item"] = (b.item + 1).into();
        }
        Some(report)
    }

    /// Writes the targets, sorted by place, and reports the lines each now holds in the order of the requests.
    fn write(&self, check: Check) -> Result<Value, Error> {
        let (src, path, file) = (&self.src, &self.path, Path::new(&self.path));
        // Apply from the last target so earlier ranges stay valid, then shift each one's lines by the edits before it.
        let mut text = src.text.clone();
        let mut done: Vec<(edit::Outcome, isize)> = vec![];
        for t in self.targets.iter().rev() {
            let out = edit::edit(src.lang, &text, t.range.clone(), t.after, t.place);
            let grown = out.text.matches('\n').count() as isize - text.matches('\n').count() as isize;
            text = out.text.clone();
            done.push((out, grown));
        }
        done.reverse();
        // Each edit's lines after it are counted in a text without the edits before it, so shift them past those.
        let mut shift = 0;
        let mut replaced = vec![];
        let mut hunks = vec![];
        for (t, (out, grown)) in self.targets.iter().zip(done) {
            let at = |line: usize| (line as isize + shift) as usize;
            replaced.push((t.item, lines(at(out.lines.0), at(out.lines.1))));
            hunks.push(Hunk { old: out.hunk.old, new: at(out.hunk.new.start)..at(out.hunk.new.end) });
            shift += grown;
        }
        replaced.sort_by_key(|(item, _)| *item);
        let replaced: Vec<Value> = replaced.into_iter().map(|(_, r)| r).collect();
        let syntax =
            new_errors(&edit::syntax_errors(src.lang, &src.text), edit::syntax_errors(src.lang, &text), &hunks);
        let line_text = |line: usize| text.lines().nth(line - 1).unwrap_or("").trim().to_string();
        let errors: Vec<Value> = syntax.iter().map(|e| json!({"line": e.line, "text": line_text(e.line)})).collect();
        let io = |e: std::io::Error| Error::new("io", format!("{path}: {e}"));
        std::fs::create_dir_all(file.parent().unwrap_or(Path::new("."))).map_err(io)?;
        std::fs::write(file, &text).map_err(io)?;
        let mut warnings =
            check(&Written { lang: src.lang, path: file, before: &src.text, after: &text, hunks: &hunks });
        if !errors.is_empty() {
            warnings.insert("new_syntax_errors".into(), errors.into());
        }
        let mut report = json!({"ok": true, "file": path, "replaced": replaced});
        if !warnings.is_empty() {
            report["warnings"] = warnings.into();
        }
        Ok(report)
    }
}

/// The report of a file that `e` kept from being written.
fn failure(file: &str, e: Error) -> Value {
    json!({"ok": false, "file": file, "error": e.code, "message": e.message})
}
fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count() + 1
}

/// A line range as `scope` spells it, so the agent can pass it back as is.
fn lines(first: usize, last: usize) -> Value {
    format!("{first}-{last}").into()
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
    json!({"range": lines(line_of(text, range.start), last), "parents": parents, "signature": signature})
}

fn read_one(langs: &Languages, path: &str, scope: Option<&str>) -> Result<String, Error> {
    let src = open(langs, Path::new(path))?;
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
        let out = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "a.rs"), scope: None }]).unwrap().0;
        // Then: ボディを省略したアウトラインが返る
        assert_eq!(out, "1\tfn a() {\n3\t}\n");
    }

    #[test]
    fn reading_glob_prefixes_each_file_with_its_path() {
        // Given: 2 つの Rust のファイル
        let dir = dir_with(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
        // When: glob で読む
        let out = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "src/*.rs"), scope: None }]).unwrap().0;
        // Then: ファイルごとにパスの行が前に付く
        assert_eq!(
            out,
            format!("{}\n1\tfn a() {{}}\n{}\n1\tfn b() {{}}\n", at(&dir, "src/a.rs"), at(&dir, "src/b.rs"))
        );
    }

    #[test]
    fn reading_several_items_puts_each_after_its_path() {
        // Given: 関数が 2 つある Rust のファイルと、関数が 1 つある Rust のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\nfn b() {}\n"), ("b.rs", "fn c() {}\n")]);
        let (a, b) = (at(&dir, "a.rs"), at(&dir, "b.rs"));
        // When: 1 つ目のファイルの定義と、2 つ目のファイル全体を、1 回で読む
        let out =
            read(&Languages::builtin(), &[ReadItem { path: &a, scope: Some("b") }, ReadItem { path: &b, scope: None }]);
        // Then: 要素ごとに、パスの行の後にその内容が並ぶ
        assert_eq!(out.unwrap().0, format!("{a}\n2\tfn b() {{}}\n{b}\n1\tfn c() {{}}\n"));
    }

    #[test]
    fn failing_item_fails_whole_read_with_its_number() {
        // Given: 関数が 1 つある Rust のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        let a = at(&dir, "a.rs");
        // When: ファイル全体と、ない定義を、1 回で読む
        let err = read(
            &Languages::builtin(),
            &[ReadItem { path: &a, scope: None }, ReadItem { path: &a, scope: Some("zzz") }],
        );
        // Then: 全体が not_found になり、2 つ目の要素だとわかる
        let err = err.unwrap_err();
        assert_eq!((err.code, err.item), ("not_found", Some(2)));
    }

    fn read_error(files: &[(&str, &[u8])], name: &str) -> &'static str {
        let dir = tempfile::tempdir().unwrap();
        for (n, bytes) in files {
            std::fs::write(dir.path().join(n), bytes).unwrap();
        }
        read(&Languages::builtin(), &[ReadItem { path: &at(&dir, name), scope: None }]).unwrap_err().code
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
        let reports = write(&Languages::builtin(), &[req], &mut |_| Map::new()).unwrap();
        // Then: 書き込まれ、ok が報告される
        assert_eq!(content(&dir, "a.rs"), "fn a() {\n    y();\n}\n");
        assert_eq!(reports, [serde_json::json!({"ok": true, "file": path, "replaced": ["2-2"]})]);
    }

    fn write_in(dir: &tempfile::TempDir, req: WriteRequest) -> Vec<Value> {
        let path = req.path.to_string();
        let req = WriteRequest { path: &path, ..req };
        let _ = dir;
        write(&Languages::builtin(), &[req], &mut |_| Map::new()).unwrap()
    }

    #[test]
    fn panic_during_write_is_internal_error() {
        // Given: 呼び出しのある Rust のファイルと、panic するチェック
        let dir = dir_with(&[("a.rs", "fn a() { x(); }\n")]);
        let req = WriteRequest { path: &at(&dir, "a.rs"), before: Some("x();"), after: "y();", ..Default::default() };
        // When: 書く
        let err = write(&Languages::builtin(), &[req], &mut |_| panic!("boom")).unwrap_err();
        // Then: panic の内容を持つ internal エラーが返る
        assert_eq!((err.code, err.message.as_str()), ("internal", "boom"));
    }

    fn write_all(reqs: &[WriteRequest]) -> Vec<Value> {
        write(&Languages::builtin(), reqs, &mut |_| Map::new()).unwrap()
    }

    /// `app/[id]/page.tsx` and `app/d/page.tsx`, which `[id]` would match as a glob character class.
    fn bracket_dirs() -> tempfile::TempDir {
        dir_with(&[("app/[id]/page.tsx", "export function A() {}\n"), ("app/d/page.tsx", "export function D() {}\n")])
    }

    #[test]
    fn path_with_brackets_reads_that_file() {
        // Given: 名前に [ と ] を含むディレクトリのファイルと、[id] の文字に一致するディレクトリのファイル
        let dir = bracket_dirs();
        // When: [ と ] を含むパスで読む
        let out = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "app/[id]/page.tsx"), scope: None }]);
        // Then: そのファイルだけが読まれる
        assert_eq!(out.unwrap().0, "1\texport function A() {}\n");
    }

    #[test]
    fn path_with_brackets_writes_that_file() {
        // Given: 名前に [ と ] を含むディレクトリのファイルと、[id] の文字に一致するディレクトリのファイル
        let dir = bracket_dirs();
        // When: [ と ] を含むパスに、ファイル全体を書く
        write_all(&[WriteRequest {
            path: &at(&dir, "app/[id]/page.tsx"),
            after: "export function B() {}",
            ..Default::default()
        }]);
        // Then: そのファイルが書かれ、もう一方は変わらない
        assert_eq!(content(&dir, "app/[id]/page.tsx"), "export function B() {}\n");
        assert_eq!(content(&dir, "app/d/page.tsx"), "export function D() {}\n");
    }

    #[test]
    fn brackets_in_glob_match_themselves() {
        // Given: 名前に [ と ] を含むディレクトリのファイルと、[id] の文字に一致するディレクトリのファイル
        let dir = bracket_dirs();
        // When: [ と ] を含む glob で読む
        let (_, files) =
            read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "app/[id]/*.tsx"), scope: None }]).unwrap();
        // Then: [ と ] は文字どおりに一致する
        assert_eq!(files, [at(&dir, "app/[id]/page.tsx")]);
    }

    #[test]
    #[cfg(windows)]
    fn backslash_glob_matches_on_windows() {
        // Given: 2 つの Rust のファイル
        let dir = dir_with(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
        // When: \ 区切りの glob で読む
        let glob = format!("{}\\src\\*.rs", dir.path().display());
        let (_, files) = read(&Languages::builtin(), &[ReadItem { path: &glob, scope: None }]).unwrap();
        // Then: / 区切りのときと同じファイルに一致する
        assert_eq!(files, [at(&dir, "src/a.rs"), at(&dir, "src/b.rs")]);
    }

    #[test]
    fn reading_reports_files_read() {
        // Given: 2 つの Rust のファイル
        let dir = dir_with(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
        // When: glob とファイルを並べて読む
        let items = [
            ReadItem { path: &at(&dir, "src/*.rs"), scope: None },
            ReadItem { path: &at(&dir, "src/a.rs"), scope: None },
        ];
        let (_, files) = read(&Languages::builtin(), &items).unwrap();
        // Then: 読んだファイルが、要素の順に並ぶ
        assert_eq!(files, [at(&dir, "src/a.rs"), at(&dir, "src/b.rs"), at(&dir, "src/a.rs")]);
    }

    #[test]
    fn file_that_fails_to_be_written_is_reported_with_files_written() {
        // Given: 書けるファイルと、親がファイルなので書けないパス
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("f", "")]);
        let (a, b) = (at(&dir, "a.rs"), at(&dir, "f/b.rs"));
        // When: 2 つを 1 回で書く
        let reports = write_all(&[
            WriteRequest { path: &a, after: "fn b() {}", ..Default::default() },
            WriteRequest { path: &b, after: "fn c() {}", ..Default::default() },
        ]);
        // Then: 書けたファイルは ok と報告され、書けなかったファイルは io エラーと報告される
        assert_eq!(content(&dir, "a.rs"), "fn b() {}\n");
        assert_eq!(reports[0], json!({"ok": true, "file": a, "replaced": ["1-1"]}));
        assert_eq!(
            (&reports[1]["ok"], &reports[1]["file"], &reports[1]["error"]),
            (&json!(false), &json!(b), &json!("io"))
        );
    }

    /// A replace of `before` with `after` in `path`.
    fn swap<'a>(path: &'a str, before: &'a str, after: &'a str) -> WriteRequest<'a> {
        WriteRequest { path, before: Some(before), after, ..Default::default() }
    }

    #[test]
    fn several_items_are_matched_against_original_and_reported_in_their_order() {
        // Given: 呼び出しが 2 つある関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n    y();\n}\n")]);
        let a = at(&dir, "a.rs");
        // When: 後ろの呼び出し、前の呼び出しの順に、2 つの書き換えを 1 回で渡す
        let reports = write_all(&[swap(&a, "y();", "w();"), swap(&a, "x();", "v();")]);
        // Then: どちらも書かれ、replaced は渡した順に並ぶ
        assert_eq!(content(&dir, "a.rs"), "fn a() {\n    v();\n    w();\n}\n");
        assert_eq!(reports, [json!({"ok": true, "file": a, "replaced": ["3-3", "2-2"]})]);
    }

    #[test]
    fn failing_item_writes_nothing_and_has_its_number() {
        // Given: 呼び出しのある関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n}\n")]);
        let a = at(&dir, "a.rs");
        // When: 一致する書き換えと、一致しない書き換えを 1 回で渡す
        let reports = write_all(&[swap(&a, "x();", "v();"), swap(&a, "zzz();", "w();")]);
        // Then: 何も書かれず、2 つ目の要素の not_found が報告される
        assert_eq!(content(&dir, "a.rs"), "fn a() {\n    x();\n}\n");
        assert_eq!((&reports[0]["error"], &reports[0]["item"]), (&json!("not_found"), &json!(2)));
    }

    #[test]
    fn overlapping_items_write_nothing() {
        // Given: 呼び出しが 2 つある関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n    y();\n}\n")]);
        let a = at(&dir, "a.rs");
        // When: 書き換える範囲が重なる 2 つの書き換えを 1 回で渡す
        let reports = write_all(&[swap(&a, "x();", "v();"), swap(&a, "x();\ny();", "w();")]);
        // Then: overlap になり、何も書かれない
        assert_eq!(reports[0]["error"], "overlap", "{reports:?}");
        assert_eq!(content(&dir, "a.rs"), "fn a() {\n    x();\n    y();\n}\n");
    }

    #[test]
    fn items_in_several_files_are_reported_per_file() {
        // Given: 呼び出しのある 2 つのファイル
        let dir = dir_with(&[("a.rs", "fn a() { x(); }\n"), ("b.rs", "fn b() { x(); }\n")]);
        let (a, b) = (at(&dir, "a.rs"), at(&dir, "b.rs"));
        // When: それぞれのファイルへの書き換えを 1 回で渡す
        let reports = write_all(&[swap(&a, "x();", "v();"), swap(&b, "x();", "w();")]);
        // Then: ファイルごとに報告される
        assert_eq!(content(&dir, "b.rs"), "fn b() { w(); }\n");
        assert_eq!(reports.iter().map(|r| r["file"].clone()).collect::<Vec<_>>(), [json!(a), json!(b)]);
    }

    #[test]
    fn failure_in_one_file_writes_no_file() {
        // Given: 呼び出しのある 2 つのファイル
        let dir = dir_with(&[("a.rs", "fn a() { x(); }\n"), ("b.rs", "fn b() { x(); }\n")]);
        let (a, b) = (at(&dir, "a.rs"), at(&dir, "b.rs"));
        // When: 一致する書き換えと、別のファイルで一致しない書き換えを 1 回で渡す
        write_all(&[swap(&a, "x();", "v();"), swap(&b, "zzz();", "w();")]);
        // Then: どちらのファイルも書かれない
        assert_eq!(content(&dir, "a.rs"), "fn a() { x(); }\n");
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
                {"range": "3-5", "parents": ["data object Foo", "companion object"], "signature": "fun bar(x: Int)"},
                {"range": "9-10", "parents": ["class Bar"], "signature": "fun bar(s: String)"},
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
            serde_json::json!({"range": "1-2", "mismatch": {"line": 2, "expected": "hoge(42);", "found": "hoge(1);"}})
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
        assert_eq!(reports[0]["replaced"], serde_json::json!(["2-3", "4-5"]));
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
    fn syntax_error_present_before_write_is_not_reported() {
        // Given: 後ろの関数に構文エラーがあるファイル
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n}\nfn b( {}\n")]);
        // When: 前の関数の中を、行の増える正しい after で書く
        let reports = write_in(
            &dir,
            WriteRequest { path: &at(&dir, "a.rs"), before: Some("x();"), after: "y();\nz();", ..Default::default() },
        );
        // Then: もとからあった構文エラーは報告されない
        assert!(reports[0].get("warnings").is_none(), "{}", reports[0]);
    }

    #[test]
    fn syntax_errors_of_each_match_are_reported_with_lines_after_write() {
        // Given: 同じ呼び出しが 2 つある関数
        let dir = dir_with(&[("a.rs", "fn a() {\n    x();\n    w();\n    x();\n}\n")]);
        // When: 両方を、2 行目の閉じ括弧が足りない 2 行の after で書く
        let reports = write_in(
            &dir,
            WriteRequest {
                path: &at(&dir, "a.rs"),
                before: Some("x();"),
                after: "y();\nz(;",
                all_matches: true,
                ..Default::default()
            },
        );
        // Then: 両方の構文エラーが、書き込み後の行で報告される
        let errors = &reports[0]["warnings"]["new_syntax_errors"];
        assert_eq!(*errors, serde_json::json!([{"line": 3, "text": "z(;"}, {"line": 6, "text": "z(;"}]));
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
        let out = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "**/*.rs"), scope: None }]).unwrap().0;
        // Then: 除外されたファイルは含まれない
        assert!(!out.contains("b.rs"), "{out}");
        assert!(out.contains("a.rs"));
    }

    #[test]
    fn glob_read_skips_files_without_language_package() {
        // Given: Rust のファイルと JSON のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("b.json", "{}")]);
        // When: 両方に一致する glob で読む
        let out = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "*"), scope: None }]).unwrap().0;
        // Then: 言語パッケージのない JSON は飛ばされ、Rust のファイルは読める
        assert!(out.starts_with(&format!("{}\n1\tfn a() {{}}\n", at(&dir, "a.rs"))), "{out}");
        assert!(!out.contains("b.json"), "{out}");
    }

    #[test]
    fn glob_read_matching_no_file_is_not_found() {
        // Given: Rust のファイルだけがあるディレクトリ
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: どのファイルにも一致しない glob で読む
        let err = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "*.kt"), scope: None }]).unwrap_err();
        // Then: not_found になる
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn reading_scope_without_definition_is_not_found_naming_scope() {
        // Given: 関数 a だけがある Rust のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: スコープ b で読む
        let err = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "a.rs"), scope: Some("b") }]).unwrap_err();
        // Then: スコープの名前を挙げた not_found になる
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("b"), "{}", err.message);
    }

    #[test]
    fn glob_read_leaves_files_without_scope_definition_out() {
        // Given: 関数 a を持つファイルと、関数 b を持つファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        // When: 両方に一致する glob を、スコープ a で読む
        let out = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "*.rs"), scope: Some("a") }]).unwrap().0;
        // Then: a を持つファイルだけが出る
        assert_eq!(out, format!("{}\n1\tfn a() {{}}\n", at(&dir, "a.rs")));
    }

    #[test]
    fn glob_read_with_scope_no_file_defines_is_not_found() {
        // Given: 関数 a だけを持つファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n")]);
        // When: glob を、どのファイルにもないスコープ b で読む
        let err = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "*.rs"), scope: Some("b") }]).unwrap_err();
        // Then: not_found になる
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn glob_read_of_only_unsupported_files_is_unsupported_language_with_counts() {
        // Given: 言語パッケージのないファイルだけがあるディレクトリ
        let dir = dir_with(&[("a.json", "{}"), ("b.json", "{}"), ("c.yaml", "a: 1\n")]);
        // When: glob で読む
        let err = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "*"), scope: None }]).unwrap_err();
        // Then: unsupported_language になり、飛ばした拡張子ごとの件数を持つ
        assert_eq!(err.code, "unsupported_language");
        assert_eq!(
            err.to_json()["skipped"],
            serde_json::json!({"reason": "unsupported_language", "extensions": {"json": 2, "yaml": 1}})
        );
    }

    #[test]
    fn glob_read_skipping_some_files_ends_with_counts_as_mison_line() {
        // Given: Rust のファイルと JSON のファイル
        let dir = dir_with(&[("a.rs", "fn a() {}\n"), ("b.json", "{}")]);
        // When: glob で読む
        let out = read(&Languages::builtin(), &[ReadItem { path: &at(&dir, "*"), scope: None }]).unwrap().0;
        // Then: 出力の最後の行に、飛ばした拡張子ごとの件数が MISON で付く
        let last = "skipped=(reason=unsupported_language extensions=(json=1))";
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
