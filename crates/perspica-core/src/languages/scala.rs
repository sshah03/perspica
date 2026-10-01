use crate::languages::LanguageSupport;
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct ScalaSupport;

impl LanguageSupport for ScalaSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_scala::LANGUAGE.into()
    }

    // Everything is public unless marked otherwise.
    fn is_exported(&self, node: &tree_sitter::Node, _name: &str, source: &str) -> bool {
        !has_modifier(node, source, &["private", "protected"])
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

/// Top-level items. A braced or indented `package a.b { … }` nests its members.
fn extract_into(node: &tree_sitter::Node, source: &str, items: &mut Vec<SemanticItem>) {
    if node.kind() == "package_clause" {
        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                extract_into(&child, source, items);
            }
        }
        return;
    }
    if let Some(item) = extract_item(node, source) {
        items.push(item);
    }
}

fn extract_item(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    match node.kind() {
        "class_definition" | "object_definition" | "trait_definition" | "enum_definition" | "package_object" => extract_class(node, source),
        "function_definition" | "function_declaration" => extract_function(node, source),
        "val_definition" | "var_definition" | "val_declaration" | "var_declaration" => extract_variable(node, source),
        "type_definition" => {
            let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
            Some(SemanticItem::TypeDef { name, span: node_span(node) })
        }
        "import_declaration" => extract_import(node, source),
        "given_definition" => extract_given(node, source),
        "extension_definition" => extract_extension(node, source),
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
    Some(function_item(node, source, name))
}

fn function_item(node: &tree_sitter::Node, source: &str, name: String) -> SemanticItem {
    let return_type = node.child_by_field_name("return_type").map(|n| node_text(&n, source));
    let body = node.child_by_field_name("body").map(|n| node_text(&n, source)).unwrap_or_default();
    // Whitespace doesn't count: reformatting isn't a change.
    let normalized_body: String = body.split_whitespace().collect();
    SemanticItem::Function {
        name,
        params: extract_params(node, source),
        return_type,
        body_hash: hash_str(&normalized_body),
        decl_hash: 0,
        span: node_span(node),
        children: vec![],
    }
}

/// Every parameter list, in order: `def f(a: Int)(using b: B)` has two.
fn extract_params(node: &tree_sitter::Node, source: &str) -> Vec<Param> {
    let mut params = Vec::new();
    let mut cursor = node.walk();
    for list in node.children(&mut cursor).filter(|c| c.kind() == "parameters" || c.kind() == "class_parameters") {
        let mut inner = list.walk();
        for p in list.children(&mut inner).filter(|c| c.kind() == "parameter" || c.kind() == "class_parameter") {
            let name = p.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_else(|| node_text(&p, source));
            let type_annotation = p.child_by_field_name("type").map(|n| node_text(&n, source));
            // A default value means callers may leave it out.
            let optional = p.child_by_field_name("default_value").is_some();
            params.push(Param { name, type_annotation, optional });
        }
    }
    params
}

/// `class`, `case class`, `object`, `trait`, `enum`: members from the constructor
/// parameters and the body. Nested types count as members by name, so a new or
/// renamed ADT case shows up on its enclosing object.
fn extract_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
    let mut methods = Vec::new();
    let mut fields = Vec::new();

    if let Some(params) = node.child_by_field_name("class_parameters") {
        let mut cursor = params.walk();
        for p in params.children(&mut cursor).filter(|c| c.kind() == "class_parameter") {
            fields.push(Field {
                name: p.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default(),
                type_annotation: p.child_by_field_name("type").map(|n| node_text(&n, source)),
            });
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "function_definition" | "function_declaration" => {
                    if let Some(f) = extract_function(&child, source) { methods.push(f); }
                }
                "given_definition" => {
                    if let Some(f) = extract_given(&child, source) { methods.push(f); }
                }
                "val_definition" | "var_definition" | "val_declaration" | "var_declaration" => {
                    fields.push(Field {
                        name: child.child_by_field_name("pattern").map(|n| node_text(&n, source)).unwrap_or_default(),
                        type_annotation: child.child_by_field_name("type").map(|n| node_text(&n, source)),
                    });
                }
                "class_definition" | "object_definition" | "trait_definition" | "enum_definition" | "type_definition" => {
                    if let Some(n) = child.child_by_field_name("name") {
                        let kind = child.kind().trim_end_matches("_definition");
                        // A companion object shares its class's name: keep the two apart.
                        let name = if kind == "object" { format!("object {}", node_text(&n, source)) } else { node_text(&n, source) };
                        fields.push(Field { name, type_annotation: Some(kind.to_string()) });
                    }
                }
                "enum_case_definitions" => {
                    // `case Red, Green` and `case Circle(r: Double)`.
                    let mut cases = child.walk();
                    for c in child.named_children(&mut cases) {
                        if let Some(n) = c.child_by_field_name("name").or(Some(c)).filter(|n| n.kind() != "class_parameters") {
                            fields.push(Field { name: node_text(&n, source), type_annotation: Some("case".into()) });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Some(SemanticItem::Class { name, span: node_span(node), methods, fields })
}

fn extract_variable(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node.child_by_field_name("pattern").map(|n| node_text(&n, source))?;
    Some(SemanticItem::Variable {
        name,
        is_exported: !has_modifier(node, source, &["private", "protected"]),
        span: node_span(node),
    })
}

/// `import a.b.C`, `import a.b.{C, D => E}`, `import a.b._` / `a.b.*`.
fn extract_import(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut path: Vec<String> = Vec::new();
    let mut symbols: Vec<String> = Vec::new();
    let mut wildcard = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => path.push(node_text(&child, source)),
            "namespace_wildcard" => wildcard = true,
            "namespace_selectors" => {
                let mut sel = child.walk();
                for s in child.named_children(&mut sel) {
                    // `D => E` imports D under another name; the symbol is still D.
                    let text = node_text(&s, source);
                    let name = text.split("=>").next().unwrap_or(&text).trim().to_string();
                    if !name.is_empty() && name != "_" && name != "*" {
                        symbols.push(name);
                    }
                }
            }
            _ => {}
        }
    }
    if path.is_empty() {
        return None;
    }
    let source_module = if wildcard || !symbols.is_empty() || path.len() == 1 {
        path.join(".")
    } else {
        let last = path.pop().unwrap_or_default();
        symbols.push(last);
        path.join(".")
    };
    Some(SemanticItem::Import { source: source_module, symbols, span: node_span(node) })
}

/// `given Show[User] = …` is anonymous: name it by what it provides.
fn extract_given(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = match node.child_by_field_name("name") {
        Some(n) => node_text(&n, source),
        None => format!("given {}", node.child_by_field_name("return_type").map(|n| node_text(&n, source)).unwrap_or_default()),
    };
    Some(function_item(node, source, name))
}

/// `extension (u: User) def display = …`: a group of methods on a type.
fn extract_extension(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let params = node.child_by_field_name("parameters").map(|n| node_text(&n, source)).unwrap_or_default();
    let name = format!("extension {}", params.split_whitespace().collect::<Vec<_>>().join(" "));
    let mut methods = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        if body.kind() == "function_definition" {
            if let Some(f) = extract_function(&body, source) { methods.push(f); }
        } else {
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                if child.kind() == "function_definition" {
                    if let Some(f) = extract_function(&child, source) { methods.push(f); }
                }
            }
        }
    }
    Some(SemanticItem::Class { name, span: node_span(node), methods, fields: vec![] })
}

fn has_modifier(node: &tree_sitter::Node, source: &str, words: &[&str]) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor)
        .filter(|c| c.kind() == "modifiers")
        .any(|m| source[m.byte_range()].split_whitespace().any(|w| words.contains(&w)));
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
