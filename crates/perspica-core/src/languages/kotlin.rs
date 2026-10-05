use crate::languages::{FunctionDef, LanguageSupport};
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct KotlinSupport;

impl LanguageSupport for KotlinSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_kotlin_ng::LANGUAGE.into()
    }

    // Everything is public unless marked otherwise.
    fn is_exported(&self, node: &tree_sitter::Node, _name: &str, source: &str) -> bool {
        !has_modifier(node, source, &["private"])
    }

    fn function_kinds(&self) -> &'static [&'static str] {
        &["function_declaration"]
    }

    fn function_def(&self, node: &tree_sitter::Node, source: &str) -> Option<FunctionDef> {
        let SemanticItem::Function { name, params, .. } = extract_function(node, source)? else { return None };
        // A fun directly in a class or object body is a method.
        let method = node.parent().is_some_and(|p| matches!(p.kind(), "class_body" | "enum_class_body"));
        Some(FunctionDef { name, params, bare: !method })
    }

    fn extract_semantic_tree(&self, tree: &tree_sitter::Tree, source: &str) -> SemanticTree {
        let mut items = Vec::new();
        let root = tree.root_node();
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
        "class_declaration" | "object_declaration" => extract_class(node, source),
        "function_declaration" => extract_function(node, source),
        "property_declaration" => {
            let name = property_name(node, source)?;
            Some(SemanticItem::Variable { name, is_exported: !has_modifier(node, source, &["private"]), span: node_span(node) })
        }
        "type_alias" => {
            let name = node.child_by_field_name("type").map(|n| node_text(&n, source))?;
            Some(SemanticItem::TypeDef { name, span: node_span(node) })
        }
        "import" => extract_import(node, source),
        "package_header" => None,
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
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    let params = children.iter().find(|c| c.kind() == "function_value_parameters");
    // The return type is the type after the parameters.
    let return_type = params
        .and_then(|p| children.iter().find(|c| c.start_byte() >= p.end_byte() && c.kind().ends_with("_type")))
        .map(|n| node_text(n, source));
    let body = children.iter().find(|c| c.kind() == "function_body").map(|n| node_text(n, source)).unwrap_or_default();
    // Whitespace doesn't count, so reformatting isn't a change.
    let normalized_body: String = body.split_whitespace().collect();
    SemanticItem::Function {
        name,
        params: params.map(|p| extract_params(p, source)).unwrap_or_default(),
        return_type,
        body_hash: hash_str(&normalized_body),
        decl_hash: 0,
        span: node_span(node),
        children: vec![],
    }
}

/// A default value follows its parameter, and `vararg` comes before it, both as siblings.
fn extract_params(list: &tree_sitter::Node, source: &str) -> Vec<Param> {
    let mut params: Vec<Param> = Vec::new();
    let mut vararg = false;
    let mut cursor = list.walk();
    for c in list.named_children(&mut cursor) {
        match c.kind() {
            "parameter" | "class_parameter" => {
                let mut pc = c.walk();
                let parts: Vec<_> = c.named_children(&mut pc).collect();
                let name = parts.iter().find(|n| n.kind() == "identifier").map(|n| node_text(n, source)).unwrap_or_else(|| node_text(&c, source));
                let ty = parts.iter().find(|n| n.kind().ends_with("_type")).map(|n| node_text(n, source));
                let has_default = c.kind() == "class_parameter" && parts.last().is_some_and(|n| !n.kind().ends_with("_type") && n.kind() != "identifier" && n.kind() != "modifiers");
                let type_annotation = if std::mem::take(&mut vararg) { Some(format!("vararg {}", ty.unwrap_or_default())) } else { ty };
                params.push(Param { name, type_annotation, optional: has_default });
            }
            "parameter_modifiers" => vararg = node_text(&c, source).split_whitespace().any(|w| w == "vararg"),
            k if k.ends_with("comment") => {}
            _ => {
                if let Some(last) = params.last_mut() { last.optional = true; }
            }
        }
    }
    params
}

