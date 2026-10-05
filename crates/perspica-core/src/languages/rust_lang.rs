use crate::languages::{FunctionDef, LanguageSupport};
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct RustSupport;

impl LanguageSupport for RustSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
    }

    fn is_exported(&self, node: &tree_sitter::Node, _name: &str, _source: &str) -> bool {
        let has_child = |n: &tree_sitter::Node, kind: &str| {
            let mut c = n.walk();
            let found = n.children(&mut c).any(|ch| ch.kind() == kind);
            found
        };
        if has_child(node, "visibility_modifier") {
            return true;
        }
        // Attributes (#[test], #[tokio::main], #[derive], proc-macro registration …)
        // mean the item is reached by something other than a plain call.
        if node.prev_named_sibling().is_some_and(|s| s.kind() == "attribute_item") {
            return true;
        }
        // Trait impl methods are called through the trait, never by bare name.
        if let Some(impl_node) = node.parent().and_then(|p| p.parent()) {
            if impl_node.kind() == "impl_item" && impl_node.child_by_field_name("trait").is_some() {
                return true;
            }
        }
        false
    }

    fn function_kinds(&self) -> &'static [&'static str] {
        &["function_item"]
    }

    fn function_def(&self, node: &tree_sitter::Node, source: &str) -> Option<FunctionDef> {
        let SemanticItem::Function { name, params, .. } = extract_function(node, source)? else { return None };
        // A function in an `impl` or `trait` block is a method.
        let method = node.parent().and_then(|l| l.parent()).is_some_and(|p| matches!(p.kind(), "impl_item" | "trait_item"));
        Some(FunctionDef { name, params, bare: !method })
    }

    fn extract_semantic_tree(&self, tree: &tree_sitter::Tree, source: &str) -> SemanticTree {
        let root = tree.root_node();
        let mut items = Vec::new();

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            let mut extracted = extract_item(&child, source);
            items.append(&mut extracted);
        }

        SemanticTree::new(items)
    }
}

/// Extract one or more SemanticItems from a top-level node.
/// `impl_item` can yield multiple Function items, so we return a Vec.
fn extract_item(node: &tree_sitter::Node, source: &str) -> Vec<SemanticItem> {
    match node.kind() {
        "function_item" => extract_function(node, source).into_iter().collect(),
        "struct_item" => extract_struct(node, source).into_iter().collect(),
        "enum_item" => extract_enum(node, source).into_iter().collect(),
        "impl_item" => extract_impl(node, source),
        "use_declaration" => extract_use(node, source).into_iter().collect(),
        "trait_item" => extract_trait(node, source).into_iter().collect(),
        "type_item" => extract_type_alias(node, source).into_iter().collect(),
        "const_item" | "static_item" => extract_variable(node, source).into_iter().collect(),
        "mod_item" => extract_mod(node, source),
        // Attributes belong to the following item; its fingerprint covers them.
        "attribute_item" | "inner_attribute_item" => vec![],
        _ => {
            let text = node_text(node, source);
            if text.trim().is_empty() {
                return vec![];
            }
            vec![SemanticItem::Other {
                span: node_span(node),
                content_hash: hash_str(&text),
            }]
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
            "parameter" => {
                let name = child
                    .child_by_field_name("pattern")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_else(|| node_text(&child, source));
                let type_annotation = child
                    .child_by_field_name("type")
                    .map(|n| node_text(&n, source));
                // Skip `self` parameters
                if name == "self" || name == "&self" || name == "&mut self" || name == "mut self" {
                    continue;
                }
                params.push(Param {
                    name,
                    type_annotation,
                optional: false,
                });
            }
            "self_parameter" => {
                // Skip self parameter
            }
            _ => {}
        }
    }
    params
}

fn extract_struct(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let mut fields = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "field_declaration" {
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

    Some(SemanticItem::Class {
        name,
        span: node_span(node),
        methods: vec![],
        fields,
    })
}

fn extract_enum(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;

    let mut fields = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "enum_variant" {
                let vname = child
                    .child_by_field_name("name")
                    .map(|n| node_text(&n, source))
                    .unwrap_or_default();
                fields.push(Field {
                    name: vname,
                    type_annotation: None,
                });
            }
        }
    }

    Some(SemanticItem::Class {
        name,
        span: node_span(node),
        methods: vec![],
        fields,
    })
}

