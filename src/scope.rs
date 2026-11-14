use std::ops::Range;

use crate::Error;
use crate::outline::Definition;

/// A resolved region of a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Byte range of the region.
    pub range: Range<usize>,
    /// Index of the definition when the region is a definition.
    pub def: Option<usize>,
}

/// Resolves every part of `spec` (see `parts`), failing with the errors of all parts that do not resolve.
pub fn resolve(spec: &str, defs: &[Definition], src: &str) -> Result<Vec<Hit>, Error> {
    let (mut hits, mut errors) = (vec![], vec![]);
    for (_, found) in resolve_each(spec, defs, src) {
        match found {
            Ok(h) => hits.extend(h),
            Err(e) => errors.push(e),
        }
    }
    joined(errors).map_or(Ok(hits), Err)
}

/// One error for the failed parts of a scope, with the first one's code and the messages of all, or `None`
/// when no part failed.
pub fn joined(errors: impl IntoIterator<Item = Error>) -> Option<Error> {
    let mut errors = errors.into_iter();
    let first = errors.next()?;
    let message = std::iter::once(first.message.clone()).chain(errors.map(|e| e.message)).collect::<Vec<_>>();
    Some(Error { message: message.join("; "), ..first })
}

/// Resolves each part of `spec` on its own, in order.
pub fn resolve_each<'s>(spec: &'s str, defs: &[Definition], src: &str) -> Vec<(&'s str, Result<Vec<Hit>, Error>)> {
    parts(spec, defs).into_iter().map(|part| (part, resolve_one(part, defs, src))).collect()
}

/// The scopes separated by commas in `spec`, or `spec` itself when a definition is named by all of it,
/// as a Markdown heading may be.
pub fn parts<'s>(spec: &'s str, defs: &[Definition]) -> Vec<&'s str> {
    if defs.iter().any(|d| d.name == spec) {
        return vec![spec];
    }
    spec.split(',').map(str::trim).collect()
}

fn resolve_one(spec: &str, defs: &[Definition], src: &str) -> Result<Vec<Hit>, Error> {
    if let Some((n, m)) = parse_lines(spec) {
        let lines = src.lines().count();
        if n == 0 || m < n || m > lines {
            return Err(Error::new("out_of_range", format!("lines {spec} are outside the file (1-{lines})")));
        }
        return Ok(vec![Hit { range: line_span(src, n, m).unwrap(), def: None }]);
    }
    let mut found: Vec<usize> = (0..defs.len()).filter(|&i| defs[i].name == spec).collect();
    if found.is_empty() {
        let segs: Vec<&str> = spec.split("::").flat_map(|s| s.split('.')).collect();
        found = (0..defs.len()).filter(|&i| matches_path(defs, i, &segs)).collect();
    }
    if found.is_empty() {
        return Err(Error::new("not_found", format!("no definition matches {spec}")));
    }
    Ok(found.into_iter().map(|i| Hit { range: defs[i].range.clone(), def: Some(i) }).collect())
}

/// `N` or `N-M`, the scopes that name lines rather than definitions.
pub fn parse_lines(spec: &str) -> Option<(usize, usize)> {
    match spec.split_once('-') {
        Some((n, m)) => Some((n.parse().ok()?, m.parse().ok()?)),
        None => spec.parse().ok().map(|n| (n, n)),
    }
}

/// Byte range from the start of line `first` to the end of line `last` (1-based, without the newline).
pub fn line_span(src: &str, first: usize, last: usize) -> Option<Range<usize>> {
    let mut starts = std::iter::once(0).chain(src.match_indices('\n').map(|(i, _)| i + 1));
    let start = starts.nth(first.checked_sub(1)?)?;
    let end = src[start..].match_indices('\n').nth(last.checked_sub(first)?).map_or(src.len(), |(i, _)| start + i);
    let end = if src[..end].ends_with('\r') { end - 1 } else { end };
    Some(start..end)
}

