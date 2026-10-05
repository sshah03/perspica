use crate::languages::{FunctionDef, LanguageSupport};
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct CSharpSupport;

impl LanguageSupport for CSharpSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_c_sharp::LANGUAGE.into()
    }

    // Top-level types are internal by default, which other files can still use.
    fn is_exported(&self, node: &tree_sitter::Node, _name: &str, source: &str) -> bool {
        !has_modifier(node, source, &["private", "file"])
    }

    fn function_kinds(&self) -> &'static [&'static str] {
        &["method_declaration", "constructor_declaration", "local_function_statement"]
    }

    fn function_def(&self, node: &tree_sitter::Node, source: &str) -> Option<FunctionDef> {
        let SemanticItem::Function { name, params, .. } = extract_function(node, source)? else { return None };
        Some(FunctionDef { name, params, bare: node.kind() == "local_function_statement" })
    }

    fn extract_semantic_tree(&self, tree: &tree_sitter::Tree, source: &str) -> SemanticTree {
        let mut items = Vec::new();
        let root = tree.root_node();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            extract_into(&child, source, &mut items);
        }
        SemanticTree::new(items)
    }
}

/// A block namespace nests its members, so take them out of it. A file-scoped
/// `namespace A.B;` comes before its members, which are already top-level.
fn extract_into(node: &tree_sitter::Node, source: &str, items: &mut Vec<SemanticItem>) {
    match node.kind() {
        "namespace_declaration" => {
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    extract_into(&child, source, items);
                }
            }
        }
        "file_scoped_namespace_declaration" | "{" | "}" | ";" => {}
        _ => {
            if let Some(item) = extract_item(node, source) {
                items.push(item);
            }
        }
    }
}

fn extract_item(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    match node.kind() {
        "class_declaration" | "struct_declaration" | "record_declaration" | "interface_declaration" | "enum_declaration" => extract_class(node, source),
        "delegate_declaration" => {
            let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
            Some(SemanticItem::TypeDef { name, span: node_span(node) })
        }
        "using_directive" => extract_using(node, source),
        _ => {
            let text = node_text(node, source);
            if text.trim().is_empty() {
                return None;
            }
            Some(SemanticItem::Other { span: node_span(node), content_hash: hash_str(&text) })
        }
    }
}

/// Methods, constructors, operators and local functions. Properties and indexers too, since
/// their accessors are code.
fn extract_function(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = match node.kind() {
        "operator_declaration" => format!("operator {}", node.child_by_field_name("operator").map(|n| node_text(&n, source)).unwrap_or_default()),
        "conversion_operator_declaration" => format!("operator {}", node.child_by_field_name("type").map(|n| node_text(&n, source)).unwrap_or_default()),
        "indexer_declaration" => "this[]".to_string(),
        "destructor_declaration" => format!("~{}", node.child_by_field_name("name").map(|n| node_text(&n, source))?),
        _ => node.child_by_field_name("name").map(|n| node_text(&n, source))?,
    };
    let return_type = node.child_by_field_name("returns").or_else(|| node.child_by_field_name("type")).map(|n| node_text(&n, source));
    let body = node.child_by_field_name("body")
        .or_else(|| node.child_by_field_name("accessors"))
        .or_else(|| node.child_by_field_name("value"))
        .map(|n| node_text(&n, source))
        .unwrap_or_default();
    // Whitespace doesn't count, so reformatting isn't a change.
    let normalized_body: String = body.split_whitespace().collect();
    Some(SemanticItem::Function {
        name,
        params: node.child_by_field_name("parameters").map(|p| extract_params(&p, source)).unwrap_or_default(),
        return_type,
        body_hash: hash_str(&normalized_body),
        decl_hash: 0,
        span: node_span(node),
        children: vec![],
    })
}

