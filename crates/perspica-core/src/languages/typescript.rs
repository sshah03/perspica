use crate::languages::{FunctionDef, LanguageSupport};
use crate::manifest::Span;
use crate::parser::{hash_str, Field, Param, SemanticItem, SemanticTree};

pub struct TypeScriptSupport {
    /// Use the TSX grammar (for .tsx / .jsx files).
    pub tsx: bool,
}

impl LanguageSupport for TypeScriptSupport {
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        if self.tsx {
            tree_sitter_typescript::LANGUAGE_TSX.into()
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
    }

    fn is_exported(&self, node: &tree_sitter::Node, name: &str, source: &str) -> bool {
        // `export function foo` / `export const foo` / `export default …`
        let mut cur = node.parent();
        while let Some(p) = cur {
            if p.kind() == "export_statement" {
                return true;
            }
            if p.kind() == "program" {
                break;
            }
            cur = p.parent();
        }
        // `export { foo }` / `export default foo` elsewhere in the file
        let Some(root) = node.parent().filter(|p| p.kind() == "program").or_else(|| {
            node.parent().and_then(|p| p.parent()).filter(|p| p.kind() == "program")
        }) else {
            return false;
        };
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "export_statement" && child.child_by_field_name("declaration").is_none() {
                let text = &source[child.byte_range()];
                if crate::classify::contains_identifier(text, name) {
                    return true;
                }
            }
            // CommonJS: module.exports = { foo } / exports.foo = …
            if child.kind() == "expression_statement" {
                let text = &source[child.byte_range()];
                if (text.contains("module.exports") || text.starts_with("exports."))
                    && crate::classify::contains_identifier(text, name)
                {
                    return true;
                }
            }
        }
        false
    }

    fn function_kinds(&self) -> &'static [&'static str] {
        &["function_declaration", "generator_function_declaration", "variable_declarator", "method_definition"]
    }

    fn function_def(&self, node: &tree_sitter::Node, source: &str) -> Option<FunctionDef> {
        match node.kind() {
            "function_declaration" | "generator_function_declaration" => {
                let SemanticItem::Function { name, params, .. } = extract_function(node, source)? else { return None };
                Some(FunctionDef { name, params, bare: true })
            }
            // `const handle = (e) => { ... }`
            "variable_declarator" => {
                let name = node.child_by_field_name("name").filter(|n| n.kind() == "identifier")?;
                let value = node.child_by_field_name("value").filter(|v| matches!(v.kind(), "arrow_function" | "function_expression" | "function"))?;
                // `x => x + 1` has one bare parameter and no parentheses.
                let params = match value.child_by_field_name("parameter") {
                    Some(p) => vec![Param { name: node_text(&p, source), type_annotation: None, optional: false }],
                    None => extract_params(&value, source),
                };
                Some(FunctionDef { name: node_text(&name, source), params, bare: true })
            }
            // Methods are never called by their name alone, but they still contain local functions.
            "method_definition" => {
                let name = node.child_by_field_name("name").map(|n| node_text(&n, source))?;
                Some(FunctionDef { name, params: extract_params(node, source), bare: false })
            }
            _ => None,
        }
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
        "export_statement" => {
            // export_statement wraps the actual declaration; extract that
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_declaration" => return extract_function(&child, source),
                    "class_declaration" => return extract_class(&child, source),
                    "lexical_declaration" => return extract_variable(&child, source),
                    "type_alias_declaration" | "interface_declaration" | "enum_declaration" => return extract_typedef(&child, source),
                    "abstract_class_declaration" => return extract_class(&child, source),
                    _ => {} // skip keywords, decorators, etc.
                }
            }
            // `export default defineConfig({ ... })` or `export { a, b }`, kept as a plain statement.
            Some(SemanticItem::Other {
                span: node_span(node),
                content_hash: hash_str(&node_text(node, source)),
            })
        }
        "lexical_declaration" | "variable_declaration" => require_import(node, source).or_else(|| extract_variable(node, source)),
        "import_statement" => extract_import(node, source),
        "class_declaration" => extract_class(node, source),
        "type_alias_declaration" | "interface_declaration" | "enum_declaration" => extract_typedef(node, source),
        "abstract_class_declaration" => extract_class(node, source),
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
        if child.kind() == "required_parameter" || child.kind() == "optional_parameter" {
            let name = child
                .child_by_field_name("pattern")
                .map(|n| node_text(&n, source))
                .unwrap_or_else(|| node_text(&child, source));
            let type_annotation = child
                .child_by_field_name("type")
                .map(|n| node_text(&n, source));
            params.push(Param {
                name,
                type_annotation,
            optional: child.kind() == "optional_parameter" || child.child_by_field_name("value").is_some(),
            });
        }
    }
    params
}

