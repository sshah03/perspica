use crate::languages::LanguageSupport;
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct PythonSupport;

impl LanguageSupport for PythonSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn is_exported(&self, _node: &tree_sitter::Node, name: &str, _source: &str) -> bool {
        // Module-level names without a leading underscore are importable.
        !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"))
    }

    fn extract_semantic_tree(&self, tree: &tree_sitter::Tree, source: &str) -> SemanticTree {
        let root = tree.root_node();
        let mut items = Vec::new();

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if let Some(item) = extract_item(&child, source) {
                items.push(item);
            }
        }

        SemanticTree::new(items)
    }
}

fn extract_item(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    match node.kind() {
        "function_definition" | "decorated_definition" => {
            if node.kind() == "decorated_definition" {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "function_definition" || child.kind() == "class_definition" {
                        return extract_item(&child, source);
                    }
                }
                return None;
            }
            extract_function(node, source)
        }
        "class_definition" => extract_class(node, source),
        "import_statement" | "import_from_statement" => extract_import(node, source),
        "expression_statement" => {
            // Top-level assignments: `x = 5`
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "assignment" => return extract_assignment(&child, source),
                    // Docstrings are documentation, not code.
                    "string" => return None,
                    _ => {}
                }
            }
            // Top-level calls and other expressions still matter (`main()`, `app.run()`).
            Some(SemanticItem::Other {
                span: node_span(node),
                content_hash: hash_str(&node_text(node, source)),
            })
        }
        _ => {
            let text = node_text(node, source);
            if text.trim().is_empty() {
                return None;
            }
            Some(SemanticItem::Other {
                span: node_span(node),
                content_hash: hash_str(&text),
            })
        }
    }
}

fn extract_function(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let params = extract_params(node, source);
    let return_type = node
        .child_by_field_name("return_type")
        .map(|n| node_text(&n, source));

    let body = node
        .child_by_field_name("body")
        .map(|n| node_text(&n, source))
        .unwrap_or_default();

    let normalized_body: String = body.split_whitespace().collect();
    let body_hash = hash_str(&normalized_body);

    Some(SemanticItem::Function {
        name,
        params,
        return_type,
        body_hash,
        decl_hash: 0,
        span: node_span(node),
        children: vec![],
    })
}

fn extract_params(node: &tree_sitter::Node, source: &str) -> Vec<Param> {
    let Some(params_node) = node.child_by_field_name("parameters") else {
        return vec![];
    };

    let mut params = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                let name = node_text(&child, source);
                if name != "self" && name != "cls" {
                    params.push(Param {
                        name,
                        type_annotation: None,
                    optional: false,
                    });
                }
            }
            "typed_parameter" => {
                let name = child
                    .child_by_field_name("name")
                    .or_else(|| child.child(0))
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                if name != "self" && name != "cls" {
                    let type_annotation = child
                        .child_by_field_name("type")
                        .map(|n| node_text(&n, source));
                    params.push(Param {
                        name,
                        type_annotation,
                    optional: false,
                    });
                }
            }
            "default_parameter" | "typed_default_parameter" => {
                let name = child
                    .child_by_field_name("name")
                    .or_else(|| child.child(0))
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                if name != "self" && name != "cls" {
                    let type_annotation = child
                        .child_by_field_name("type")
                        .map(|n| node_text(&n, source));
                    params.push(Param {
                        name,
                        type_annotation,
                    optional: true,
                    });
                }
            }
            _ => {}
        }
    }
    params
}

fn extract_import(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut source_module = String::new();
    let mut symbols = Vec::new();

    if node.kind() == "import_from_statement" {
        if let Some(module) = node.child_by_field_name("module_name") {
            source_module = node_text(&module, source);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "dotted_name" && source_module.is_empty() {
                source_module = node_text(&child, source);
            }
            if (child.kind() == "aliased_import" || child.kind() == "dotted_name")
                && child != node.child_by_field_name("module_name").unwrap_or(child) {
                    symbols.push(node_text(&child, source));
                }
        }
    } else {
        // import_statement: `import os`
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "dotted_name" {
                source_module = node_text(&child, source);
            }
        }
    }

    Some(SemanticItem::Import {
        source: source_module,
        symbols,
        span: node_span(node),
    })
}

fn extract_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let mut methods = Vec::new();
    let mut fields = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "function_definition" => {
                    if let Some(item) = extract_function(&child, source) {
                        methods.push(item);
                    }
                }
                "expression_statement" => {
                    // Class-level assignments as fields
                    let mut inner = child.walk();
                    for ic in child.children(&mut inner) {
                        if ic.kind() == "assignment" {
                            if let Some(name_node) = ic.child_by_field_name("left") {
                                fields.push(Field {
                                    name: node_text(&name_node, source),
                                    type_annotation: None,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Some(SemanticItem::Class {
        name,
        span: node_span(node),
        methods,
        fields,
    })
}

fn extract_assignment(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("left")
        .map(|n| node_text(&n, source))?;
    Some(SemanticItem::Variable {
        name,
        is_exported: false,
        span: node_span(node),
    })
}

fn node_text(node: &tree_sitter::Node, source: &str) -> String {
    source[node.byte_range()].to_string()
}

fn node_span(node: &tree_sitter::Node) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span {
        start_line: start.row + 1,
        start_col: start.column,
        end_line: end.row + 1,
        end_col: end.column,
    }
}
