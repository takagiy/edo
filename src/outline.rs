use std::ops::Range;

use streaming_iterator::StreamingIterator;

use crate::language::Language;

#[derive(Debug, Clone)]
pub struct Definition {
    pub name: String,
    /// Byte range including attached docs.
    pub range: Range<usize>,
    /// Byte range of the definition node itself.
    pub node: Range<usize>,
    /// Attached docs, either adjacent before the node or inside it.
    pub docs: Vec<Range<usize>>,
    /// Elidable regions (inside any delimiters); a definition with bodies is a function.
    pub bodies: Vec<Range<usize>>,
    /// Index of the innermost enclosing definition.
    pub parent: Option<usize>,
}

impl Definition {
    pub fn is_function(&self) -> bool {
        !self.bodies.is_empty()
    }

    pub fn header(&self, src: &str) -> String {
        let rest = &src[self.node.start..];
        let line = rest.lines().next().unwrap_or("").trim();
        line.strip_suffix(['{', '(', '[']).unwrap_or(line).trim_end().to_string()
    }
}

pub fn outline(lang: &Language, tree: &tree_sitter::Tree, src: &str) -> Vec<Definition> {
    let names = lang.query.capture_names();
    let mut defs: Vec<(usize, Definition)> = vec![];
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&lang.query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        let caps = m.captures();
        let Some(def) = caps.iter().find(|c| names[c.index as usize].starts_with("definition.")) else {
            continue;
        };
        let id = def.node.id();
        let i = match defs.iter().position(|(i, _)| *i == id) {
            Some(i) => i,
            None => {
                let node = def.node.byte_range();
                defs.push((
                    id,
                    Definition {
                        name: String::new(),
                        range: node.clone(),
                        node,
                        docs: vec![],
                        bodies: vec![],
                        parent: None,
                    },
                ));
                defs.len() - 1
            }
        };
        let d = &mut defs[i].1;
        if let Some(c) = caps.iter().find(|c| names[c.index as usize] == "name") {
            d.name = src[c.node.byte_range()].trim().to_string();
        } else if d.name.is_empty()
            && let Some(v) = lang.query.property_settings(m.pattern_index).iter().find(|p| &*p.key == "name")
        {
            d.name = v.value.as_deref().unwrap_or_default().to_string();
        }
        for c in caps.iter().filter(|c| names[c.index as usize] == "body") {
            let inner = inner_range(c.node);
            if !d.bodies.contains(&inner) {
                d.bodies.push(inner);
            }
        }
        for c in caps.iter().filter(|c| names[c.index as usize] == "doc") {
            if !d.docs.contains(&c.node.byte_range()) {
                d.docs.push(c.node.byte_range());
            }
        }
    }
    // Patterns that only attach docs capture nodes without a name; those are not definitions.
    let mut defs: Vec<Definition> =
        defs.into_iter().map(|(_, d)| d).filter(|d| !d.name.is_empty()).map(|d| attach_docs(d, src)).collect();
    defs.sort_by_key(|d| (d.range.start, std::cmp::Reverse(d.range.end)));
    let mut stack: Vec<usize> = vec![];
    for i in 0..defs.len() {
        while let Some(&top) = stack.last() {
            if defs[top].node.start <= defs[i].range.start && defs[i].range.end <= defs[top].node.end {
                break;
            }
            stack.pop();
        }
        defs[i].parent = stack.last().copied();
        stack.push(i);
    }
    defs
}

/// The body without its delimiters when it starts and ends with anonymous nodes such as `{` `}`.
fn inner_range(node: tree_sitter::Node) -> Range<usize> {
    let n = node.child_count();
    match (node.child(0), n.checked_sub(1).and_then(|i| node.child(i))) {
        (Some(first), Some(last)) if n >= 2 && !first.is_named() && !last.is_named() => {
            first.end_byte()..last.start_byte()
        }
        _ => node.byte_range(),
    }
}

/// Keeps docs inside the node and the docs chained directly above it (no blank line between).
fn attach_docs(mut d: Definition, src: &str) -> Definition {
    d.docs.sort_by_key(|r| r.start);
    let mut start = d.node.start;
    for r in d.docs.iter().rev().filter(|r| r.end <= d.node.start) {
        let gap = &src[r.end..start];
        if !gap.trim().is_empty() || gap.matches('\n').count() > 1 {
            break;
        }
        start = r.start;
    }
    d.docs.retain(|r| r.start >= start);
    d.range = start..d.node.end;
    d.bodies = merge_bodies(std::mem::take(&mut d.bodies), src);
    for b in &mut d.bodies {
        if b.end < d.node.end && src[b.end..d.node.end].trim().is_empty() {
            b.end = d.node.end;
        }
    }
    d
}

