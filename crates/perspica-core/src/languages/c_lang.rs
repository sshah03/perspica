use crate::languages::LanguageSupport;
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct CSupport;

impl LanguageSupport for CSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_c::LANGUAGE.into()
    }

    fn is_exported(&self, node: &tree_sitter::Node, _name: &str, source: &str) -> bool {
        let mut c = node.walk();
        let is_static = node
            .children(&mut c)
            .any(|ch| ch.kind() == "storage_class_specifier" && &source[ch.byte_range()] == "static");
        !is_static
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
        "function_definition" => extract_function(node, source),
        "declaration" => extract_declaration(node, source),
        "preproc_include" => extract_include(node, source),
        "type_definition" => extract_typedef(node, source),
        "preproc_def" => extract_macro_def(node, source),
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
    // function_definition has a "declarator" field which is a function_declarator
    let declarator = node.child_by_field_name("declarator")?;

    let (name, params) = extract_function_declarator(&declarator, source)?;

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

/// Extract name and params from a function_declarator node.
/// The declarator may be nested inside a pointer_declarator.
fn extract_function_declarator(
    node: &tree_sitter::Node,
    source: &str,
) -> Option<(String, Vec<Param>)> {
    if node.kind() == "function_declarator" {
        let name = node
            .child_by_field_name("declarator")
            .map(|n| node_text(&n, source))?;
        let params = extract_params(node, source);
        return Some((name, params));
    }

    // pointer_declarator wraps the function_declarator (e.g., `*func(...)`)
    if node.kind() == "pointer_declarator" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "function_declarator" {
                return extract_function_declarator(&child, source);
            }
        }
    }

    None
}

fn extract_params(node: &tree_sitter::Node, source: &str) -> Vec<Param> {
    let Some(params_node) = node.child_by_field_name("parameters") else {
        return vec![];
    };

    let mut params = Vec::new();
    let mut cursor = params_node.walk();
    for child in params_node.children(&mut cursor) {
        if child.kind() == "parameter_declaration" {
            let name = child
                .child_by_field_name("declarator")
                .map(|n| node_text(&n, source))
                .unwrap_or_default();
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

fn extract_declaration(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // A top-level "declaration" can be:
    // 1. A function prototype: `int foo(int x);`
    // 2. A variable declaration: `int x = 5;`
    // 3. A struct/enum forward declaration: `struct Foo;`
    // Check children for struct_specifier or enum_specifier first.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "struct_specifier" => {
                return extract_struct_as_class(&child, source, node);
            }
            "enum_specifier" => {
                return extract_enum_as_class(&child, source, node);
            }
            _ => {}
        }
    }

    // Check if it's a function prototype (declarator is a function_declarator)
    if let Some(declarator) = node.child_by_field_name("declarator") {
        if let Some((name, params)) = extract_function_declarator(&declarator, source) {
            let return_type = node
                .child_by_field_name("type")
                .map(|n| node_text(&n, source));
            // Function prototype: no body, hash is 0
            return Some(SemanticItem::Function {
                name,
                params,
                return_type,
                body_hash: 0,
                decl_hash: 0,
                span: node_span(node),
                children: vec![],
            });
        }
    }

    // Otherwise treat as a variable declaration
    let name = node
        .child_by_field_name("declarator")
        .map(|n| {
            // The declarator might be an init_declarator wrapping the actual name
            n.child_by_field_name("name")
                .or_else(|| n.child_by_field_name("declarator"))
                .map(|inner| node_text(&inner, source))
                .unwrap_or_else(|| node_text(&n, source))
        })?;

    Some(SemanticItem::Variable {
        name,
        is_exported: false,
        span: node_span(node),
    })
}

fn extract_include(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // preproc_include has a "path" field which is either a string_literal or system_lib_string
    let path = node
        .child_by_field_name("path")
        .map(|n| {
            node_text(&n, source)
                .trim_matches(|c| c == '"' || c == '<' || c == '>')
                .to_string()
        })?;

    Some(SemanticItem::Import {
        source: path,
        symbols: vec![],
        span: node_span(node),
    })
}

