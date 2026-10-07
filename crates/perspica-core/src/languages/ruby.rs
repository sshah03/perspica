use crate::languages::{FunctionDef, LanguageSupport};
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct RubySupport;

impl LanguageSupport for RubySupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_ruby::LANGUAGE.into()
    }

    // Classes, modules and top-level methods are visible to any file that loads this one.
    fn is_exported(&self, _node: &tree_sitter::Node, _name: &str, _source: &str) -> bool {
        true
    }

    fn function_kinds(&self) -> &'static [&'static str] {
        &["method", "singleton_method"]
    }

    // Ruby has no local functions. A `def` inside a class or module is a method.
    fn function_def(&self, node: &tree_sitter::Node, source: &str) -> Option<FunctionDef> {
        let SemanticItem::Function { name, params, .. } = extract_function(node, source)? else { return None };
        let top_level = node.parent().is_some_and(|p| p.kind() == "program");
        Some(FunctionDef { name, params, bare: top_level })
    }

    fn extract_semantic_tree(&self, tree: &tree_sitter::Tree, source: &str) -> SemanticTree {
        let mut items = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            extract_into(&child, source, &mut items);
        }
        SemanticTree::new(items)
    }
}

/// A module that holds classes or modules is a namespace, so take its members out of it. Its
/// own methods keep the module's name, like `Faker.config`. A module without any is an item
/// like a class, such as a mixin.
fn extract_into(node: &tree_sitter::Node, source: &str, items: &mut Vec<SemanticItem>) {
    if node.kind() == "module" && is_namespace(node) {
        let module = node.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default();
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                match child.kind() {
                    "method" | "singleton_method" => {
                        if let Some(SemanticItem::Function { name, params, return_type, body_hash, decl_hash, span, children }) = extract_function(&child, source) {
                            items.push(SemanticItem::Function { name: format!("{module}.{name}"), params, return_type, body_hash, decl_hash, span, children });
                        }
                    }
                    _ => extract_into(&child, source, items),
                }
            }
        }
        return;
    }
    if let Some(item) = extract_item(node, source) {
        items.push(item);
    }
}

fn is_namespace(module: &tree_sitter::Node) -> bool {
    let Some(body) = module.child_by_field_name("body") else { return false };
    let mut cursor = body.walk();
    let found = body.named_children(&mut cursor).any(|c| matches!(c.kind(), "class" | "module"));
    found
}

fn extract_item(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    match node.kind() {
        "class" | "module" => extract_class(node, source),
        "method" | "singleton_method" => extract_function(node, source),
        "assignment" if node.child_by_field_name("left").is_some_and(|l| l.kind() == "constant") => {
            let name = node.child_by_field_name("left").map(|n| node_text(&n, source))?;
            Some(SemanticItem::Variable { name, is_exported: true, span: node_span(node) })
        }
        "call" if required_file(node, source).is_some() => {
            Some(SemanticItem::Import { bindings: vec![], source: required_file(node, source)?, symbols: vec![], span: node_span(node) })
        }
        _ => {
            let text = node_text(node, source);
            if text.trim().is_empty() {
                return None;
            }
            Some(SemanticItem::Other { span: node_span(node), content_hash: hash_str(&text) })
        }
    }
}

fn extract_function(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
    let body = node.child_by_field_name("body").map(|n| node_text(&n, source)).unwrap_or_default();
    // Whitespace doesn't count, so reformatting isn't a change.
    let normalized_body: String = body.split_whitespace().collect();
    Some(SemanticItem::Function {
        name,
        params: node.child_by_field_name("parameters").map(|p| extract_params(&p, source)).unwrap_or_default(),
        return_type: None,
        body_hash: hash_str(&normalized_body),
        decl_hash: 0,
        span: node_span(node),
        children: vec![],
    })
}