/// Joins bodies that only have whitespace between them, so the gap is elided too.
fn merge_bodies(mut bodies: Vec<Range<usize>>, src: &str) -> Vec<Range<usize>> {
    bodies.sort_by_key(|r| r.start);
    let mut out: Vec<Range<usize>> = vec![];
    for r in bodies {
        match out.last_mut() {
            Some(last) if last.end <= r.start && src[last.end..r.start].trim().is_empty() => last.end = r.end,
            Some(last) if r.end <= last.end => {}
            _ => out.push(r),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn rust(query: &str) -> Language {
        let manifest =
            "[package]\nname = \"rust\"\nextensions = [\"rs\"]\n[package.syntax]\nmodule = \"builtin:rust\"\n";
        Language::load(manifest, query, None).unwrap()
    }

    pub(crate) fn defs(lang: &Language, src: &str) -> Vec<Definition> {
        let tree = lang.parser().parse(src, None).unwrap();
        outline(lang, &tree, src)
    }

    #[test]
    fn definition_node_and_name_capture_make_definition() {
        // Given: 関数を定義に、識別子を名前にする query
        let lang = rust("(function_item name: (identifier) @name) @definition.function");
        // When: 関数が 2 つあるファイルから定義を抽出する
        let defs = defs(&lang, "fn a() {}\nfn b() {}\n");
        // Then: 関数ごとに名前を持つ定義になる
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(defs[1].range, 10..19);
    }

    #[test]
    fn doc_before_definition_is_part_of_definition_range() {
        // Given: 直前のコメントを doc として紐づける query
        let lang = rust("((line_comment)* @doc . (function_item name: (identifier) @name) @definition.function)");
        // When: doc コメント付きの関数から定義を抽出する
        let src = "// unrelated\n\n/// first\n/// second\nfn a() {}\n";
        let defs = defs(&lang, src);
        // Then: 定義の範囲は連続した doc の先頭から始まる
        assert_eq!(&src[defs[0].range.clone()], "/// first\n/// second\nfn a() {}");
    }

    #[test]
    fn nested_definition_has_enclosing_definition_as_parent() {
        // Given: impl と関数を定義にする query
        let lang = rust(
            "(impl_item type: (type_identifier) @name) @definition.impl
             (function_item name: (identifier) @name) @definition.function",
        );
        // When: impl の中に関数があるファイルから定義を抽出する
        let defs = defs(&lang, "impl Foo {\n    fn a() {}\n}\nfn b() {}\n");
        // Then: 中の関数は impl を親に持ち、外の関数は親を持たない
        let parents: Vec<_> = defs.iter().map(|d| (d.name.as_str(), d.parent)).collect();
        assert_eq!(parents, [("Foo", None), ("a", Some(0)), ("b", None)]);
    }

    #[test]
    fn header_is_first_line_without_surrounding_whitespace() {
        // Given: 構造体を定義にする query
        let lang = rust("(struct_item name: (type_identifier) @name) @definition.struct");
        // When: インデントされた構造体の定義を抽出する
        let src = "mod m {\n    struct S;   \n}\n";
        let defs = defs(&lang, src);
        // Then: 見出しは前後の空白を除いた先頭行になる
        assert_eq!(defs[0].header(src), "struct S;");
    }

    #[test]
    fn header_drops_trailing_open_bracket() {
        // Given: 関数を定義にする query
        let lang = rust("(function_item name: (identifier) @name) @definition.function");
        // When: 先頭行が { で終わる関数を抽出する
        let src = "fn a(x: u32) {\n}\n";
        let defs = defs(&lang, src);
        // Then: 見出しから末尾の { が除かれる
        assert_eq!(defs[0].header(src), "fn a(x: u32)");
    }

    #[test]
    fn header_skips_doc_and_uses_first_line_of_definition_node() {
        // Given: 直前のコメントを doc として紐づける query
        let lang = rust("((line_comment)* @doc . (function_item name: (identifier) @name) @definition.function)");
        // When: doc 付きの関数を抽出する
        let src = "/// doc\nfn a() {\n}\n";
        let defs = defs(&lang, src);
        // Then: 見出しは doc ではなく関数の先頭行から作られる
        assert_eq!(defs[0].header(src), "fn a()");
    }

    #[test]
    fn definition_with_body_is_function() {
        // Given: 関数にだけ @body を付ける query
        let lang = rust(
            "(impl_item type: (type_identifier) @name) @definition.impl
             (function_item name: (identifier) @name body: (block) @body) @definition.function",
        );
        // When: impl と関数を抽出する
        let defs = defs(&lang, "impl Foo {\n    fn a() {}\n}\n");
        // Then: @body を持つ定義だけが関数になる
        let kinds: Vec<_> = defs.iter().map(|d| (d.name.as_str(), d.is_function())).collect();
        assert_eq!(kinds, [("Foo", false), ("a", true)]);
    }

    #[test]
    fn definition_without_name_is_dropped() {
        // Given: コメントの次のノードを名前なしで定義として捕まえる query
        let lang = rust("((line_comment)+ @doc . (_) @definition.doc)");
        // When: コメントの後に構造体があるファイルから定義を抽出する
        let defs = defs(&lang, "// c\nstruct S;\n");
        // Then: 名前のない定義は残らない
        assert!(defs.is_empty());
    }

    #[test]
    fn doc_and_name_from_different_patterns_merge_into_one_definition() {
        // Given: doc を付けるパターンと、名前を付けるパターンが別にある query
        let lang = rust(
            "((line_comment)+ @doc . (_) @definition.doc)
             (struct_item name: (type_identifier) @name) @definition.struct",
        );
        // When: コメント付きの構造体を抽出する
        let src = "// c\nstruct S;\n";
        let defs = defs(&lang, src);
        // Then: doc と名前を持つ 1 つの定義になる
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "S");
        assert_eq!(&src[defs[0].range.clone()], "// c\nstruct S;");
    }

    #[test]
    fn bodies_separated_only_by_whitespace_merge_into_one() {
        // Given: 関数の中の文をそれぞれボディにする query
        let lang = rust("(function_item name: (identifier) @name body: (block (_) @body)) @definition.function");
        // When: 文が空行を挟んで並ぶ関数を抽出する
        let src = "fn a() {\n    x();\n\n    y();\n}\n";
        let defs = defs(&lang, src);
        // Then: 1 つのボディにまとまる
        let bodies: Vec<_> = defs[0].bodies.iter().map(|r| &src[r.clone()]).collect();
        assert_eq!(bodies, ["x();\n\n    y();"]);
    }

    #[test]
    fn body_followed_only_by_whitespace_extends_to_definition_end() {
        // Given: 段落をボディにする Markdown の query
        let manifest =
            "[package]\nname = \"md\"\nextensions = [\"md\"]\n[package.syntax]\nmodule = \"builtin:markdown\"\n";
        let query = "(section (atx_heading heading_content: (inline) @name) (paragraph) @body) @definition.section";
        let lang = Language::load(manifest, query, None).unwrap();
        // When: 段落の後に空行が続くセクションを抽出する
        let src = "# A\n\npara\n\n\n# B\n";
        let defs = defs(&lang, src);
        // Then: ボディはセクションの末尾の空行まで広がる
        assert_eq!(&src[defs[0].bodies[0].clone()], "para\n\n\n");
    }

    #[test]
    fn definition_without_name_capture_takes_name_from_set_property() {
        // Given: 名前のノードがない定義に #set! で名前を付ける query
        let lang = rust("((impl_item) @definition.impl (#set! name \"Impl\"))");
        // When: impl を抽出する
        let defs = defs(&lang, "impl Foo {}\n");
        // Then: #set! の値が名前になる
        assert_eq!(defs[0].name, "Impl");
    }

    #[test]
    fn name_capture_wins_over_set_property() {
        // Given: 名前を捕まえるパターンと、同じノードに #set! で既定の名前を付けるパターン
        for query in [
            "(impl_item type: (type_identifier) @name) @definition.impl
             ((impl_item) @definition.impl (#set! name \"Impl\"))",
            "((impl_item) @definition.impl (#set! name \"Impl\"))
             (impl_item type: (type_identifier) @name) @definition.impl",
        ] {
            let lang = rust(query);
            // When: impl を抽出する
            let defs = defs(&lang, "impl Foo {}\n");
            // Then: パターンの順序によらず、捕まえた名前が使われる
            assert_eq!(defs[0].name, "Foo", "{query}");
        }
    }
}
