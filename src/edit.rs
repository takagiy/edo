use std::ops::Range;

use crate::language::Language;
use crate::matcher::Rule;

/// Where `after` goes relative to the target range.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Place {
    #[default]
    Replace,
    InsertBefore,
    InsertAfter,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub text: String,
    /// First and last line (1-based) of the written text; last is first - 1 when nothing was written.
    pub lines: (usize, usize),
    /// Syntax errors that the edit introduced.
    pub new_syntax_errors: Vec<SyntaxError>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxError {
    pub line: usize,
    pub text: String,
}

/// Start bytes of ERROR and MISSING nodes.
fn error_starts(lang: &Language, text: &str) -> Vec<usize> {
    let tree = lang.parser().parse(text, None).expect("parse");
    let mut out = vec![];
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            out.push(node.start_byte());
        } else if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}

/// Errors in `new` that were not already in `old` (shifted past the edit) are new.
fn new_syntax_errors(lang: &Language, old: &str, new: &str, replaced: &Range<usize>) -> Vec<SyntaxError> {
    let delta = new.len() as isize - old.len() as isize;
    let before: Vec<usize> = error_starts(lang, old)
        .into_iter()
        .filter(|&s| !replaced.contains(&s))
        .map(|s| if s >= replaced.end { (s as isize + delta) as usize } else { s })
        .collect();
    let mut out: Vec<SyntaxError> = vec![];
    for s in error_starts(lang, new).into_iter().filter(|s| !before.contains(s)) {
        let line = new[..s].matches('\n').count() + 1;
        let start = new[..s].rfind('\n').map_or(0, |i| i + 1);
        let text = new[start..].lines().next().unwrap_or("").trim().to_string();
        if !out.iter().any(|e| e.line == line) {
            out.push(SyntaxError { line, text });
        }
    }
    out
}

pub fn edit(lang: &Language, src: &str, range: Range<usize>, after: &str, place: Place) -> Outcome {
    let (text, replaced) = apply(lang, src, range, after, place);
    let new_syntax_errors = new_syntax_errors(lang, src, &text, &replaced);
    let written = replaced.start..replaced.start + text.len() + replaced.len() - src.len();
    let first = text[..written.start].matches('\n').count() + 1;
    let last = text[..written.end].trim_end_matches(['\n', '\r']).matches('\n').count() + 1;
    let lines =if written.is_empty() { (first, first - 1) } else { (first, last.max(first)) };
    Outcome { text, lines, new_syntax_errors }
}

/// Returns the edited text and the range of `src` that it replaced.
fn apply(lang: &Language, src: &str, range: Range<usize>, after: &str, place: Place) -> (String, Range<usize>) {
    if place == Place::Replace && after.trim().is_empty() && let Some(lines) = whole_lines(src, &range) {
        return (format!("{}{}", &src[..lines.start], &src[lines.end..]), lines);
    }
    let base = indent_of_line_at(src, range.start);
    let nl = if src.contains("\r\n") { "\r\n" } else { "\n" };
    // An insertion is whole lines next to the target, starting at the target's indentation.
    let (range, prefix, suffix) = match place {
        Place::Replace => (range, String::new(), ""),
        Place::InsertBefore => {
            let pos = src[..range.start].rfind('\n').map_or(0, |i| i + 1);
            (pos..pos, base.to_string(), nl)
        }
        Place::InsertAfter => match src[range.end..].find('\n') {
            Some(i) => (range.end + i + 1..range.end + i + 1, base.to_string(), nl),
            None => (src.len()..src.len(), format!("{nl}{base}"), ""),
        },
    };
    let unit = indent_unit(&indents(src.lines()));
    let after = after.replace("\r\n", "\n");
    // A leading blank line stays blank instead of carrying the indentation.
    let wrap = |s: String| {
        let prefix = if s.starts_with('\n') { prefix.trim_end_matches([' ', '\t']) } else { &prefix };
        format!("{prefix}{}{suffix}", s.replace('\n', nl))
    };
    let splice = |after: &str| format!("{}{}{}", &src[..range.start], after, &src[range.end..]);
    // Embed once with plain reindenting, then parse the whole file to find lines inside literals.
    let draft = wrap(reindent(&after, base, unit.clone(), &[]));
    let first = range.start + prefix.len();
    let protected = literal_lines(lang, &splice(&draft), first, draft.split('\n').count());
    let after = wrap(reindent(&after, base, unit, &protected));
    let text = drop_seam_duplicates(lang, splice(&after), range.start..range.start + after.len());
    (text, range)
}

