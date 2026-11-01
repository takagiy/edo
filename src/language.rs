use std::path::Path;
use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub package: Option<PackageManifest>,
    pub registry: Option<RegistryManifest>,
}

#[derive(Debug, Deserialize)]
pub struct PackageManifest {
    pub name: String,
    pub extensions: Vec<String>,
    pub grammar: String,
    pub grammar_symbol: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RegistryManifest {
    pub packages: Vec<String>,
}

pub struct Language {
    pub name: String,
    pub extensions: Vec<String>,
    pub grammar: tree_sitter::Language,
    pub query: tree_sitter::Query,
}

pub struct Languages {
    list: Vec<Language>,
}

macro_rules! builtin {
    ($name:literal) => {
        (
            include_str!(concat!("../languages/", $name, "/edo-package.toml")),
            include_str!(concat!("../languages/", $name, "/edo.scm")),
        )
    };
}

const BUILTINS: [(&str, &str); 9] = [
    builtin!("rust"),
    builtin!("typescript"),
    builtin!("tsx"),
    builtin!("javascript"),
    builtin!("python"),
    builtin!("go"),
    builtin!("java"),
    builtin!("kotlin"),
    builtin!("markdown"),
];

fn builtin_grammar(name: &str) -> Option<tree_sitter::Language> {
    Some(match name {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "kotlin" => edo_tree_sitter_kotlin::LANGUAGE.into(),
        "markdown" => tree_sitter_md::LANGUAGE.into(),
        _ => return None,
    })
}

impl Language {
    pub fn load_dir(dir: &Path) -> Result<Language, String> {
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()));
        Language::load(&read("edo-package.toml")?, &read("edo.scm")?, Some(dir))
    }

    pub fn load(manifest: &str, query: &str, base: Option<&Path>) -> Result<Language, String> {
        let m: Manifest = toml::from_str(manifest).map_err(|e| e.to_string())?;
        let p = m.package.ok_or("missing [package]")?;
        let grammar = match p.grammar.strip_prefix("builtin:") {
            Some(name) => builtin_grammar(name).ok_or(format!("unknown builtin grammar: {name}"))?,
            None => {
                let path = base.ok_or("wasm grammar needs a package directory")?.join(&p.grammar);
                let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let symbol = p.grammar_symbol.as_deref().unwrap_or(&p.name);
                let mut store = tree_sitter::WasmStore::new(engine()).map_err(|e| e.to_string())?;
                store.load_language(symbol, &bytes).map_err(|e| e.to_string())?
            }
        };
        let query = tree_sitter::Query::new(&grammar, query).map_err(|e| format!("edo.scm: {e}"))?;
        Ok(Language { name: p.name, extensions: p.extensions, grammar, query })
    }
}

fn engine() -> &'static tree_sitter::wasmtime::Engine {
    static ENGINE: OnceLock<tree_sitter::wasmtime::Engine> = OnceLock::new();
    ENGINE.get_or_init(Default::default)
}

impl Language {
    pub fn parser(&self) -> tree_sitter::Parser {
        let mut parser = tree_sitter::Parser::new();
        if self.grammar.is_wasm() {
            let store = tree_sitter::WasmStore::new(engine()).expect("wasm store");
            parser.set_wasm_store(store).expect("wasm store");
        }
        parser.set_language(&self.grammar).expect("grammar version");
        parser
    }
}

impl Languages {
    pub fn builtin() -> Self {
        Self::with_installed(vec![])
    }

