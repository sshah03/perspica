pub mod typescript;
pub mod python;
pub mod rust_lang;
pub mod go;
pub mod java;
pub mod c_lang;
pub mod scala;

use crate::parser::{Param, SemanticTree};
use crate::Language;

/// A function definition found anywhere in the code. `bare` is false for methods, since
/// they're never called by their name alone.
pub struct FunctionDef {
    pub name: String,
    pub params: Vec<Param>,
    pub bare: bool,
}

pub trait LanguageSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language;
    fn extract_semantic_tree(&self, tree: &tree_sitter::Tree, source: &str) -> SemanticTree;
    /// Whether a named top-level item is visible outside its file. Used to avoid
    /// flagging public API as dead code. Default: conservative (exported).
    fn is_exported(&self, _node: &tree_sitter::Node, _name: &str, _source: &str) -> bool {
        true
    }
    /// Node kinds that can define a function, including methods. Used to find functions
    /// defined inside other functions.
    fn function_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    /// The function that `node` defines, if it defines one.
    fn function_def(&self, _node: &tree_sitter::Node, _source: &str) -> Option<FunctionDef> {
        None
    }
    /// Whether functions defined in module-level blocks, like `if ...: def f():`, belong to the whole module.
    fn module_blocks_define(&self) -> bool {
        false
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
        Language::Scala => Box::new(scala::ScalaSupport),
        Language::Unknown => Box::new(typescript::TypeScriptSupport { tsx: false }), // unused: Unknown is never parsed
    }
}
