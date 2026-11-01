use std::ops::Range;

use crate::outline::Definition;
use crate::scope::Hit;

/// Renders the lines overlapping `region` with line numbers, eliding bodies when `elide` is set.
pub fn render(src: &str, defs: &[Definition], region: Range<usize>, elide: bool) -> String {
    let hidden: Vec<Range<usize>> = if elide { defs.iter().flat_map(|d| d.bodies.clone()).collect() } else { vec![] };
    let docs: Vec<Range<usize>> = defs.iter().flat_map(|d| d.docs.clone()).collect();
    let is_hidden = |b: usize| hidden.iter().any(|r| r.contains(&b)) && !docs.iter().any(|r| r.contains(&b));
    let mut out = String::new();
    let mut start = 0;
    for (i, line) in src.split_inclusive('\n').enumerate() {
        let end = start + line.len();
        if start < region.end.max(region.start + 1) && end > region.start {
            let text = line.trim_end_matches(['\n', '\r']);
            let mut visible = text.char_indices().filter(|(_, c)| !c.is_whitespace()).peekable();
            let hide = match visible.peek() {
                Some(_) => visible.all(|(o, _)| is_hidden(start + o)),
                None => is_hidden(start),
            };
            if !hide {
                out.push_str(&format!("{}\t{}\n", i + 1, truncate(&collapse_hidden(text, start, &is_hidden))));
            }
        }
        start = end;
    }
    out
}

/// Renders one definition; only a function shows its own body (and everything inside it).
pub fn render_definition(src: &str, defs: &[Definition], i: usize) -> String {
    render(src, defs, defs[i].range.clone(), !defs[i].is_function())
}

/// Renders resolved scope hits, separated by a blank line. Line ranges are shown in full.
pub fn render_hits(src: &str, defs: &[Definition], hits: &[Hit]) -> String {
    let parts: Vec<String> = hits
        .iter()
        .map(|h| match h.def {
            Some(i) => render_definition(src, defs, i),
            None => render(src, defs, h.range.clone(), false),
        })
        .collect();
    parts.join("\n")
}

const MAX_LINE_CHARS: usize = 2000;

fn truncate(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((cut, _)) => format!("{}…(+{} chars)", &line[..cut], line[cut..].chars().count()),
        None => line.to_string(),
    }
}

