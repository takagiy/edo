use std::ops::Range;

use crate::diagnostic::{Diagnostic, Hunk};
use crate::language::Language;
use crate::matcher::{Compare, Skip};

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
    /// The lines the edit replaced and the lines it wrote in their place.
    pub hunk: Hunk,
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

/// A `syntax error` for each line where an ERROR or MISSING node starts.
pub fn syntax_errors(lang: &Language, text: &str) -> Vec<Diagnostic> {
    let mut lines: Vec<usize> =
        error_starts(lang, text).into_iter().map(|s| text[..s].matches('\n').count() + 1).collect();
    lines.dedup();
    lines.into_iter().map(|line| Diagnostic { line, message: "syntax error".into() }).collect()
}

/// The 1-based lines that bytes `range` of `text` touch, half-open; an empty range at a line start
/// (an insertion of whole lines) touches none.
fn lines_of(text: &str, range: &Range<usize>) -> Range<usize> {
    let first = text[..range.start].matches('\n').count() + 1;
    if range.is_empty() {
        let at_line_start = range.start == 0 || text[..range.start].ends_with('\n');
        return if at_line_start { first..first } else { first..first + 1 };
    }
    first..text[..range.end - 1].matches('\n').count() + 2
}

pub fn edit(lang: &Language, src: &str, range: Range<usize>, after: &str, place: Place) -> Outcome {
    let (text, replaced) = apply(lang, src, range, after, place);
    let written = replaced.start..replaced.start + text.len() + replaced.len() - src.len();
    let first = text[..written.start].matches('\n').count() + 1;
    let last = text[..written.end].trim_end_matches(['\n', '\r']).matches('\n').count() + 1;
    let lines = if written.is_empty() { (first, first - 1) } else { (first, last.max(first)) };
    let hunk = Hunk { old: lines_of(src, &replaced), new: lines_of(&text, &written) };
    Outcome { text, lines, hunk }
}

/// Number of lines that end at or before byte `pos`, which is a line boundary or the end of `src`.
fn lines_before(src: &str, pos: usize) -> usize {
    src[..pos].split_inclusive('\n').count()
}

/// Returns the edited text and the range of `src` that it replaced.
///
/// A target covering whole lines (and every insertion) is edited as lines: the file is its lines plus
/// whether it ends with a newline, and `after` becomes zero or more lines, so deleting, inserting and
/// replacing are the same splice. Any other target is a span of text inside a line.
fn apply(lang: &Language, src: &str, range: Range<usize>, after: &str, place: Place) -> (String, Range<usize>) {
    let base = indent_of_line_at(src, range.start);
    let nl = if src.contains("\r\n") { "\r\n" } else { "\n" };
    let unit = indent_unit(&indents(src.lines()));
    let after = after.replace("\r\n", "\n");
    let line_start = |b: usize| src[..b].rfind('\n').map_or(0, |i| i + 1);
    let line_end = |b: usize| src[b..].find('\n').map_or(src.len(), |i| b + i + 1);
    let lines = match place {
        Place::InsertBefore => Some(line_start(range.start)..line_start(range.start)),
        Place::InsertAfter => Some(line_end(range.end)..line_end(range.end)),
        Place::Replace => whole_lines(src, &range),
    };
    // Builds the text with `after` reindented (keeping `protected` lines) and where `after` landed in it.
    let build = |protected: &[usize]| -> (String, Range<usize>) {
        let body = reindent(&after, base, unit.clone(), protected);
        let Some(span) = &lines else {
            let start = range.start;
            let body = body.replace('\n', nl);
            return (format!("{}{body}{}", &src[..start], &src[range.end..]), start..start + body.len());
        };
        let mut new: Vec<String> = if after.is_empty() { vec![] } else { body.split('\n').map(String::from).collect() };
        if after.ends_with('\n') {
            new.pop();
        }
        if let Some(first) = new.first_mut().filter(|l| !l.is_empty()) {
            first.insert_str(0, base);
        }
        let mut all: Vec<String> = src.lines().map(String::from).collect();
        let (a, b) = (lines_before(src, span.start), lines_before(src, span.end));
        let start: usize = all[..a].iter().map(|l| l.len() + nl.len()).sum();
        let len = new.join(nl).len();
        all.splice(a..b, new);
        let ends_with_newline = src.is_empty() || src.ends_with('\n');
        let tail = if ends_with_newline && !all.is_empty() { nl } else { "" };
        (all.join(nl) + tail, start..start + len)
    };
    // Embed once with plain reindenting, then parse the whole file to find lines inside literals.
    let (draft, at) = build(&[]);
    let protected = literal_lines(lang, &draft, at.start, draft[at.clone()].split('\n').count());
    let (text, at) = build(&protected);
    let replaced = lines.unwrap_or(range);
    (drop_seam_duplicates(lang, text, at), replaced)
}

