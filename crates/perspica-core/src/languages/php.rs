use crate::languages::{FunctionDef, LanguageSupport};
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct PhpSupport;

impl LanguageSupport for PhpSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_php::LANGUAGE_PHP.into()
    }

    // Classes and functions are visible to any file that loads this one.
    fn is_exported(&self, _node: &tree_sitter::Node, _name: &str, _source: &str) -> bool {
        true
    }

    fn function_kinds(&self) -> &'static [&'static str] {
        &["function_definition", "method_declaration"]
    }

    fn function_def(&self, node: &tree_sitter::Node, source: &str) -> Option<FunctionDef> {
        let SemanticItem::Function { name, params, .. } = extract_function(node, source)? else { return None };
        Some(FunctionDef { name, params, bare: node.kind() == "function_definition" })
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

/// A braced `namespace A { … }` nests its members, so take them out of it. A plain
/// `namespace A;` comes before its members, which are already top-level.
fn extract_into(node: &tree_sitter::Node, source: &str, items: &mut Vec<SemanticItem>) {
    match node.kind() {
        "namespace_definition" => {
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    extract_into(&child, source, items);
                }
            }
        }
        "php_tag" | "text_interpolation" | "declare_statement" | "{" | "}" | "?>" => {}
        _ => {
            if let Some(item) = extract_item(node, source) {
                items.push(item);
            }
        }
    }
}

fn extract_item(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    match node.kind() {
        "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration" => extract_class(node, source),
        "function_definition" => extract_function(node, source),
        "namespace_use_declaration" => extract_use(node, source),
        "const_declaration" => {
            let name = first_const_name(node, source)?;
            Some(SemanticItem::Variable { name, is_exported: true, span: node_span(node) })
        }
        _ => {
            let text = node_text(node, source);
            if text.trim().is_empty() {
                return None;
            }
            // `require_once 'x.php'` loads another file, like an import.
            if let Some(path) = required_file(node, source) {
                return Some(SemanticItem::Import { bindings: vec![], source: path, symbols: vec![], span: node_span(node) });
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
        return_type: node.child_by_field_name("return_type").map(|n| node_text(&n, source)),
        body_hash: hash_str(&normalized_body),
        decl_hash: 0,
        span: node_span(node),
        children: vec![],
    })
}

/// `string $who`, `int $times = 1`, `string ...$extra` and promoted constructor parameters.
/// The `$` is left out of the name.
fn extract_params(list: &tree_sitter::Node, source: &str) -> Vec<Param> {
    let mut params = Vec::new();
    let mut cursor = list.walk();
    for p in list.named_children(&mut cursor) {
        if !matches!(p.kind(), "simple_parameter" | "variadic_parameter" | "property_promotion_parameter") { continue; }
        let name = p.child_by_field_name("name").map(|n| variable(&n, source)).unwrap_or_else(|| node_text(&p, source));
        let name = if p.kind() == "variadic_parameter" { format!("...{name}") } else { name };
        let type_annotation = p.child_by_field_name("type").map(|n| node_text(&n, source));
        let optional = p.child_by_field_name("default_value").is_some();
        params.push(Param { name, type_annotation, optional });
    }
    params
}

/// Classes, interfaces, traits and enums. Members come from the body and from promoted
/// constructor parameters. Constants and enum cases count as fields.
fn extract_class(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
    let mut methods = Vec::new();
    let mut fields = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "method_declaration" => {
                    if child.child_by_field_name("name").is_some_and(|n| node_text(&n, source) == "__construct") {
                        if let Some(params) = child.child_by_field_name("parameters") {
                            let mut pc = params.walk();
                            for p in params.named_children(&mut pc).filter(|p| p.kind() == "property_promotion_parameter") {
                                fields.push(Field {
                                    name: p.child_by_field_name("name").map(|n| variable(&n, source)).unwrap_or_default(),
                                    type_annotation: p.child_by_field_name("type").map(|n| node_text(&n, source)),
                                });
                            }
                        }
                    }
                    if let Some(f) = extract_function(&child, source) { methods.push(f); }
                }
                "property_declaration" => {
                    let ty = child.child_by_field_name("type").map(|n| node_text(&n, source));
                    let mut c = child.walk();
                    for el in child.named_children(&mut c).filter(|e| e.kind() == "property_element") {
                        let name = el.child_by_field_name("name").map(|n| variable(&n, source)).unwrap_or_default();
                        fields.push(Field { name, type_annotation: ty.clone() });
                    }
                }
                "const_declaration" => {
                    let mut c = child.walk();
                    for el in child.named_children(&mut c).filter(|e| e.kind() == "const_element") {
                        let mut ec = el.walk();
                        let name = el.named_children(&mut ec).find(|n| n.kind() == "name").map(|n| node_text(&n, source));
                        if let Some(name) = name { fields.push(Field { name, type_annotation: Some("const".into()) }); }
                    }
                }
                "enum_case" => {
                    let name = child.child_by_field_name("name").map(|n| node_text(&n, source)).unwrap_or_default();
                    fields.push(Field { name, type_annotation: Some("case".into()) });
                }
                _ => {}
            }
        }
    }
    Some(SemanticItem::Class { name, span: node_span(node), methods, fields })
}

/// `use App\Models\User;`, `use Foo\Bar as Baz;`, `use function strlen;` and `use App\{A, B};`.
fn extract_use(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut module = String::new();
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_use_clause" => {
                let mut c = child.walk();
                let target = child.named_children(&mut c).find(|n| matches!(n.kind(), "qualified_name" | "name")).map(|n| node_text(&n, source));
                if let Some(full) = target {
                    let (prefix, last) = full.rsplit_once('\\').unwrap_or(("", &full));
                    if module.is_empty() { module = prefix.trim_start_matches('\\').to_string(); }
                    symbols.push(last.to_string());
                }
            }
            "namespace_name" => module = node_text(&child, source),
            "namespace_use_group" => {
                let mut c = child.walk();
                for clause in child.named_children(&mut c) {
                    let text = node_text(&clause, source);
                    symbols.push(text.split_whitespace().next().unwrap_or("").rsplit('\\').next().unwrap_or("").to_string());
                }
            }
            _ => {}
        }
    }
    if module.is_empty() && symbols.is_empty() { return None; }
    if module.is_empty() { module = symbols.first().cloned().unwrap_or_default(); }
    Some(SemanticItem::Import { bindings: vec![], source: module, symbols, span: node_span(node) })
}

/// `require_once 'lib/x.php';` and friends, the file they load.
fn required_file(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let expr = if node.kind() == "expression_statement" { node.named_child(0)? } else { *node };
    if !matches!(expr.kind(), "require_once_expression" | "require_expression" | "include_once_expression" | "include_expression") { return None; }
    let text = node_text(&expr, source);
    let path = text.split(['\'', '"']).nth(1)?;
    Some(path.to_string())
}

fn first_const_name(node: &tree_sitter::Node, source: &str) -> Option<String> {
    let mut c = node.walk();
    let el = node.named_children(&mut c).find(|e| e.kind() == "const_element")?;
    let mut ec = el.walk();
    let name = el.named_children(&mut ec).find(|n| n.kind() == "name").map(|n| node_text(&n, source));
    name
}

/// A variable's name without its `$`.
fn variable(node: &tree_sitter::Node, source: &str) -> String {
    node_text(node, source).trim_start_matches('$').to_string()
}

fn node_text(node: &tree_sitter::Node, source: &str) -> String {
    source[node.byte_range()].to_string()
}

fn node_span(node: &tree_sitter::Node) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span { start_line: start.row + 1, start_col: start.column, end_line: end.row + 1, end_col: end.column }
}
