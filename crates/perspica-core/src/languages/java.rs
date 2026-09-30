use crate::languages::LanguageSupport;
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct JavaSupport;

impl LanguageSupport for JavaSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_java::LANGUAGE.into()
    }

    fn is_exported(&self, node: &tree_sitter::Node, _name: &str, source: &str) -> bool {
        let mut c = node.walk();
        let private = node
            .children(&mut c)
            .any(|ch| ch.kind() == "modifiers" && source[ch.byte_range()].contains("private"));
        !private
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
        "class_declaration" => extract_class(node, source),
        "interface_declaration" => extract_typedef(node, source),
        "enum_declaration" => extract_enum_as_class(node, source),
        "record_declaration" => extract_record_as_class(node, source),
        "import_declaration" => extract_import(node, source),
        "method_declaration" => extract_function(node, source),
        "package_declaration" => {
            // Skip package declarations: they don't change the structure
            None
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
        .child_by_field_name("type")
        .map(|n| node_text(&n, source));

    let body = node
        .child_by_field_name("body")
        .map(|n| node_text(&n, source))
        .unwrap_or_default();

    // Strip whitespace for body hash so formatting changes don't affect it
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
        if child.kind() == "formal_parameter" || child.kind() == "spread_parameter" {
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_else(|| node_text(&child, source));
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
    params
}

fn extract_import(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // import_declaration contains a scoped_identifier like "java.util.List"
    // We parse the full path as the source and the last segment as the symbol.
    let full_text = node_text(node, source);
    // Strip "import " prefix and trailing ";"
    let path = full_text
        .trim()
        .trim_start_matches("import")
        .trim()
        .trim_start_matches("static")
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();

    let mut source_module = path.clone();
    let mut symbols = Vec::new();

    // Split on last '.' to get module path and symbol name
    if let Some(dot_pos) = path.rfind('.') {
        source_module = path[..dot_pos].to_string();
        let symbol = path[dot_pos + 1..].to_string();
        if symbol != "*" {
            symbols.push(symbol);
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
                "method_declaration" | "constructor_declaration" => {
                    if let Some(item) = extract_function(&child, source) {
                        methods.push(item);
                    }
                }
                "field_declaration" => {
                    extract_fields_from_declaration(&child, source, &mut fields);
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

fn extract_enum_as_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let mut methods = Vec::new();
    let mut fields = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "method_declaration" | "constructor_declaration" => {
                    if let Some(item) = extract_function(&child, source) {
                        methods.push(item);
                    }
                }
                "enum_constant" => {
                    let fname = child
                        .child_by_field_name("name")
                        .map(|n| node_text(&n, source))
                        .unwrap_or_default();
                    fields.push(Field {
                        name: fname,
                        type_annotation: None,
                    });
                }
                "field_declaration" => {
                    extract_fields_from_declaration(&child, source, &mut fields);
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

fn extract_record_as_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let mut methods = Vec::new();
    let mut fields = Vec::new();

    // Record parameters are in the "parameters" field
    if let Some(params_node) = node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.children(&mut cursor) {
            if child.kind() == "formal_parameter" {
                let fname = child
                    .child_by_field_name("name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                let ftype = child
                    .child_by_field_name("type")
                    .map(|n| node_text(&n, source));
                fields.push(Field {
                    name: fname,
                    type_annotation: ftype,
                });
            }
        }
    }

    // Record body may have methods
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "method_declaration" || child.kind() == "constructor_declaration" {
                if let Some(item) = extract_function(&child, source) {
                    methods.push(item);
                }
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

fn extract_fields_from_declaration(
    node: &tree_sitter::Node,
    source: &str,
    fields: &mut Vec<Field>,
) {
    let ftype = node
        .child_by_field_name("type")
        .map(|n| node_text(&n, source));

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let fname = child
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_default();
            fields.push(Field {
                name: fname,
                type_annotation: ftype.clone(),
            });
        }
    }
}

fn extract_typedef(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;
    Some(SemanticItem::TypeDef {
        name,
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
