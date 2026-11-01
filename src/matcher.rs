use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use streaming_iterator::StreamingIterator;

use crate::language::Language;

/// How a leaf's text compares with `before` (`@match.collapse`, `@match.quote`).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Compare {
    #[default]
    Exact,
    /// Whitespace runs inside the leaf compare as one.
    Collapse,
    /// The leaf may be any of these quotes in `before`.
    AnyOf(Vec<String>),
}

/// When a leaf may be missing from `before` (`@match.optional`, `@match.trailing`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Skip {
    #[default]
    Never,
    Always,
    /// Only when a closing bracket follows it.
    BeforeClose,
}

/// A token of the file that `before` is matched against.
#[derive(Debug, Clone, Default)]
pub struct Leaf {
    pub range: Range<usize>,
    pub compare: Compare,
    pub skip: Skip,
}

/// Lists the file's tokens in order. A node captured by `@match.*` is one token even if it has children.
pub fn leaves(lang: &Language, tree: &tree_sitter::Tree, src: &str) -> Vec<Leaf> {
    let rules = rules(lang, tree, src);
    let mut out = vec![];
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let rule = rules.get(&node.id());
        if rule.is_some() || node.child_count() == 0 {
            if !node.byte_range().is_empty() {
                out.push(Leaf { range: node.byte_range(), ..rule.cloned().unwrap_or_default() });
            }
        } else if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return out;
            }
        }
    }
}

/// The rules for each captured node, as a `Leaf` without its range; captures on one node combine.
fn rules(lang: &Language, tree: &tree_sitter::Tree, src: &str) -> HashMap<usize, Leaf> {
    let names = lang.query.capture_names();
    let mut out = HashMap::new();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&lang.query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        for c in m.captures() {
            let alt = || {
                let prop = lang.query.property_settings(m.pattern_index).iter().find(|p| {
                    &*p.key == "alt" && p.capture_id.is_none_or(|id| id == c.index as usize)
                });
                let alt = prop.and_then(|p| p.value.as_deref()).unwrap_or("\" '");
                alt.split_whitespace().map(String::from).collect()
            };
            let name = names[c.index as usize];
            if !name.starts_with("match.") {
                continue;
            }
            let rule: &mut Leaf = out.entry(c.node.id()).or_default();
            match name {
                "match.quote" => rule.compare = Compare::AnyOf(alt()),
                "match.collapse" => rule.compare = Compare::Collapse,
                "match.optional" => rule.skip = Skip::Always,
                "match.trailing" => rule.skip = Skip::BeforeClose,
                _ => {}
            }
        }
    }
    out
}

/// Where `before` came closest to matching when it did not match.
#[derive(Debug, Clone, PartialEq)]
pub struct Closest {
    pub range: Range<usize>,
    /// Byte in the file where `before` stopped matching.
    pub mismatch: usize,
    /// The line of `before` that did not match, trimmed.
    pub expected: String,
    /// The line of the file where it did not match, trimmed.
    pub found: String,
}

/// Every non-overlapping match, and where `before` came closest when there is none.
#[derive(Debug, Default)]
pub struct Found {
    pub matches: Vec<Range<usize>>,
    pub closest: Option<Closest>,
}

/// Finds every non-overlapping match of `before` inside `scope`.
pub fn find(src: &str, leaves: &[Leaf], scope: Range<usize>, before: &str) -> Found {
    let mut optional: Vec<&str> = leaves.iter().filter(|l| l.skip == Skip::Always).map(|l| &src[l.range.clone()]).collect();
    optional.sort();
    optional.dedup();
    let leaves: Vec<&Leaf> = leaves.iter().filter(|l| scope.start <= l.range.start && l.range.end <= scope.end).collect();
    let m = Matcher { src, leaves: &leaves, before, optional, start: Cell::new(0), best: Cell::new(None), failed: Default::default() };
    let mut found: Vec<Range<usize>> = vec![];
    for (i, leaf) in leaves.iter().enumerate() {
        if found.last().is_some_and(|f| leaf.range.start < f.end) {
            continue;
        }
        m.start.set(leaf.range.start);
        if let Some(range) = m.start_at(i) {
            found.push(range);
        }
    }
    let closest = m.best.get().filter(|_| found.is_empty()).map(|p| Closest {
        range: p.start..p.matched_end,
        mismatch: p.mismatch,
        expected: line_at(before, p.consumed).to_string(),
        found: line_at(src, p.mismatch).to_string(),
    });
    Found { matches: found, closest }
}