/// Removes the file's copy of an optional leaf (such as `;`) that `after` already brings to a seam.
fn drop_seam_duplicates(lang: &Language, mut text: String, inserted: Range<usize>) -> String {
    let tree = lang.parser().parse(&text, None).expect("parse");
    let leaves = crate::matcher::leaves(lang, &tree, &text);
    let inside = |l: &&crate::matcher::Leaf| inserted.start <= l.range.start && l.range.end <= inserted.end;
    let twins = |a: &crate::matcher::Leaf, b: &crate::matcher::Leaf| {
        a.rule == Rule::Optional
            && b.rule == Rule::Optional
            && text[a.range.clone()] == text[b.range.clone()]
            && text[a.range.end..b.range.start].trim().is_empty()
    };
    let end_seam = leaves.iter().rfind(inside).zip(leaves.iter().find(|l| l.range.start >= inserted.end));
    let start_seam = leaves.iter().rfind(|l| l.range.end <= inserted.start).zip(leaves.iter().find(inside));
    let mut drops = vec![];
    if let Some((_, b)) = end_seam.filter(|(a, b)| twins(a, b)) {
        drops.push(b.range.clone());
    }
    if let Some((a, _)) = start_seam.filter(|(a, b)| twins(a, b)) {
        drops.push(a.range.clone());
    }
    for r in drops {
        text.replace_range(r, "");
    }
    text
}

/// The range widened to its whole lines, including the newline, when only whitespace surrounds it on them.
fn whole_lines(src: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let start = src[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let end = src[range.end..].find('\n').map_or(src.len(), |i| range.end + i + 1);
    let only_ws = src[start..range.start].trim().is_empty() && src[range.end..end].trim().is_empty();
    only_ws.then_some(start..end)
}

/// Indexes of the inserted lines (after the first) that start inside a multi-line leaf whose text must stay as is.
fn literal_lines(lang: &Language, text: &str, start: usize, lines: usize) -> Vec<usize> {
    let tree = lang.parser().parse(text, None).expect("parse");
    let leaves = crate::matcher::leaves(lang, &tree, text);
    let mut pos = start;
    let mut out = vec![];
    for k in 1..lines {
        pos += text[pos..].find('\n').map_or(0, |i| i + 1);
        let inside = leaves.iter().any(|l| l.range.start < pos && pos < l.range.end && l.rule != Rule::Collapse);
        if inside {
            out.push(k);
        }
    }
    out
}

fn indent_of_line_at(src: &str, byte: usize) -> &str {
    leading_ws(&src[src[..byte].rfind('\n').map_or(0, |i| i + 1)..])
}

fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn indents<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    lines.filter(|l| !l.trim().is_empty()).map(leading_ws).collect()
}

/// The indentation of one level: a tab, or the most common step between consecutive indents.
fn indent_unit(indents: &[&str]) -> Option<String> {
    let tabs = indents.iter().filter(|i| i.starts_with('\t')).count();
    if tabs > indents.iter().filter(|i| i.starts_with(' ')).count() {
        return Some("\t".into());
    }
    let mut steps: Vec<usize> =
        indents.windows(2).filter(|w| w[1].len() > w[0].len()).map(|w| w[1].len() - w[0].len()).collect();
    steps.sort();
    let step = steps.chunk_by(|a, b| a == b).max_by_key(|c| c.len())?[0];
    Some(" ".repeat(step))
}