fn extract_impl(node: &tree_sitter::Node, source: &str) -> Vec<SemanticItem> {
    let mut methods = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "function_item" {
                // Prefix the method name with the impl type
                if let Some(SemanticItem::Function {
                    name,
                    params,
                    return_type,
                    body_hash,
                    decl_hash,
                    span,
                    children,
                }) = extract_function(&child, source)
                {
                    // `impl<'a> IgnoreMatch<'a>` names the type `IgnoreMatch`.
                    let impl_type = node
                        .child_by_field_name("type")
                        .map(|n| node_text(&n, source).split('<').next().unwrap_or_default().trim().to_string());
                    let qualified_name = if let Some(ref ty) = impl_type {
                        format!("{ty}::{name}")
                    } else {
                        name
                    };
                    methods.push(SemanticItem::Function {
                        name: qualified_name,
                        params,
                        return_type,
                        body_hash,
                        decl_hash,
                        span,
                        children,
                    });
                }
            }
        }
    }

    methods
}

fn extract_use(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    // Split `use a::b::{C, D}` / `use a::b::C` / `use a::b::*` into a module path
    // (`a::b`) and the symbols imported from it, so import changes can be
    // compared per module and per symbol.
    let arg = node.child_by_field_name("argument")?;
    let squash = |s: String| s.split_whitespace().collect::<String>();
    let (source_path, symbols) = match arg.kind() {
        "scoped_use_list" => {
            let path = arg
                .child_by_field_name("path")
                .map(|p| node_text(&p, source))
                .unwrap_or_default();
            let mut symbols = Vec::new();
            if let Some(list) = arg.child_by_field_name("list") {
                let mut inner = list.walk();
                for item in list.named_children(&mut inner) {
                    symbols.push(squash(node_text(&item, source)));
                }
            }
            (path, symbols)
        }
        "scoped_identifier" => {
            let path = arg.child_by_field_name("path").map(|p| node_text(&p, source));
            let name = arg.child_by_field_name("name").map(|n| node_text(&n, source));
            match (path, name) {
                (Some(p), Some(n)) => (p, vec![n]),
                _ => (node_text(&arg, source), vec![]),
            }
        }
        "use_wildcard" => {
            let text = node_text(&arg, source);
            (text.trim_end_matches("::*").to_string(), vec!["*".to_string()])
        }
        "use_as_clause" => {
            let text = squash(node_text(&arg, source));
            match text.rsplit_once("::") {
                Some((p, sym)) => (p.to_string(), vec![sym.to_string()]),
                None => (text, vec![]),
            }
        }
        _ => (squash(node_text(&arg, source)), vec![]),
    };

    Some(SemanticItem::Import {
        source: source_path,
        symbols,
        span: node_span(node),
    })
}

fn extract_trait(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;
    Some(SemanticItem::TypeDef {
        name,
        span: node_span(node),
    })
}

fn extract_type_alias(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;
    Some(SemanticItem::TypeDef {
        name,
        span: node_span(node),
    })
}

fn extract_variable(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node
        .child_by_field_name("name")
        .map(|n| node_text(&n, source))?;
    Some(SemanticItem::Variable {
        name,
        is_exported: false,
        span: node_span(node),
    })
}

/// An inline module (`mod tests { … }`) contributes its items, named `module::item`;
/// a file module declaration (`mod foo;`) is a single opaque item.
fn extract_mod(node: &tree_sitter::Node, source: &str) -> Vec<SemanticItem> {
    let Some(body) = node.child_by_field_name("body") else {
        return vec![SemanticItem::Other { span: node_span(node), content_hash: hash_str(&node_text(node, source)) }];
    };
    let module = node.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default();
    let mut items = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        for mut item in extract_item(&child, source) {
            match &mut item {
                SemanticItem::Function { name, .. }
                | SemanticItem::Class { name, .. }
                | SemanticItem::Variable { name, .. }
                | SemanticItem::TypeDef { name, .. } => *name = format!("{module}::{name}"),
                _ => {}
            }
            items.push(item);
        }
    }
    items
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