fn line_at(text: &str, byte: usize) -> &str {
    let start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    let end = text[byte..].find('\n').map_or(text.len(), |i| byte + i);
    text[start..end].trim()
}

/// How far one attempt got before failing.
#[derive(Clone, Copy)]
struct Progress {
    /// Bytes of `before` consumed.
    consumed: usize,
    start: usize,
    /// Start and end of the leaf where it failed.
    mismatch: usize,
    matched_end: usize,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn starts_with_word(s: &str) -> bool {
    s.chars().next().is_some_and(is_word)
}

fn ends_with_word(s: &str) -> bool {
    s.chars().next_back().is_some_and(is_word)
}

/// Length of the prefix of `rest` equal to `text` when whitespace runs count as one; both sides need a run.
fn collapsed_prefix(text: &str, rest: &str) -> Option<usize> {
    let (mut t, mut r) = (text.char_indices().peekable(), rest.char_indices().peekable());
    while let Some((_, tc)) = t.next() {
        if tc.is_whitespace() {
            while t.next_if(|(_, c)| c.is_whitespace()).is_some() {}
            r.next_if(|(_, c)| c.is_whitespace())?;
            while r.next_if(|(_, c)| c.is_whitespace()).is_some() {}
        } else if r.next()?.1 != tc {
            return None;
        }
    }
    Some(r.peek().map_or(rest.len(), |(i, _)| *i))
}

struct Matcher<'a> {
    src: &'a str,
    leaves: &'a [&'a Leaf],
    before: &'a str,
    /// Texts of optional leaves in the file; `before` may contain them where the file does not.
    optional: Vec<&'a str>,
    /// Start of the current attempt and the furthest failed attempt so far.
    start: Cell<usize>,
    best: Cell<Option<Progress>>,
    failed: RefCell<HashSet<(usize, usize)>>,
}