/// Puts the first line as is and the later lines at `base` plus their depth relative to the shallowest of them,
/// converting `after`'s indent unit to the file's.
/// Lines listed in `protected` (by index in `after`) are kept as written.
fn reindent(after: &str, base: &str, file_unit: Option<String>, protected: &[usize]) -> String {
    let mut lines = after.trim_start_matches([' ', '\t']).split('\n');
    let first = lines.next().unwrap_or("");
    let rest: Vec<&str> = lines.collect();
    let free = rest.iter().enumerate().filter(|(k, _)| !protected.contains(&(k + 1))).map(|(_, l)| *l);
    let rest_indents = indents(free);
    let shallowest = rest_indents.iter().min_by_key(|i| i.len()).copied().unwrap_or("");
    let min = shallowest.len();
    // The shallowest depth is level 0, so a step from it to the first line counts too.
    let after_unit = indent_unit(&[&[shallowest], &rest_indents[..]].concat());
    let mut out = first.to_string();
    for (k, line) in rest.into_iter().enumerate() {
        out.push('\n');
        if protected.contains(&(k + 1)) {
            out.push_str(line);
        } else if !line.trim().is_empty() {
            let rel = leading_ws(line).len() - min;
            let depth = match (&after_unit, &file_unit) {
                (Some(a), Some(f)) => format!("{}{}", f.repeat(rel / a.len()), " ".repeat(rel % a.len())),
                _ => leading_ws(line)[min..].to_string(),
            };
            out.push_str(base);
            out.push_str(&depth);
            out.push_str(line.trim_start_matches([' ', '\t']));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::rust;

    fn replace(src: &str, target: &str, after: &str) -> String {
        let start = src.find(target).unwrap();
        edit(&rust(""), src, start..start + target.len(), after, Place::Replace).text
    }

    #[test]
    fn target_range_is_replaced_by_after() {
        // Given: 呼び出しを含む関数
        let src = "fn a() { x(); }\n";
        // When: 呼び出しの範囲を after で置き換える
        let out = replace(src, "x();", "y();");
        // Then: その範囲が after になる
        assert_eq!(out, "fn a() { y(); }\n");
    }

    #[test]
    fn first_line_of_after_drops_leading_whitespace() {
        // Given: インデントされた呼び出し
        let src = "fn a() {\n    x();\n}\n";
        // When: 先頭に空白のある after で置き換える
        let out = replace(src, "x();", "      y();");
        // Then: after の 1 行目は先頭の空白を除いて置換範囲の開始位置に入る
        assert_eq!(out, "fn a() {\n    y();\n}\n");
    }

    #[test]
    fn shallowest_later_line_of_after_aligns_with_first_target_line() {
        // Given: impl の中のインデントされた関数
        let src = "impl A {\n    fn a() {\n        x();\n    }\n}\n";
        // When: 2 行目以降をインデントなしで書いた after で関数を置き換える
        let out = replace(src, "fn a() {\n        x();\n    }", "fn b() {\n    y();\n}");
        // Then: 最も浅い行が関数の先頭行と同じ深さになり、相対的な深さは保たれる
        assert_eq!(out, "impl A {\n    fn b() {\n        y();\n    }\n}\n");
    }

    #[test]
    fn indent_unit_of_after_converts_to_file_unit() {
        // Given: 2 スペースでインデントされたファイル
        let src = "impl A {\n  fn a() {\n    x();\n  }\n}\n";
        // When: 4 スペースでインデントした after で関数を置き換える
        let out = replace(src, "fn a() {\n    x();\n  }", "fn b() {\n    y();\n}");
        // Then: after の 1 段がファイルの 1 段（2 スペース）に換算される
        assert_eq!(out, "impl A {\n  fn b() {\n    y();\n  }\n}\n");
    }

    #[test]
    fn spaces_not_divisible_by_unit_remain() {
        // Given: 2 スペースでインデントされたファイル
        let src = "impl A {\n  fn a() {\n    x();\n  }\n}\n";
        // When: 4 スペースの単位で、引数の位置を 2 スペース余分に揃えた after で置き換える
        let out = replace(src, "fn a() {\n    x();\n  }", "fn b() {\n    f(a,\n      b);\n}");
        // Then: 1 段は換算され、余りの 2 スペースはそのまま残る
        assert_eq!(out, "impl A {\n  fn b() {\n    f(a,\n      b);\n  }\n}\n");
    }

    #[test]
    fn lines_inside_multiline_string_keep_their_indentation() {
        // Given: インデントされた文
        let src = "fn a() {\n    x();\n}\n";
        // When: 複数行の文字列を含む after で置き換える
        let out = replace(src, "x();", "let s = \"one\n  two\";\ny();");
        // Then: 文字列の中の行はそのまま、それ以外の行は付け直される
        assert_eq!(out, "fn a() {\n    let s = \"one\n  two\";\n    y();\n}\n");
    }

    fn replace_in(lang: &Language, src: &str, target: &str, after: &str) -> String {
        let start = src.find(target).unwrap();
        edit(lang, src, start..start + target.len(), after, Place::Replace).text
    }

    #[test]
    fn lines_inside_collapse_leaf_are_reindented() {
        // Given: ブロックコメントの空白をまとめて比べる query と、インデントされた文
        let lang = rust("(block_comment) @match.collapse");
        let src = "fn a() {\n    x();\n}\n";
        // When: 複数行のブロックコメントを含む after で置き換える
        let out = replace_in(&lang, src, "x();", "/* one\n    two */\ny();");
        // Then: コメントの中の行もインデントが付け直される
        assert_eq!(out, "fn a() {\n    /* one\n        two */\n    y();\n}\n");
    }

    #[test]
    fn duplicate_optional_leaf_at_seam_removes_file_side() {
        // Given: ; を省略可能にする query と、; で終わる文
        let manifest = "[package]\nname = \"js\"\nextensions = [\"js\"]\ngrammar = \"builtin:javascript\"\n";
        let lang = Language::load(manifest, "\";\" @match.optional", None).unwrap();
        let src = "x();\nz();\n";
        // When: ; の手前までを、; で終わる after で置き換える
        let out = replace_in(&lang, src, "x()", "y();");
        // Then: 継ぎ目で重なったファイル側の ; が消える
        assert_eq!(out, "y();\nz();\n");
    }

    #[test]
    fn crlf_file_stays_crlf() {
        // Given: CRLF の改行のファイル
        let src = "fn a() {\r\n    x();\r\n}\r\n";
        // When: LF で書いた複数行の after で置き換える
        let out = replace(src, "x();", "y();\nz();");
        // Then: CRLF のまま書かれる
        assert_eq!(out, "fn a() {\r\n    y();\r\n    z();\r\n}\r\n");
    }

    #[test]
    fn emptying_whole_line_range_removes_the_lines() {
        // Given: impl の中のインデントされた関数
        let src = "impl A {\n    fn a() {}\n    fn b() {}\n}\n";
        // When: 関数 a の行全体を空の after で置き換える
        let out = replace(src, "fn a() {}", "");
        // Then: その行が消え、空行も残らない
        assert_eq!(out, "impl A {\n    fn b() {}\n}\n");
    }

    fn insert(src: &str, target: &str, after: &str, place: Place) -> String {
        let start = src.find(target).unwrap();
        edit(&rust(""), src, start..start + target.len(), after, place).text
    }

    #[test]
    fn insert_after_goes_to_next_line_of_target() {
        // Given: 2 つの関数
        let src = "fn a() {}\nfn c() {}\n";
        // When: 関数 a の後ろに挿入する
        let out = insert(src, "fn a() {}", "fn b() {}", Place::InsertAfter);
        // Then: a の次の行に入る
        assert_eq!(out, "fn a() {}\nfn b() {}\nfn c() {}\n");
    }

    #[test]
    fn insert_before_goes_before_first_line_of_target() {
        // Given: 2 つの関数
        let src = "fn a() {}\nfn c() {}\n";
        // When: 関数 c の前に挿入する
        let out = insert(src, "fn c() {}", "fn b() {}", Place::InsertBefore);
        // Then: c の行の前に入る
        assert_eq!(out, "fn a() {}\nfn b() {}\nfn c() {}\n");
    }

    #[test]
    fn inserted_lines_take_depth_of_first_target_line() {
        // Given: impl の中のインデントされた関数
        let src = "impl A {\n    fn a() {\n        x();\n    }\n}\n";
        // When: インデントなしで書いた複数行の関数を a の後ろに挿入する
        let out = insert(src, "fn a() {\n        x();\n    }", "fn b() {\n    y();\n}", Place::InsertAfter);
        // Then: 挿入した行は a の先頭行と同じ深さになる
        assert_eq!(out, "impl A {\n    fn a() {\n        x();\n    }\n    fn b() {\n        y();\n    }\n}\n");
    }

    #[test]
    fn leading_blank_line_of_after_is_kept() {
        // Given: impl の中のインデントされた関数
        let src = "impl A {\n    fn a() {}\n}\n";
        // When: 空行から始まる after を a の後ろに挿入する
        let out = insert(src, "fn a() {}", "\nfn b() {}", Place::InsertAfter);
        // Then: 空行が残り、関数は a と同じ深さになる
        assert_eq!(out, "impl A {\n    fn a() {}\n\n    fn b() {}\n}\n");
    }

    fn outcome(src: &str, target: &str, after: &str) -> Outcome {
        let start = src.find(target).unwrap();
        edit(&rust(""), src, start..start + target.len(), after, Place::Replace)
    }

    #[test]
    fn syntax_error_introduced_by_edit_is_reported() {
        // Given: 正しい関数
        let src = "fn a() {\n    x();\n}\n";
        // When: 閉じ括弧の足りない after で置き換える
        let out = outcome(src, "x();", "y(;");
        // Then: 書き換えた行の構文エラーが報告される
        assert_eq!(out.new_syntax_errors, [SyntaxError { line: 2, text: "y(;".into() }]);
    }

    #[test]
    fn syntax_error_present_before_edit_is_not_reported() {
        // Given: 後ろの関数に構文エラーがあるファイル
        let src = "fn a() {\n    x();\n}\nfn b( {}\n";
        // When: 前の関数の中を正しい after で置き換える
        let out = outcome(src, "x();", "y();\nz();");
        // Then: もとからあった構文エラーは報告されない
        assert!(out.new_syntax_errors.is_empty());
    }

    #[test]
    fn lines_of_replaced_range_after_edit_are_returned() {
        // Given: 3 行の関数
        let src = "fn a() {\n    x();\n}\n";
        // When: 中の文を 2 行の after で置き換える
        let out = outcome(src, "x();", "y();\nz();");
        // Then: 書き換えた後の 2〜3 行目が返る
        assert_eq!(out.lines, (2, 3));
    }
}
