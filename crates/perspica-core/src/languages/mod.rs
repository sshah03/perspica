pub mod typescript;
pub mod python;
pub mod rust_lang;
pub mod go;
pub mod java;
pub mod c_lang;

use crate::parser::SemanticTree;
use crate::Language;

pub trait LanguageSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language;
    fn extract_semantic_tree(&self, tree: &tree_sitter::Tree, source: &str) -> SemanticTree;
    /// Whether a named top-level item is visible outside its file. Used to avoid
    /// flagging public API as dead code. Default: conservative (exported).
    fn is_exported(&self, _node: &tree_sitter::Node, _name: &str, _source: &str) -> bool {
        true
    }
}

/// Get the language support implementation for a given language.
/// Falls back to TypeScript for unknown languages (line-based fallback is TODO).
pub fn get_language_support(language: Language) -> Box<dyn LanguageSupport> {
    match language {
        Language::TypeScript => Box::new(typescript::TypeScriptSupport { tsx: false }),
        Language::Tsx => Box::new(typescript::TypeScriptSupport { tsx: true }),
        Language::Python => Box::new(python::PythonSupport),
        Language::Rust => Box::new(rust_lang::RustSupport),
        Language::Go => Box::new(go::GoSupport),
        Language::Java => Box::new(java::JavaSupport),
        Language::C => Box::new(c_lang::CSupport),
        Language::Unknown => Box::new(typescript::TypeScriptSupport { tsx: false }), // unused: Unknown is never parsed
    }
}
