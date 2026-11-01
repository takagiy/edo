//! Compiles the brokk Kotlin grammar from the `brokk/` submodule.
//! brokk-tree-sitter-kotlin itself pins tree-sitter-language =0.1.7, which current tree-sitter
//! rejects, so only its C sources are used here.

fn main() {
    let src_dir = std::path::Path::new("brokk/src");
    let parser = src_dir.join("parser.c");
    if !parser.exists() {
        panic!("{} is missing; run `git submodule update --init`", parser.display());
    }
    let mut c = cc::Build::new();
    c.std("c11").include(src_dir).warnings(false);
    // Rename the exported symbols so they cannot clash with another Kotlin grammar in the same binary.
    for name in [
        "tree_sitter_kotlin",
        "tree_sitter_kotlin_external_scanner_create",
        "tree_sitter_kotlin_external_scanner_destroy",
        "tree_sitter_kotlin_external_scanner_scan",
        "tree_sitter_kotlin_external_scanner_serialize",
        "tree_sitter_kotlin_external_scanner_deserialize",
    ] {
        c.define(name, format!("edo_{name}").as_str());
    }
    #[cfg(target_env = "msvc")]
    c.flag("-utf-8");
    for file in ["parser.c", "scanner.c"] {
        let path = src_dir.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        c.file(path);
    }
    c.compile("edo-tree-sitter-kotlin");
}
