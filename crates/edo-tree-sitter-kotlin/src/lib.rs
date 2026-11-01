//! The Kotlin grammar from [brokk-tree-sitter-kotlin](https://github.com/BrokkAi/tree-sitter-kotlin).

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn edo_tree_sitter_kotlin() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for this grammar.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(edo_tree_sitter_kotlin) };