/// Removes the file's copy of an optional leaf (such as `;`) that `after` already brings to a seam.
fn drop_seam_duplicates(lang: &Language, mut text: String, inserted: Range<usize>) -> String {
    let tree = lang.parser().parse(&text, None).expect("parse");
    let leaves = crate::matcher::leaves(lang, &tree, &text);
    let inside = |l: &&crate::matcher::Leaf| inserted.start <= l.range.start && l.range.end <= inserted.end;
    let twins = |a: &crate::matcher::Leaf, b: &crate::matcher::Leaf| {
        a.skip == Skip::Always
            && b.skip == Skip::Always
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
        let inside = leaves.iter().any(|l| l.range.start < pos && pos < l.range.end && l.compare != Compare::Collapse);
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

/// Puts the first line as is and each later line at `base` plus its depth relative to a reference line,
/// converting `after`'s indent unit to the file's. The reference is the first line when it is indented;
/// an unindented first line may have been cut from the middle of a line, so then it is the shallowest
/// later line. A later line shallower than the reference goes shallower than `base`.
/// Lines listed in `protected` (by index in `after`) are kept as written.
fn reindent(after: &str, base: &str, file_unit: Option<String>, protected: &[usize]) -> String {
    let first_indent = leading_ws(after);
    let mut lines = after[first_indent.len()..].split('\n');
    let first = lines.next().unwrap_or("");
    let rest: Vec<&str> = lines.collect();
    let free = rest.iter().enumerate().filter(|(k, _)| !protected.contains(&(k + 1))).map(|(_, l)| *l);
    let rest_indents = indents(free);
    let shallowest = rest_indents.iter().min_by_key(|i| i.len()).copied().unwrap_or("");
    let reference = if first_indent.is_empty() || first.trim().is_empty() { shallowest } else { first_indent };
    // The reference is level 0, so a step from it to a later line counts too.
    let after_unit = indent_unit(&[&[reference], &rest_indents[..]].concat());
    let convert = |n: usize| match (&after_unit, &file_unit) {
        (Some(a), Some(f)) => Some(format!("{}{}", f.repeat(n / a.len()), " ".repeat(n % a.len()))),
        _ => None,
    };
    let mut out = first.to_string();
    for (k, line) in rest.into_iter().enumerate() {
        out.push('\n');
        if protected.contains(&(k + 1)) {
            out.push_str(line);
        } else if !line.trim().is_empty() {
            let indent = leading_ws(line);
            match indent.len().checked_sub(reference.len()) {
                Some(n) => {
                    out.push_str(base);
                    out.push_str(&convert(n).unwrap_or_else(|| indent[reference.len()..].to_string()));
                }
                None => {
                    let n = reference.len() - indent.len();
                    let drop = convert(n).map_or(n, |d| d.len());
                    out.push_str(&base[..base.len().saturating_sub(drop)]);
                }
            }
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
    fn indented_first_line_of_after_sets_depth_of_later_lines() {
        // Given: 関数の中の if
        let src = "fn a() {\n    if c {\n        x();\n    }\n}\n";
        // When: 1 行目もインデントして書いた after で、if の開き括弧の行と中身の行を置き換える
        let out = replace(src, "if c {\n        x();", "    if d {\n        y();");
        // Then: 2 行目以降は 1 行目との深さの差を保つ
        assert_eq!(out, "fn a() {\n    if d {\n        y();\n    }\n}\n");
    }

    #[test]
    fn later_line_shallower_than_indented_first_line_goes_shallower_than_target() {
        // Given: 関数の中の if
        let src = "fn a() {\n    if c {\n        x();\n    }\n}\n";
        // When: 1 行目もインデントして書いた after で、if の中身の行と閉じ括弧の行を置き換える
        let out = replace(src, "x();\n    }", "        y();\n    }");
        // Then: 1 行目より浅い閉じ括弧は、置換範囲の先頭行より浅くなる
        assert_eq!(out, "fn a() {\n    if c {\n        y();\n    }\n}\n");
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
        let manifest =
            "[package]\nname = \"js\"\nextensions = [\"js\"]\n[package.syntax]\nmodule = \"builtin:javascript\"\n";
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
    fn insert_after_last_line_without_newline() {
        // Given: 最後の行に改行がないファイル
        let src = "fn a() {}\nfn b() {}";
        // When: 最後の関数の後ろに挿入する
        let out = insert(src, "fn b() {}", "fn c() {}", Place::InsertAfter);
        // Then: 次の行に入り、ファイル末尾に改行がない状態は保たれる
        assert_eq!(out, "fn a() {}\nfn b() {}\nfn c() {}");
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
    fn hunk_has_replaced_lines_before_and_written_lines_after() {
        // Given: 3 行の関数
        let src = "fn a() {\n    x();\n}\n";
        // When: 2 行目を 2 行の after で置き換える
        let out = outcome(src, "x();", "y();\nz();");
        // Then: 前の 2 行目が、後の 2〜3 行目になった
        assert_eq!(out.hunk, Hunk { old: 2..3, new: 2..4 });
    }

    #[test]
    fn line_with_error_node_is_syntax_error() {
        // Given: 2 行目に閉じ括弧の足りない呼び出しがある関数
        let src = "fn a() {\n    y(;\n}\n";
        // When: 構文エラーを求める
        let errors = syntax_errors(&rust(""), src);
        // Then: 2 行目が構文エラーになる
        assert_eq!(errors, [Diagnostic { line: 2, message: "syntax error".into() }]);
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