fn extract_struct_as_class(
    specifier: &tree_sitter::Node,
    source: &str,
    decl_node: &tree_sitter::Node,
) -> Option<SemanticItem> {
    let name = specifier
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let mut fields = Vec::new();

    if let Some(body) = specifier.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "field_declaration" {
                let ftype = child
                    .child_by_field_name("type")
                    .map(|n| node_text(&n, source));
                let fname = child
                    .child_by_field_name("declarator")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                fields.push(Field {
                    name: fname,
                    type_annotation: ftype,
                });
            }
        }
    }

    Some(SemanticItem::Class {
        name,
        span: node_span(decl_node),
        methods: vec![],
        fields,
    })
}

fn extract_enum_as_class(
    specifier: &tree_sitter::Node,
    source: &str,
    decl_node: &tree_sitter::Node,
) -> Option<SemanticItem> {
    let name = specifier
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let mut fields = Vec::new();

    if let Some(body) = specifier.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "enumerator" {
                let fname = child
                    .child_by_field_name("name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                fields.push(Field {
                    name: fname,
                    type_annotation: None,
                });
            }
        }
    }

    Some(SemanticItem::Class {
        name,
        span: node_span(decl_node),
        methods: vec![],
        fields,
    })
}

fn extract_typedef(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // type_definition: `typedef struct { ... } Name;`
    // The "declarator" field holds the new type name.
    // Also check for an embedded struct/enum specifier.

    // First, check if there's an embedded struct_specifier or enum_specifier with a body
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "struct_specifier" => {
                if child.child_by_field_name("body").is_some() {
                    // typedef struct { ... } Name: extract as Class using the typedef name
                    let typedef_name = node
                        .child_by_field_name("declarator")
                        .map(|n| node_text(&n, source));

                    // Prefer the typedef alias name; fall back to the struct tag name
                    let name = typedef_name
                        .or_else(|| {
                            child
                                .child_by_field_name("name")
                                .map(|n| node_text(&n, source))
                        })?;

                    let mut fields = Vec::new();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut body_cursor = body.walk();
                        for fc in body.children(&mut body_cursor) {
                            if fc.kind() == "field_declaration" {
                                let ftype = fc
                                    .child_by_field_name("type")
                                    .map(|n| node_text(&n, source));
                                let fname = fc
                                    .child_by_field_name("declarator")
                                    .map(|n| node_text(&n, source))
                                    .unwrap_or_default();
                                fields.push(Field {
                                    name: fname,
                                    type_annotation: ftype,
                                });
                            }
                        }
                    }

                    return Some(SemanticItem::Class {
                        name,
                        span: node_span(node),
                        methods: vec![],
                        fields,
                    });
                }
            }
            "enum_specifier" => {
                if child.child_by_field_name("body").is_some() {
                    let typedef_name = node
                        .child_by_field_name("declarator")
                        .map(|n| node_text(&n, source));

                    let name = typedef_name
                        .or_else(|| {
                            child
                                .child_by_field_name("name")
                                .map(|n| node_text(&n, source))
                        })?;

                    let mut fields = Vec::new();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut body_cursor = body.walk();
                        for fc in body.children(&mut body_cursor) {
                            if fc.kind() == "enumerator" {
                                let fname = fc
                                    .child_by_field_name("name")
                                    .map(|n| node_text(&n, source))
                                    .unwrap_or_default();
                                fields.push(Field {
                                    name: fname,
                                    type_annotation: None,
                                });
                            }
                        }
                    }

                    return Some(SemanticItem::Class {
                        name,
                        span: node_span(node),
                        methods: vec![],
                        fields,
                    });
                }
            }
            _ => {}
        }
    }

    // Simple typedef (e.g., `typedef unsigned long size_t;`)
    let name = node
        .child_by_field_name("declarator")
        .map(|n| node_text(&n, source))?;
    Some(SemanticItem::TypeDef {
        name,
        span: node_span(node),
    })
}

fn extract_macro_def(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // preproc_def: `#define FOO 42`
    let name = node
        .child_by_field_name("name")
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