fn extract_variable(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let name = child
                .child_by_field_name("name")
                .map(|n| node_text(&n, source))?;
            // `const f = (…) => {…}` / `const f = function (…) {…}` is a function.
            if let Some(value) = child.child_by_field_name("value").filter(|v| is_function_value(v)) {
                let mut params = extract_params(&value, source);
                if let Some(single) = value.child_by_field_name("parameter") {
                    params.push(Param { name: node_text(&single, source), type_annotation: None, optional: false });
                }
                let body = value.child_by_field_name("body").map(|n| node_text(&n, source)).unwrap_or_default();
                return Some(SemanticItem::Function {
                    name,
                    params,
                    return_type: value.child_by_field_name("return_type").map(|n| node_text(&n, source)),
                    body_hash: hash_str(&body.split_whitespace().collect::<String>()),
                    decl_hash: 0,
                    span: node_span(node),
                    children: vec![],
                });
            }
            return Some(SemanticItem::Variable {
                name,
                is_exported: false, // TODO: check parent for export
                span: node_span(node),
            });
        }
    }
    None
}

fn is_function_value(node: &tree_sitter::Node) -> bool {
    matches!(node.kind(), "arrow_function" | "function_expression" | "function" | "generator_function")
}

/// `const { a, b } = require('x')`, `const x = require('x')` and `const a = require('x').a`
/// are imports, like their `import` forms.
fn require_import(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut cursor = node.walk();
    let decls: Vec<_> = node.named_children(&mut cursor).filter(|c| c.kind() == "variable_declarator").collect();
    let [decl] = decls.as_slice() else { return None };
    let mut value = decl.child_by_field_name("value")?;
    let mut symbols = Vec::new();
    let mut member = None;
    if value.kind() == "member_expression" {
        member = Some(node_text(&value.child_by_field_name("property")?, source));
        symbols.extend(member.clone());
        value = value.child_by_field_name("object")?;
    }
    if value.kind() != "call_expression" || value.child_by_field_name("function").map(|f| node_text(&f, source)).as_deref() != Some("require") {
        return None;
    }
    let args = value.child_by_field_name("arguments")?;
    let mut c = args.walk();
    let arg_nodes: Vec<_> = args.named_children(&mut c).collect();
    let [module] = arg_nodes.as_slice() else { return None };
    if module.kind() != "string" { return None; }
    let module = node_text(module, source).trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string();
    // The names taken from the module. `{ a: b }` takes a, like `import { a as b }`.
    let pattern = decl.child_by_field_name("name")?;
    let mut bindings = Vec::new();
    match pattern.kind() {
        "object_pattern" => {
            let mut pc = pattern.walk();
            for p in pattern.named_children(&mut pc) {
                match p.kind() {
                    "shorthand_property_identifier_pattern" => {
                        let n = node_text(&p, source);
                        symbols.push(n.clone());
                        bindings.push((n.clone(), n));
                    }
                    "pair_pattern" => {
                        let (Some(k), Some(v)) = (p.child_by_field_name("key"), p.child_by_field_name("value")) else { continue };
                        symbols.push(node_text(&k, source));
                        if v.kind() == "identifier" { bindings.push((node_text(&k, source), node_text(&v, source))); }
                    }
                    _ => {}
                }
            }
        }
        "identifier" => bindings.push((member.unwrap_or_else(|| "default".into()), node_text(&pattern, source))),
        _ => {}
    }
    Some(SemanticItem::Import { source: module, symbols, span: node_span(node), bindings })
}

fn extract_import(node: &tree_sitter::Node, source: &str) -> Option<SemanticItem> {
    let mut source_module = String::new();
    let mut symbols = Vec::new();
    let mut bindings = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string" || child.kind() == "string_fragment" {
            source_module = node_text(&child, source)
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string();
        }
        if child.kind() == "import_clause" {
            let mut inner = child.walk();
            for ic in child.children(&mut inner) {
                match ic.kind() {
                    "identifier" => bindings.push(("default".to_string(), node_text(&ic, source))),
                    "namespace_import" => {
                        let mut ns = ic.walk();
                        let local = ic.named_children(&mut ns).find(|n| n.kind() == "identifier").map(|n| node_text(&n, source));
                        if let Some(local) = local { bindings.push(("*".to_string(), local)); }
                    }
                    "named_imports" => {
                        let mut imp = ic.walk();
                        for spec in ic.children(&mut imp) {
                            if spec.kind() == "import_specifier" {
                                let name = spec.child_by_field_name("name")
                                    .map(|n| node_text(&n, source))
                                    .unwrap_or_else(|| node_text(&spec, source));
                                let local = spec.child_by_field_name("alias").map(|n| node_text(&n, source)).unwrap_or_else(|| name.clone());
                                bindings.push((name.clone(), local));
                                symbols.push(name);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Try source field if we didn't get it from children
    if source_module.is_empty() {
        if let Some(src) = node.child_by_field_name("source") {
            source_module = node_text(&src, source)
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string();
        }
    }

    Some(SemanticItem::Import {
        source: source_module,
        symbols,
        span: node_span(node),
        bindings,
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
                "method_definition" => {
                    if let Some(item) = extract_function(&child, source) {
                        methods.push(item);
                    }
                }
                "public_field_definition" | "property_definition" => {
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