impl Matcher<'_> {
    fn text(&self, i: usize) -> &str {
        &self.src[self.leaves[i].range.clone()]
    }

    /// Matches `before` starting at leaf `i`, or at a later part of it that does not split a word.
    fn start_at(&self, i: usize) -> Option<Range<usize>> {
        let leaf = self.leaves[i];
        if let Some(end) = self.at(i, 0, None) {
            return Some(leaf.range.start..end);
        }
        let text = self.text(i);
        let rest = self.before.trim_start();
        let b = self.before.len() - rest.len();
        text.char_indices().skip(1).find_map(|(k, c)| {
            let splits_word = text[..k].chars().next_back().is_some_and(is_word) && is_word(c);
            if splits_word || !rest.starts_with(&text[k..]) {
                return None;
            }
            let end = self.at(i + 1, b + text.len() - k, Some(leaf.range.end))?;
            Some(leaf.range.start + k..end)
        })
    }

    /// Matches the rest of `before` from byte `b` against leaves from index `i`.
    /// `end` is where the match so far ends in the file; returns the final end.
    fn at(&self, i: usize, b: usize, end: Option<usize>) -> Option<usize> {
        // Once started, whether the rest matches depends only on (leaf, position in before);
        // optional leaves reach the same state by several paths, so remember the dead ends.
        let state = (i, b);
        if end.is_some() && self.failed.borrow().contains(&state) {
            return None;
        }
        let found = self.at_uncached(i, b, end);
        if found.is_none() && end.is_some() {
            self.failed.borrow_mut().insert(state);
        }
        found
    }

    fn at_uncached(&self, i: usize, b: usize, end: Option<usize>) -> Option<usize> {
        let rest = self.before[b..].trim_start();
        let spaced = rest.len() < self.before.len() - b;
        let b = self.before.len() - rest.len();
        if rest.is_empty() {
            return end;
        }
        let found = self.leaf_at(i, b, rest, spaced, end).or_else(|| {
            let extra = self.optional.iter().find(|t| rest.starts_with(**t))?;
            end.and_then(|_| self.at(i, b + extra.len(), end))
        });
        if found.is_none() && let Some(matched_end) = end
            && self.best.get().is_none_or(|p| b > p.consumed)
        {
            let leaf = self.leaves.get(i).map_or(matched_end..matched_end, |l| l.range.clone());
            let progress = Progress { consumed: b, start: self.start.get(), mismatch: leaf.start, matched_end: leaf.end };
            self.best.set(Some(progress));
        }
        found
    }

    fn leaf_at(&self, i: usize, b: usize, rest: &str, spaced: bool, end: Option<usize>) -> Option<usize> {
        let leaf = self.leaves.get(i)?;
        let text = self.text(i);
        if end.is_some() && !spaced && ends_with_word(self.text(i - 1)) && starts_with_word(text) {
            return None;
        }
        let consumed = match &leaf.compare {
            Compare::Exact => rest.starts_with(text).then_some(text.len()),
            Compare::Collapse => collapsed_prefix(text, rest),
            Compare::AnyOf(alt) => alt.iter().map(String::as_str).chain([text]).find(|q| rest.starts_with(q)).map(str::len),
        };
        let skippable = match leaf.skip {
            Skip::Never => false,
            Skip::Always => true,
            Skip::BeforeClose => self.leaves.get(i + 1).is_some_and(|_| matches!(self.text(i + 1), ")" | "]" | "}")),
        };
        consumed
            .and_then(|n| self.at(i + 1, b + n, Some(leaf.range.end)))
            .or_else(|| (skippable && end.is_some()).then(|| self.at(i + 1, b, end)).flatten())
            .or_else(|| {
                // The rest of `before` may end inside this leaf, at a point that does not split a word.
                let tail = rest.trim_end();
                let next = text.get(tail.len()..)?.chars().next()?;
                let splits_word = ends_with_word(tail) && is_word(next);
                (end.is_some() && text.starts_with(tail) && !splits_word).then_some(leaf.range.start + tail.len())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::rust;

    fn find_in(lang: &Language, src: &str, before: &str) -> Vec<String> {
        let tree = lang.parser().parse(src, None).unwrap();
        let leaves = leaves(lang, &tree, src);
        find(src, &leaves, 0..src.len(), before).matches.iter().map(|r| src[r.clone()].to_string()).collect()
    }

    #[test]
    fn before_with_different_indentation_matches() {
        // Given: インデントされた文を持つ関数
        let src = "fn a() {\n        x();\n        y();\n}\n";
        // When: インデントの違う before で照合する
        let found = find_in(&rust(""), src, "x();\ny();");
        // Then: 一致する
        assert_eq!(found, ["x();\n        y();"]);
    }

    #[test]
    fn before_with_different_line_breaks_matches() {
        // Given: 引数を 1 行に書いた呼び出し
        let src = "fn a() { f(x, y); }\n";
        // When: 引数ごとに改行した before で照合する
        let found = find_in(&rust(""), src, "f(\n  x,\n  y\n);");
        // Then: 一致する
        assert_eq!(found, ["f(x, y);"]);
    }

    #[test]
    fn before_with_different_spacing_around_symbols_matches() {
        // Given: 演算子の前後に空白のある式
        let src = "fn a() { let z = x + y; }\n";
        // When: 演算子の前後に空白のない before で照合する
        let found = find_in(&rust(""), src, "let z=x+y;");
        // Then: 一致する
        assert_eq!(found, ["let z = x + y;"]);
    }

    #[test]
    fn before_without_space_between_word_leaves_does_not_match() {
        // Given: 単語の葉 let と z が続く文
        let src = "fn a() { let z = 1; }\n";
        // When: 間に空白のない before で照合する
        let found = find_in(&rust(""), src, "letz = 1;");
        // Then: 一致しない
        assert!(found.is_empty());
    }

    #[test]
    fn before_with_different_spacing_inside_string_does_not_match() {
        // Given: 空白が 2 つ並ぶ文字列
        let src = "fn a() { f(\"a  b\"); }\n";
        // When: 空白が 1 つの文字列の before で照合する
        let found = find_in(&rust(""), src, "f(\"a b\");");
        // Then: 一致しない
        assert!(found.is_empty());
    }

    #[test]
    fn collapse_leaf_matches_with_different_whitespace_runs() {
        // Given: コメントの空白をまとめて比べる query と、インデントの付いたブロックコメント
        let lang = rust("(block_comment) @match.collapse");
        let src = "fn a() {\n    /* one\n       two */\n    x();\n}\n";
        // When: コメントの中のインデントが違う before で照合する
        let found = find_in(&lang, src, "/* one\n two */\nx();");
        // Then: 一致する
        assert_eq!(found, ["/* one\n       two */\n    x();"]);
    }

    fn js(query: &str) -> Language {
        let manifest = "[package]\nname = \"js\"\nextensions = [\"js\"]\ngrammar = \"builtin:javascript\"\n";
        Language::load(manifest, query, None).unwrap()
    }

    #[test]
    fn optional_leaf_may_be_missing_from_before() {
        // Given: ; を省略可能にする query
        let lang = js("\";\" @match.optional");
        let src = "x();\ny();\n";
        // When: ; のない before で照合する
        let found = find_in(&lang, src, "x()\ny()");
        // Then: 一致する
        assert_eq!(found, ["x();\ny()"]);
    }

    #[test]
    fn optional_text_only_in_before_is_skipped() {
        // Given: ; を省略可能にする query と、2 文目に ; のないファイル
        let lang = js("\";\" @match.optional");
        let src = "x();\ny()\n";
        // When: 2 文目にも ; のある before で照合する
        let found = find_in(&lang, src, "x();\ny();");
        // Then: before にだけある ; は読み飛ばされて一致する
        assert_eq!(found, ["x();\ny()"]);
    }

    #[test]
    fn trailing_leaf_before_closing_bracket_may_be_missing() {
        // Given: , を末尾のカンマとして扱う query と、末尾にカンマのある配列
        let lang = js("\",\" @match.trailing");
        let src = "x = [1, 2,];\n";
        // When: 末尾のカンマのない before で照合する
        let found = find_in(&lang, src, "x = [1, 2];");
        // Then: 一致する
        assert_eq!(found, ["x = [1, 2,];"]);
    }

    #[test]
    fn trailing_leaf_not_before_closing_bracket_is_required() {
        // Given: , を末尾のカンマとして扱う query と、引数の間にカンマのある呼び出し
        let lang = js("\",\" @match.trailing");
        let src = "f(a, b);\n";
        // When: 引数の間のカンマを省いた before で照合する
        let found = find_in(&lang, src, "f(a b);");
        // Then: 一致しない
        assert!(found.is_empty());
    }

    #[test]
    fn quote_leaf_matches_alternative_quote() {
        // Given: 文字列の区切りを " と ' で入れ替えられる query
        let lang = js(r#"(string ["\"" "'"] @match.quote (#set! @match.quote alt "\" '"))"#);
        let src = "f(\"a\");\n";
        // When: ' で囲んだ before で照合する
        let found = find_in(&lang, src, "f('a');");
        // Then: 一致する
        assert_eq!(found, ["f(\"a\");"]);
    }

    #[test]
    fn head_of_before_may_match_latter_part_of_leaf() {
        // Given: 文字列を引数に取る呼び出し
        let src = "fn a() { f(\"hello world\"); }\n";
        // When: 文字列の途中から始まる before で照合する
        let found = find_in(&rust(""), src, "world\");");
        // Then: 文字列の後半から一致する
        assert_eq!(found, ["world\");"]);
    }

    #[test]
    fn tail_of_before_may_match_former_part_of_leaf() {
        // Given: 文字列を引数に取る呼び出し
        let src = "fn a() { f(\"hello world\"); }\n";
        // When: 文字列の途中で終わる before で照合する
        let found = find_in(&rust(""), src, "f(\"hello");
        // Then: 文字列の前半まで一致する
        assert_eq!(found, ["f(\"hello"]);
    }

    #[test]
    fn matches_are_searched_only_inside_scope() {
        // Given: 2 つの関数に同じ文があるファイル
        let lang = rust("");
        let src = "fn a() { x(); }\nfn b() { x(); }\n";
        let tree = lang.parser().parse(src, None).unwrap();
        let leaves = leaves(&lang, &tree, src);
        // When: 2 つ目の関数の範囲をスコープにして照合する
        let found = find(src, &leaves, 16..src.len(), "x();").matches;
        // Then: スコープの中の一致だけが返る
        assert_eq!(found, vec![25..29]);
    }

    #[test]
    fn all_matches_are_returned() {
        // Given: 同じ文が 2 つあるファイル
        let src = "fn a() { x(); x(); }\n";
        // When: その文で照合する
        let found = find_in(&rust(""), src, "x();");
        // Then: 2 つとも返る
        assert_eq!(found, ["x();", "x();"]);
    }

    fn closest_in(src: &str, before: &str) -> Closest {
        let lang = rust("");
        let tree = lang.parser().parse(src, None).unwrap();
        let leaves = leaves(&lang, &tree, src);
        find(src, &leaves, 0..src.len(), before).closest.unwrap()
    }

    #[test]
    fn closest_candidate_is_where_before_was_consumed_longest() {
        // Given: 途中まで一致する呼び出しが 2 つあるファイル
        let src = "fn a() { x(1); }\nfn b() { x(2, 3); }\n";
        // When: どちらにも一致しない before で照合する
        let closest = closest_in(src, "x(2, 4);");
        // Then: より長く一致した 2 つ目の呼び出しが最も近い候補になる
        assert_eq!(&src[closest.range.clone()], "x(2, 3");
    }

    #[test]
    fn closest_candidate_has_mismatch_line_expected_and_found() {
        // Given: 引数だけが違う呼び出し
        let src = "fn a() {\n    hoge(1);\n}\n";
        // When: 引数の違う before で照合する
        let closest = closest_in(src, "fn a() {\n  hoge(42);\n}");
        // Then: 食い違った行と、before の行と、ファイルの行を持つ
        assert_eq!(src[..closest.mismatch].lines().count(), 2);
        assert_eq!(closest.expected, "hoge(42);");
        assert_eq!(closest.found, "hoge(1);");
    }

    #[test]
    fn many_optional_leaves_do_not_slow_down_failed_match() {
        // Given: ; を省略可能にする query と、; で終わる文が 18 個並ぶファイル
        let lang = js("\";\" @match.optional");
        let src: String = (0..18).map(|i| format!("x{i}();\n")).collect();
        let tree = lang.parser().parse(&src, None).unwrap();
        let leaves = leaves(&lang, &tree, &src);
        // When: 最後の文だけが違う before で照合する
        let before: String = (0..17).map(|i| format!("x{i}();\n")).collect::<String>() + "nope();";
        let start = std::time::Instant::now();
        let found = find(&src, &leaves, 0..src.len(), &before);
        // Then: すぐに一致なしで終わる
        assert!(found.matches.is_empty());
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "took {:?}", start.elapsed());
    }
}
