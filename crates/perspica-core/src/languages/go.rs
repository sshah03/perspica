use crate::languages::LanguageSupport;
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct GoSupport;

impl LanguageSupport for GoSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_go::LANGUAGE.into()
    }

    fn is_exported(&self, _node: &tree_sitter::Node, name: &str, _source: &str) -> bool {
        let bare = crate::parser::bare_name(name);
        bare.chars().next().is_some_and(|c| c.is_uppercase()) || bare == "init"
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
        "function_declaration" => extract_function(node, source),
        "method_declaration" => extract_method(node, source),
        "type_declaration" => extract_type_declaration(node, source),
        "import_declaration" => extract_import(node, source),
        "var_declaration" => extract_var_declaration(node, source),
        "const_declaration" => extract_const_declaration(node, source),
        "package_clause" => {
            // Skip the package clause: boilerplate, not an item worth diffing
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
        .child_by_field_name("result")
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

/// The type in a receiver list: `(r *Box[T])` → `Box`, `(Box)` → `Box`.
fn receiver_type(recv: &str) -> String {
    let inner = recv.trim().trim_start_matches('(').trim_end_matches(')');
    let ty = inner.split_whitespace().last().unwrap_or("");
    ty.trim_start_matches('*').split('[').next().unwrap_or("").to_string()
}

fn extract_method(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    // Name methods by their receiver type: `func (r *Box[T]) Len()` → `Box.Len`.
    let receiver = node
        .child_by_field_name("receiver")
        .map(|n| receiver_type(&node_text(&n, source)))
        .filter(|t| !t.is_empty());

    let qualified_name = match receiver {
        Some(recv) => format!("{recv}.{name}"),
        None => name,
    };

    let params = extract_params(node, source);
    let return_type = node
        .child_by_field_name("result")
        .map(|n| node_text(&n, source));

    let body = node
        .child_by_field_name("body")
        .map(|n| node_text(&n, source))
        .unwrap_or_default();

    let normalized_body: String = body.split_whitespace().collect();
    let body_hash = hash_str(&normalized_body);

    Some(SemanticItem::Function {
        name: qualified_name,
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
        if child.kind() == "parameter_declaration" {
            // Go parameter declarations can have multiple names: `a, b int`
            let type_annotation = child
                .child_by_field_name("type")
                .map(|n| node_text(&n, source));

            let mut has_name = false;
            let mut inner = child.walk();
            for param_child in child.children(&mut inner) {
                if param_child.kind() == "identifier" {
                    has_name = true;
                    params.push(Param {
                        name: node_text(&param_child, source),
                        type_annotation: type_annotation.clone(),
                        optional: false,
                    });
                }
            }

            // Unnamed parameter (e.g. `func foo(int)`)
            if !has_name {
                if let Some(ref ta) = type_annotation {
                    params.push(Param {
                        name: ta.clone(),
                        type_annotation: type_annotation.clone(),
                    optional: false,
                    });
                }
            }
        }
        if child.kind() == "variadic_parameter_declaration" {
            let type_annotation = child
                .child_by_field_name("type")
                .map(|n| format!("...{}", node_text(&n, source)));
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_default();
            params.push(Param {
                name,
                type_annotation,
            optional: false,
            });
        }
    }
    params
}

fn extract_type_declaration(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // type_declaration contains one or more type_spec children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // `type T struct{…}` and the alias form `type T = U`.
        if child.kind() == "type_spec" || child.kind() == "type_alias" {
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))?;

            // Check what kind of type this is
            let type_node = child.child_by_field_name("type");
            if let Some(ref tn) = type_node {
                if tn.kind() == "struct_type" {
                    // Extract struct fields
                    let mut fields = Vec::new();
                    // The field list is an unnamed child: `struct { … }`.
                    let mut tc = tn.walk();
                    let field_list = tn.children(&mut tc).find(|c| c.kind() == "field_declaration_list");
                    if let Some(field_list) = field_list {
                        let mut fc = field_list.walk();
                        for field in field_list.children(&mut fc) {
                            if field.kind() == "field_declaration" {
                                let ftype = field
                                    .child_by_field_name("type")
                                    .map(|n| node_text(&n, source));
                                // `a, b int` declares two fields; an embedded `*Base` is named by its type.
                                let mut nc = field.walk();
                                let names: Vec<String> = field.children_by_field_name("name", &mut nc).map(|n| node_text(&n, source)).collect();
                                let names = if names.is_empty() {
                                    vec![ftype.as_deref().unwrap_or("").trim_start_matches('*').rsplit('.').next().unwrap_or("").to_string()]
                                } else { names };
                                for fname in names {
                                    fields.push(Field {
                                        name: fname,
                                        type_annotation: ftype.clone(),
                                    });
                                }
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
                if tn.kind() == "interface_type" {
                    return Some(SemanticItem::TypeDef {
                        name,
                        span: node_span(node),
                    });
                }
            }

            // Fallback: type alias or other type definition
            return Some(SemanticItem::TypeDef {
                name,
                span: node_span(node),
            });
        }
    }

    None
}

fn extract_import(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut symbols = Vec::new();
    let mut source_path = String::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_spec" => {
                // Single import: `import "fmt"`
                let path = child
                    .child_by_field_name("path")
                    .map(|n| {
                        node_text(&n, source)
                            .trim_matches('"')
                            .to_string()
                    })
                    .unwrap_or_default();
                if source_path.is_empty() {
                    source_path = path.clone();
                }
                symbols.push(path);
            }
            "import_spec_list" => {
                // Grouped imports: `import ( "fmt" \n "os" )`
                let mut inner = child.walk();
                for spec in child.children(&mut inner) {
                    if spec.kind() == "import_spec" {
                        let path = spec
                            .child_by_field_name("path")
                            .map(|n| {
                                node_text(&n, source)
                                    .trim_matches('"')
                                    .to_string()
                            })
                            .unwrap_or_default();

                        // Check for alias: `import alias "path"`
                        let alias = spec
                            .child_by_field_name("name")
                            .map(|n| node_text(&n, source));

                        let symbol = if let Some(a) = alias {
                            format!("{a} {path}")
                        } else {
                            path.clone()
                        };

                        let _ = path;
                        symbols.push(symbol);
                    }
                }
            }
            _ => {}
        }
    }

    // Grouped imports (`import ( … )`) have no single module: an empty source
    // means every symbol is its own module path.
    if symbols.len() > 1 || node.named_children(&mut node.walk()).any(|c| c.kind() == "import_spec_list") {
        source_path = String::new();
    } else {
        symbols.clear();
    }
    Some(SemanticItem::Import {
        source: source_path,
        symbols,
        span: node_span(node),
    })
}

fn extract_var_declaration(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // var_declaration can contain one or more var_spec children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "var_spec" {
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_else(|| {
                    // Multiple names: grab the first identifier
                    let mut inner = child.walk();
                    for ic in child.children(&mut inner) {
                        if ic.kind() == "identifier" {
                            return node_text(&ic, source);
                        }
                    }
                    String::new()
                });
            return Some(SemanticItem::Variable {
                name,
                is_exported: false,
                span: node_span(node),
            });
        }
    }
    None
}

fn extract_const_declaration(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "const_spec" {
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))
                .unwrap_or_else(|| {
                    let mut inner = child.walk();
                    for ic in child.children(&mut inner) {
                        if ic.kind() == "identifier" {
                            return node_text(&ic, source);
                        }
                    }
                    String::new()
                });
            return Some(SemanticItem::Variable {
                name,
                is_exported: false,
                span: node_span(node),
            });
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::receiver_type;

    #[test]
    fn receiver_types() {
        assert_eq!(receiver_type("(s *cursedRenderer)"), "cursedRenderer");
        assert_eq!(receiver_type("(r *Box[T])"), "Box");
        assert_eq!(receiver_type("(Server)"), "Server");
    }
}