/// Parameters as callers see them. `*rest` and `**opts` take what's left over, and a keyword
/// parameter is passed by name. A `&block` is passed as a block, not in the parentheses.
fn extract_params(list: &tree_sitter::Node, source: &str) -> Vec<Param> {
    let mut params = Vec::new();
    let mut cursor = list.walk();
    for p in list.named_children(&mut cursor) {
        let name = |prefix: &str| format!("{prefix}{}", p.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default());
        let param = match p.kind() {
            "identifier" => Param { name: node_text(&p, source), type_annotation: None, optional: false },
            "optional_parameter" => Param { name: name(""), type_annotation: None, optional: true },
            "keyword_parameter" => Param { name: name(""), type_annotation: Some("keyword".into()), optional: p.child_by_field_name("value").is_some() },
            "splat_parameter" => Param { name: name("*"), type_annotation: None, optional: false },
            "hash_splat_parameter" => Param { name: name("**"), type_annotation: None, optional: false },
            "forward_parameter" => Param { name: "...".into(), type_annotation: None, optional: false },
            "destructured_parameter" => Param { name: node_text(&p, source), type_annotation: None, optional: false },
            _ => continue,
        };
        params.push(param);
    }
    params
}

/// Classes and modules. Members are methods (`def`, `def self.`, and those in `class << self`),
/// constants, `attr_*` accessors and nested classes and modules by name.
fn extract_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // `class Api::BaseController` is BaseController in the Api namespace, like a nested one.
    let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
    let name = name.rsplit("::").next().unwrap_or(&name).to_string();
    let mut methods = Vec::new();
    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        class_members(&body, source, &mut methods, &mut fields);
    }
    Some(SemanticItem::Class { name, span: node_span(node), methods, fields })
}

fn class_members(body: &tree_sitter::Node, source: &str, methods: &mut Vec<SemanticItem>, fields: &mut Vec<Field>) {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "method" | "singleton_method" => {
                if let Some(f) = extract_function(&child, source) { methods.push(f); }
            }
            "singleton_class" => {
                if let Some(inner) = child.child_by_field_name("body") { class_members(&inner, source, methods, fields); }
            }
            "assignment" => {
                if let Some(left) = child.child_by_field_name("left").filter(|l| l.kind() == "constant") {
                    fields.push(Field { name: node_text(&left, source), type_annotation: Some("const".into()) });
                }
            }
            "call" => {
                let method = child.child_by_field_name("method").map(|m| node_text(&m, source)).unwrap_or_default();
                if matches!(method.as_str(), "attr_reader" | "attr_writer" | "attr_accessor") {
                    if let Some(args) = child.child_by_field_name("arguments") {
                        let mut ac = args.walk();
                        for a in args.named_children(&mut ac).filter(|a| a.kind() == "simple_symbol") {
                            fields.push(Field { name: node_text(&a, source).trim_start_matches(':').to_string(), type_annotation: Some(method.clone()) });
                        }
                    }
                }
            }
            "class" | "module" => {
                if let Some(n) = child.child_by_field_name("name") {
                    fields.push(Field { name: node_text(&n, source), type_annotation: Some(child.kind().to_string()) });
                }
            }
            _ => {}
        }
    }
}

/// `require "json"` and `require_relative "support/helper"`, the file they load.
fn required_file(node: &tree_sitter::Node, source: &str) -> Option<String> {
    if node.child_by_field_name("receiver").is_some() { return None; }
    let method = node.child_by_field_name("method").map(|m| node_text(&m, source))?;
    if !matches!(method.as_str(), "require" | "require_relative" | "load") { return None; }
    let args = node.child_by_field_name("arguments")?;
    let mut c = args.walk();
    let first = args.named_children(&mut c).next()?;
    if first.kind() != "string" { return None; }
    Some(node_text(&first, source).trim_matches(|c| c == '"' || c == '\'').to_string())
}

fn node_text(node: &tree_sitter::Node, source: &str) -> String {
    source[node.byte_range()].to_string()
}

fn node_span(node: &tree_sitter::Node) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span { start_line: start.row + 1, start_col: start.column, end_line: end.row + 1, end_col: end.column }
}