/// Replaces each run of hidden text in a line with `...`, keeping the whitespace around it.
fn collapse_hidden(text: &str, start: usize, is_hidden: &impl Fn(usize) -> bool) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |out: &mut String, run: &mut String| {
        let core = run.trim();
        if core.is_empty() {
            out.push_str(run);
        } else {
            let lead = &run[..run.len() - run.trim_start().len()];
            let trail = &run[run.trim_end().len()..];
            out.push_str(&format!("{lead}...{trail}"));
        }
        run.clear();
    };
    for (o, c) in text.char_indices() {
        if is_hidden(start + o) {
            run.push(c);
        } else {
            flush(&mut out, &mut run);
            out.push(c);
        }
    }
    flush(&mut out, &mut run);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outline::tests::{defs, rust};

    const QUERY: &str = "(impl_item type: (type_identifier) @name) @definition.impl
        (function_item name: (identifier) @name body: (block) @body) @definition.function";

    fn outline(src: &str) -> String {
        let defs = defs(&rust(QUERY), src);
        render(src, &defs, 0..src.len(), true)
    }

    #[test]
    fn each_line_is_prefixed_with_line_number_and_tab() {
        // Given: 定義のないファイル
        let src = "use a;\nuse b;\n";
        // When: 表示する
        let out = outline(src);
        // Then: 各行の前に行番号とタブが付く
        assert_eq!(out, "1\tuse a;\n2\tuse b;\n");
    }

    fn python_outline(src: &str) -> String {
        let manifest = "[package]\nname = \"py\"\nextensions = [\"py\"]\ngrammar = \"builtin:python\"\n";
        let query = "(function_definition name: (identifier) @name
            body: (block . (expression_statement (string) @doc)?) @body) @definition.function";
        let lang = crate::language::Language::load(manifest, query, None).unwrap();
        render(src, &defs(&lang, src), 0..src.len(), true)
    }

    #[test]
    fn line_entirely_inside_body_is_hidden() {
        // Given: ボディが複数行の関数
        let src = "def a():\n    x()\n    y()\nz = 1\n";
        // When: 表示する
        let out = python_outline(src);
        // Then: ボディの中だけの行は表示されない
        assert_eq!(out, "1\tdef a():\n4\tz = 1\n");
    }

    #[test]
    fn blank_line_outside_body_is_shown() {
        // Given: 関数の前後に空行があり、ボディの中にも空行があるファイル
        let src = "x = 1\n\ndef a():\n    x()\n\n    y()\n\nz = 1\n";
        // When: 表示する
        let out = python_outline(src);
        // Then: ボディの外の空行だけが表示される
        assert_eq!(out, "1\tx = 1\n2\t\n3\tdef a():\n7\t\n8\tz = 1\n");
    }

    #[test]
    fn body_sharing_line_with_outer_code_becomes_ellipsis() {
        // Given: ボディが def と同じ行にある関数
        let src = "def a(): return 1\n";
        // When: 表示する
        let out = python_outline(src);
        // Then: ボディの部分が ... になる
        assert_eq!(out, "1\tdef a(): ...\n");
    }

    #[test]
    fn delimiters_at_body_edges_are_shown() {
        // Given: { } で囲まれたボディを持つ関数
        let src = "fn a() {\n    x();\n}\nfn b() { y() }\n";
        // When: 表示する
        let out = outline(src);
        // Then: 区切り記号の { と } は表示される
        assert_eq!(out, "1\tfn a() {\n3\t}\n4\tfn b() { ... }\n");
    }

    #[test]
    fn doc_inside_body_is_shown() {
        // Given: docstring を持つ関数
        let src = "def a():\n    \"\"\"Doc.\n\n    More.\n    \"\"\"\n    x()\n";
        // When: 表示する
        let out = python_outline(src);
        // Then: ボディの中の docstring は表示される
        assert_eq!(out, "1\tdef a():\n2\t    \"\"\"Doc.\n3\t\n4\t    More.\n5\t    \"\"\"\n");
    }

    fn show(src: &str, name: &str) -> String {
        let defs = defs(&rust(QUERY), src);
        let i = defs.iter().position(|d| d.name == name).unwrap();
        render_definition(src, &defs, i)
    }

    #[test]
    fn showing_function_definition_keeps_its_body() {
        // Given: impl の中の関数
        let src = "impl Foo {\n    fn a() {\n        x();\n    }\n}\n";
        // When: 関数 a を表示する
        let out = show(src, "a");
        // Then: ボディは省略されない
        assert_eq!(out, "2\t    fn a() {\n3\t        x();\n4\t    }\n");
    }

    #[test]
    fn showing_non_function_definition_elides_inner_function_bodies() {
        // Given: impl の中の関数
        let src = "impl Foo {\n    fn a() {\n        x();\n    }\n}\n";
        // When: impl Foo を表示する
        let out = show(src, "Foo");
        // Then: 中の関数のボディは省略される
        assert_eq!(out, "1\timpl Foo {\n2\t    fn a() {\n4\t    }\n5\t}\n");
    }

    #[test]
    fn line_longer_than_2000_chars_is_truncated_with_rest_count() {
        // Given: 2005 文字の行
        let src = format!("{}\n", "x".repeat(2005));
        // When: 表示する
        let out = outline(&src);
        // Then: 2000 文字で切り詰められ、残りの文字数が示される
        assert_eq!(out, format!("1\t{}…(+5 chars)\n", "x".repeat(2000)));
    }

    #[test]
    fn showing_several_definitions_separates_them_with_blank_line() {
        // Given: 同名の関数 a が 2 つ一致したスコープ
        let src = "fn a() {}\nimpl Foo {\n    fn a() {}\n}\n";
        let defs = defs(&rust(QUERY), src);
        let hits = crate::scope::resolve("a", &defs, src).unwrap();
        // When: 一致したものを表示する
        let out = render_hits(src, &defs, &hits);
        // Then: 間に空行が入る
        assert_eq!(out, "1\tfn a() {}\n\n3\t    fn a() {}\n");
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;
    use crate::language::Languages;

    fn builtin_outline(file: &str) -> String {
        let langs = Languages::builtin();
        let lang = langs.for_path(std::path::Path::new(file)).unwrap();
        let src = std::fs::read_to_string(file).unwrap();
        let tree = lang.parser().parse(&src, None).unwrap();
        let defs = crate::outline::outline(lang, &tree, &src);
        render(&src, &defs, 0..src.len(), true)
    }

    #[test]
    fn rust_outline() {
        // Given: Rust のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.rs"));
    }

    #[test]
    fn typescript_outline() {
        // Given: TypeScript のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.ts"));
    }

    #[test]
    fn tsx_outline() {
        // Given: TSX のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.tsx"));
    }

    #[test]
    fn javascript_outline() {
        // Given: JavaScript のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.js"));
    }

    #[test]
    fn python_outline() {
        // Given: Python のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.py"));
    }

    #[test]
    fn go_outline() {
        // Given: Go のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.go"));
    }

    #[test]
    fn java_outline() {
        // Given: Java のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.java"));
    }

    #[test]
    fn kotlin_outline() {
        // Given: Kotlin のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.kt"));
    }

    #[test]
    fn markdown_outline() {
        // Given: Markdown のサンプル / When: 表示する / Then: スナップショットどおりのアウトラインになる
        insta::assert_snapshot!(builtin_outline("tests/fixtures/outline/sample.md"));
    }
}