/// `class`, `data class`, `interface`, `enum class` and `object`. Members come from the
/// constructor and the body. Nested types count as members by name.
fn extract_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
    let mut methods = Vec::new();
    let mut fields = Vec::new();

    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    if let Some(ctor) = children.iter().find(|c| c.kind() == "primary_constructor") {
        if let Some(list) = child_of_kind(ctor, "class_parameters") {
            for p in extract_params(&list, source) {
                fields.push(Field { name: p.name, type_annotation: p.type_annotation });
            }
        }
    }
    if let Some(body) = children.iter().find(|c| matches!(c.kind(), "class_body" | "enum_class_body")) {
        class_members(body, source, &mut methods, &mut fields);
    }

    Some(SemanticItem::Class { name, span: node_span(node), methods, fields })
}

fn class_members(body: &tree_sitter::Node, source: &str, methods: &mut Vec<SemanticItem>, fields: &mut Vec<Field>) {
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(f) = extract_function(&child, source) { methods.push(f); }
            }
            "secondary_constructor" => methods.push(function_item(&child, source, "constructor".into())),
            "property_declaration" => {
                if let Some(name) = property_name(&child, source) {
                    let mut c = child.walk();
                    let ty = child.named_children(&mut c)
                        .find(|v| v.kind() == "variable_declaration")
                        .and_then(|v| { let mut vc = v.walk(); let t = v.named_children(&mut vc).find(|t| t.kind().ends_with("_type")); t })
                        .map(|t| node_text(&t, source));
                    fields.push(Field { name, type_annotation: ty });
                }
            }
            "enum_entry" => {
                if let Some(n) = child_of_kind(&child, "identifier") {
                    fields.push(Field { name: node_text(&n, source), type_annotation: None });
                }
            }
            // The companion's members belong to the class.
            "companion_object" => {
                if let Some(inner) = child_of_kind(&child, "class_body") {
                    class_members(&inner, source, methods, fields);
                }
            }
            "class_declaration" | "object_declaration" => {
                if let Some(n) = child.child_by_field_name("name") {
                    let kind = child.kind().trim_end_matches("_declaration");
                    fields.push(Field { name: node_text(&n, source), type_annotation: Some(kind.to_string()) });
                }
            }
            _ => {}
        }
    }
}

/// `val a = 1` is named a. `val (a, b) = pair` is named by the whole pattern.
fn property_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    let decl = node.named_children(&mut cursor).find(|c| matches!(c.kind(), "variable_declaration" | "multi_variable_declaration"))?;
    if decl.kind() == "multi_variable_declaration" {
        return Some(node_text(&decl, source));
    }
    let mut c = decl.walk();
    let name = decl.named_children(&mut c).find(|n| n.kind() == "identifier").map(|n| node_text(&n, source));
    name
}

/// `import a.b.C`, `import a.b.C as D` and `import a.b.*`.
fn extract_import(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut cursor = node.walk();
    let path = node.named_children(&mut cursor).find(|c| c.kind() == "qualified_identifier").map(|n| node_text(&n, source))?;
    let wildcard = node_text(node, source).trim_end().ends_with('*');
    let (module, symbols) = match path.rsplit_once('.') {
        Some((m, s)) if !wildcard => (m.to_string(), vec![s.to_string()]),
        _ => (path, vec![]),
    };
    Some(SemanticItem::Import { source: module, symbols, span: node_span(node) })
}

fn child_of_kind<'t>(node: &tree_sitter::Node<'t>, kind: &str) -> Option<tree_sitter::Node<'t>> {
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn has_modifier(node: &tree_sitter::Node, source: &str, words: &[&str]) -> bool {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor)
        .filter(|c| c.kind() == "modifiers")
        .any(|m| {
            let mut mc = m.walk();
            let any = m.named_children(&mut mc).any(|w| w.kind() != "annotation" && words.contains(&&source[w.byte_range()]));
            any
        });
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