    /// Installed packages come first so they win over builtins on the same extension.
    pub fn with_installed(installed: Vec<Language>) -> Self {
        let builtins = BUILTINS.iter().map(|(m, q)| Language::load(m, q, None).expect("builtin package"));
        Languages { list: installed.into_iter().chain(builtins).collect() }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Language> {
        self.list.iter()
    }

    pub fn for_path(&self, path: &Path) -> Option<&Language> {
        let ext = path.extension()?.to_str()?;
        self.list.iter().find(|l| l.extensions.iter().any(|e| e == ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_language_is_chosen_by_declared_extension() {
        // Given: 組み込みの言語
        let langs = Languages::builtin();
        for (file, name) in [
            ("a.rs", "rust"),
            ("a.ts", "typescript"),
            ("a.tsx", "tsx"),
            ("a.js", "javascript"),
            ("a.mjs", "javascript"),
            ("a.jsx", "javascript"),
            ("a.py", "python"),
            ("a.go", "go"),
            ("a.java", "java"),
            ("a.kt", "kotlin"),
            ("a.kts", "kotlin"),
            ("a.md", "markdown"),
        ] {
            // When: 拡張子から言語を選ぶ
            let lang = langs.for_path(Path::new(file));
            // Then: 宣言した言語が選ばれる
            assert_eq!(lang.map(|l| l.name.as_str()), Some(name), "{file}");
        }
    }

    #[test]
    fn no_language_is_chosen_for_undeclared_extension() {
        // Given: 組み込みの言語
        let langs = Languages::builtin();
        // When: どの言語も宣言していない拡張子で選ぶ
        let lang = langs.for_path(Path::new("a.unknown"));
        // Then: 言語は選ばれない
        assert!(lang.is_none());
    }

    #[test]
    fn manifest_gives_name_and_extensions() {
        // Given: パッケージのマニフェスト
        let manifest = "[package]
name = \"mylang\"
extensions = [\"my\", \"myl\"]
grammar = \"builtin:rust\"
";
        // When: 読み込む
        let lang = Language::load(manifest, "", None).unwrap();
        // Then: 名前と拡張子が読める
        assert_eq!(lang.name, "mylang");
        assert_eq!(lang.extensions, ["my", "myl"]);
    }

    #[test]
    fn builtin_grammar_reference_points_to_builtin_grammar() {
        // Given: grammar に builtin:rust を書いたパッケージ
        let manifest = "[package]
name = \"r\"
extensions = [\"r\"]
grammar = \"builtin:rust\"
";
        let lang = Language::load(manifest, "", None).unwrap();
        // When: Rust のコードをパースする
        let tree = lang.parser().parse("fn a() {}", None).unwrap();
        // Then: Rust の grammar の構文木になる
        assert_eq!(tree.root_node().to_sexp(), "(source_file (function_item name: (identifier) parameters: (parameters) body: (block)))");
    }

    #[test]
    fn grammar_file_loads_wasm_grammar() {
        // Given: grammar に grammar.wasm を書いたパッケージ
        let lang = Language::load_dir(Path::new("tests/fixtures/packages/json")).unwrap();
        // When: JSON をパースする
        let tree = lang.parser().parse("[1]", None).unwrap();
        // Then: WASM の grammar の構文木になる
        assert_eq!(tree.root_node().to_sexp(), "(document (array (number)))");
    }

    #[test]
    fn grammar_symbol_selects_language_inside_wasm() {
        // Given: 名前と違う grammar_symbol を書いたパッケージ
        let dir = Path::new("tests/fixtures/packages/myjson");
        // When: 読み込む
        let lang = Language::load_dir(dir).unwrap();
        // Then: WASM の中の grammar_symbol の言語でパースできる
        let tree = lang.parser().parse("{}", None).unwrap();
        assert_eq!(tree.root_node().to_sexp(), "(document (object))");
    }

    #[test]
    fn installed_package_wins_over_builtin_with_same_extension() {
        // Given: 組み込みと同じ拡張子 .rs を宣言したインストール済みのパッケージ
        let manifest = "[package]
name = \"myrust\"
extensions = [\"rs\"]
grammar = \"builtin:rust\"
";
        let langs = Languages::with_installed(vec![Language::load(manifest, "", None).unwrap()]);
        // When: .rs のファイルの言語を選ぶ
        let lang = langs.for_path(Path::new("a.rs")).unwrap();
        // Then: インストール済みが選ばれる
        assert_eq!(lang.name, "myrust");
    }

    #[test]
    fn malformed_query_fails_to_load_package() {
        // Given: 文法の誤った query
        let manifest = "[package]
name = \"r\"
extensions = [\"r\"]
grammar = \"builtin:rust\"
";
        // When: 読み込む
        let result = Language::load(manifest, "(function_item", None);
        // Then: 読み込みエラーになる
        assert!(result.is_err());
    }

    #[test]
    fn builtin_kotlin_parses_annotated_data_object() {
        // Given: 組み込みの Kotlin
        let langs = Languages::builtin();
        let lang = langs.for_path(Path::new("a.kt")).unwrap();
        // When: アノテーション付きの data object をパースする
        let tree = lang.parser().parse("@Suppress(\"x\")\ndata object Foo {}\n", None).unwrap();
        // Then: エラーなしで object の宣言になる
        let root = tree.root_node();
        assert!(!root.has_error(), "{}", root.to_sexp());
        assert_eq!(root.child(0).unwrap().kind(), "object_declaration");
    }
}