/// Parameters as callers see them. `ref`, `out` and `in` are part of the type since callers
/// write them too. The `this` of an extension method is the receiver, not an argument.
fn extract_params(list: &tree_sitter::Node, source: &str) -> Vec<Param> {
    let mut params = Vec::new();
    let mut cursor = list.walk();
    let children: Vec<_> = list.children(&mut cursor).collect();
    for (i, p) in children.iter().enumerate() {
        match p.kind() {
            "parameter" => {
                let mut c = p.walk();
                let mods: Vec<String> = p.children(&mut c).filter(|m| m.kind() == "modifier").map(|m| node_text(&m, source)).collect();
                if mods.iter().any(|m| m == "this") { continue; }
                let name = p.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_else(|| node_text(p, source));
                let ty = p.child_by_field_name("type").map(|n| node_text(&n, source));
                let type_annotation = match (mods.is_empty(), ty) {
                    (true, ty) => ty,
                    (false, Some(ty)) => Some(format!("{} {ty}", mods.join(" "))),
                    (false, None) => Some(mods.join(" ")),
                };
                // Anything after the name is the default value.
                let name_end = p.child_by_field_name("name").map(|n| n.end_byte()).unwrap_or(p.end_byte());
                let mut c = p.walk();
                let optional = p.named_children(&mut c).any(|ch| ch.start_byte() >= name_end);
                params.push(Param { name, type_annotation, optional });
            }
            // `params string[] rest` isn't wrapped in a parameter node.
            "identifier" if list.field_name_for_child(i as u32) == Some("name") => {
                let ty = children[..i].iter().rev().find(|t| t.is_named()).map(|t| node_text(t, source)).unwrap_or_default();
                params.push(Param { name: node_text(p, source), type_annotation: Some(format!("params {ty}")), optional: false });
            }
            _ => {}
        }
    }
    params
}

/// `class`, `struct`, `record`, `interface` and `enum`. Members come from the record's
/// parameters and the body. Nested types count as members by name.
fn extract_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
    let mut methods = Vec::new();
    let mut fields = Vec::new();

    let mut cursor = node.walk();
    if let Some(params) = node.children(&mut cursor).find(|c| c.kind() == "parameter_list") {
        for p in extract_params(&params, source) {
            fields.push(Field { name: p.name, type_annotation: p.type_annotation });
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" | "constructor_declaration" | "destructor_declaration" | "operator_declaration"
                | "conversion_operator_declaration" | "property_declaration" | "indexer_declaration" => {
                    if let Some(f) = extract_function(&child, source) { methods.push(f); }
                }
                "field_declaration" | "event_field_declaration" => {
                    let mut c = child.walk();
                    for decl in child.named_children(&mut c).filter(|d| d.kind() == "variable_declaration") {
                        let ty = decl.child_by_field_name("type").map(|n| node_text(&n, source));
                        let mut v = decl.walk();
                        for var in decl.named_children(&mut v).filter(|d| d.kind() == "variable_declarator") {
                            let name = var.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default();
                            fields.push(Field { name, type_annotation: ty.clone() });
                        }
                    }
                }
                "event_declaration" => {
                    fields.push(Field {
                        name: child.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default(),
                        type_annotation: child.child_by_field_name("type").map(|n| node_text(&n, source)),
                    });
                }
                "enum_member_declaration" => {
                    let name = child.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default();
                    fields.push(Field { name, type_annotation: None });
                }
                "class_declaration" | "struct_declaration" | "record_declaration" | "interface_declaration" | "enum_declaration" | "delegate_declaration" => {
                    if let Some(n) = child.child_by_field_name("name") {
                        let kind = child.kind().trim_end_matches("_declaration");
                        fields.push(Field { name: node_text(&n, source), type_annotation: Some(kind.to_string()) });
                    }
                }
                _ => {}
            }
        }
    }

    Some(SemanticItem::Class { name, span: node_span(node), methods, fields })
}

/// `using A.B;` brings in a namespace. `using X = A.B;` names one type, and
/// `using static A.B;` brings in its members.
fn extract_using(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let alias = node.child_by_field_name("name").map(|n| node_text(&n, source));
    let mut cursor = node.walk();
    let target = node.named_children(&mut cursor)
        .filter(|c| !is_name_field(node, c))
        .find(|c| matches!(c.kind(), "qualified_name" | "identifier" | "generic_name" | "alias_qualified_name"))
        .map(|n| node_text(&n, source))?;
    Some(SemanticItem::Import { source: target, symbols: alias.into_iter().collect(), span: node_span(node) })
}

fn is_name_field(parent: &tree_sitter::Node, child: &tree_sitter::Node) -> bool {
    parent.child_by_field_name("name").is_some_and(|n| n.id() == child.id())
}

fn has_modifier(node: &tree_sitter::Node, source: &str, words: &[&str]) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor)
        .filter(|c| c.kind() == "modifier")
        .any(|m| words.contains(&&source[m.byte_range()]));
    found
}

fn node_text(node: &tree_sitter::Node, source: &str) -> String {
    source[node.byte_range()].to_string()
}

fn node_span(node: &tree_sitter::Node) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span { start_line: start.row + 1, start_col: start.column, end_line: end.row + 1, end_col: end.column }
}