/// The last segment names the definition and each earlier one names the next parent up.
fn matches_path(defs: &[Definition], i: usize, segs: &[&str]) -> bool {
    let mut cur = Some(i);
    for seg in segs.iter().rev() {
        match cur {
            Some(c) if defs[c].name == *seg => cur = defs[c].parent,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::{defs, rust};

    const QUERY: &str = "(impl_item type: (type_identifier) @name) @definition.impl
        (mod_item name: (identifier) @name) @definition.module
        (function_item name: (identifier) @name body: (block) @body) @definition.function";

    fn names(spec: &str, src: &str) -> Vec<String> {
        let defs = defs(&rust(QUERY), src);
        let hits = resolve(spec, &defs, src).unwrap();
        hits.iter().map(|h| src[h.range.clone()].lines().next().unwrap().to_string()).collect()
    }

    #[test]
    fn single_name_matches_same_name_at_any_depth() {
        // Given: 深さの違う場所に同名の関数 a があるファイル
        let src = "fn a() {}\nimpl Foo {\n    fn a() {}\n}\n";
        // When: スコープ a を解決する
        let hits = names("a", src);
        // Then: どちらの a にも一致する
        assert_eq!(hits, ["fn a() {}", "fn a() {}"]);
    }

    #[test]
    fn dotted_scope_matches_only_definitions_whose_parents_match_from_end() {
        // Given: Foo の中と Bar の中に同名の関数 a があるファイル
        let src = "mod m {\n    impl Foo {\n        fn a() { 1 }\n    }\n}\nimpl Bar {\n    fn a() { 2 }\n}\n";
        // When: スコープ Foo.a を解決する
        let hits = names("Foo.a", src);
        // Then: Foo の中の a にだけ一致する
        assert_eq!(hits, ["fn a() { 1 }"]);
    }

    #[test]
    fn double_colon_is_same_separator_as_dot() {
        // Given: Foo の中と Bar の中に同名の関数 a があるファイル
        let src = "impl Foo {\n    fn a() { 1 }\n}\nimpl Bar {\n    fn a() { 2 }\n}\n";
        // When: スコープ Foo::a を解決する
        let hits = names("Foo::a", src);
        // Then: Foo.a と同じく Foo の中の a にだけ一致する
        assert_eq!(hits, ["fn a() { 1 }"]);
    }

    #[test]
    fn definition_named_by_whole_scope_wins_over_splitting() {
        // Given: 見出し "v1.0" と、見出し "v1" の下の見出し "0" がある Markdown
        let manifest =
            "[package]\nname = \"md\"\nextensions = [\"md\"]\n[package.syntax]\nmodule = \"builtin:markdown\"\n";
        let query = "(section (atx_heading heading_content: (inline) @name)) @definition.section";
        let lang = crate::language::Language::load(manifest, query, None).unwrap();
        let src = "# v1.0\n\ntext\n\n# v1\n\n## 0\n";
        let defs = defs(&lang, src);
        // When: スコープ v1.0 を解決する
        let hits = resolve("v1.0", &defs, src).unwrap();
        // Then: 区切らずに見出し "v1.0" にだけ一致する
        let names: Vec<_> = hits.iter().map(|h| defs[h.def.unwrap()].name.as_str()).collect();
        assert_eq!(names, ["v1.0"]);
    }

    #[test]
    fn comma_separated_scope_points_at_each_part_in_its_order() {
        // Given: 関数が 3 つあるファイル
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        // When: 名前 c、行 2、名前 a を、空白付きの , で区切ったスコープを解決する
        let hits = names("c, 2,a", src);
        // Then: 区切った順に、c、2 行目、a を指す
        assert_eq!(hits, ["fn c() {}", "fn b() {}", "fn a() {}"]);
    }

    #[test]
    fn definition_named_by_whole_scope_with_comma_wins_over_splitting() {
        // Given: 見出し "a, b" と、見出し "a" と "b" がある Markdown
        let manifest =
            "[package]\nname = \"md\"\nextensions = [\"md\"]\n[package.syntax]\nmodule = \"builtin:markdown\"\n";
        let query = "(section (atx_heading heading_content: (inline) @name)) @definition.section";
        let lang = crate::language::Language::load(manifest, query, None).unwrap();
        let src = "# a, b\n\n# a\n\n# b\n";
        let defs = defs(&lang, src);
        // When: スコープ "a, b" を解決する
        let hits = resolve("a, b", &defs, src).unwrap();
        // Then: 区切らずに見出し "a, b" にだけ一致する
        let names: Vec<_> = hits.iter().map(|h| defs[h.def.unwrap()].name.as_str()).collect();
        assert_eq!(names, ["a, b"]);
    }

    #[test]
    fn comma_separated_scope_with_unresolved_part_is_error_naming_it() {
        // Given: 関数 a だけがあるファイル
        let src = "fn a() {}\n";
        // When: a と、ない名前 zzz を , で区切ったスコープを解決する
        let err = resolve("a, zzz", &defs(&rust(QUERY), src), src).unwrap_err();
        // Then: zzz を挙げた not_found のエラーになる
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("zzz"), "{}", err.message);
    }

    #[test]
    fn comma_separated_scope_with_several_unresolved_parts_is_error_naming_each() {
        // Given: 関数 a だけがあるファイル
        let src = "fn a() {}\n";
        // When: a と、ない名前 yyy と zzz を , で区切ったスコープを解決する
        let err = resolve("yyy, a, zzz", &defs(&rust(QUERY), src), src).unwrap_err();
        // Then: yyy と zzz の両方を挙げたエラーになる
        assert!(err.message.contains("yyy") && err.message.contains("zzz"), "{}", err.message);
    }

    fn texts(spec: &str, src: &str) -> Vec<String> {
        resolve(spec, &[], src).unwrap().iter().map(|h| src[h.range.clone()].to_string()).collect()
    }

    #[test]
    fn digits_scope_is_that_single_line() {
        // Given: 3 行のファイル
        let src = "a\nbb\nccc\n";
        // When: スコープ 2 を解決する
        let hits = texts("2", src);
        // Then: 2 行目を指す
        assert_eq!(hits, ["bb"]);
    }

    #[test]
    fn dashed_scope_is_lines_from_n_to_m() {
        // Given: 4 行のファイル
        let src = "a\nbb\nccc\ndddd\n";
        // When: スコープ 2-3 を解決する
        let hits = texts("2-3", src);
        // Then: 2 行目から 3 行目までを指す
        assert_eq!(hits, ["bb\nccc"]);
    }

    #[test]
    fn scope_without_matching_definition_is_not_found() {
        // Given: 関数 a だけがあるファイル
        let src = "fn a() {}\n";
        // When: スコープ b を解決する
        let err = resolve("b", &defs(&rust(QUERY), src), src).unwrap_err();
        // Then: not_found のエラーになる
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn line_range_beyond_file_is_out_of_range_error() {
        // Given: 2 行のファイル
        let src = "a\nb\n";
        // When: スコープ 2-3 を解決する
        let err = resolve("2-3", &[], src).unwrap_err();
        // Then: 範囲外のエラーになる
        assert_eq!(err.code, "out_of_range");
    }
}
