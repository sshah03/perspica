use crate::languages::LanguageSupport;
use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

/// Language-agnostic code structure extracted from tree-sitter CST.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticTree {
    pub items: Vec<SemanticItem>,
    /// Per-item fingerprints, parallel to `items`. Filled in by `parse`.
    #[serde(skip)]
    pub meta: Vec<ItemMeta>,
}

impl SemanticTree {
    pub fn new(items: Vec<SemanticItem>) -> Self {
        SemanticTree { items, meta: Vec::new() }
    }
}

/// Token-level fingerprint of an item. Comments and whitespace are ignored,
/// so two items with equal `norm_hash` differ only in formatting or comments.
#[derive(Debug, Clone, Default)]
pub struct ItemMeta {
    /// Hash of the item's syntax (node kinds + leaf tokens, comments excluded).
    pub norm_hash: u64,
    /// Same as `norm_hash` but with the item's own name masked: equal shape
    /// hashes with different names means a pure rename.
    pub shape_hash: u64,
    /// Sorted leaf-token hashes, used for fuzzy similarity.
    pub tokens: Vec<u32>,
    /// Hashes of identifiers referenced anywhere in the item.
    pub refs: HashSet<u64>,
    /// Visible outside the file (export / pub / capitalized / non-static …).
    pub exported: bool,
    /// The item is a comment (dropped from the tree after parsing).
    pub is_comment: bool,
    /// Test code inside a source file: `#[test]` / `#[cfg(test)]` on the item or an
    /// enclosing module.
    pub is_test: bool,
    /// For classes: identifiers referenced by each method, parallel to `methods`.
    pub method_refs: Vec<HashSet<u64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub type_annotation: Option<String>,
    /// Callers may leave it out (a default value, `?`, …).
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub type_annotation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SemanticItem {
    Function {
        name: String,
        params: Vec<Param>,
        return_type: Option<String>,
        body_hash: u64,
        /// Hash of the declaration outside the body (modifiers, attributes …). Filled in by `parse`.
        #[serde(default)]
        decl_hash: u64,
        span: crate::manifest::Span,
        children: Vec<SemanticItem>,
    },
    Class {
        name: String,
        span: crate::manifest::Span,
        methods: Vec<SemanticItem>,
        fields: Vec<Field>,
    },
    Import {
        source: String,
        symbols: Vec<String>,
        span: crate::manifest::Span,
    },
    Variable {
        name: String,
        is_exported: bool,
        span: crate::manifest::Span,
    },
    TypeDef {
        name: String,
        span: crate::manifest::Span,
    },
    Other {
        span: crate::manifest::Span,
        content_hash: u64,
    },
}

impl SemanticItem {
    pub fn name(&self) -> Option<&str> {
        match self {
            SemanticItem::Function { name, .. } => Some(name),
            SemanticItem::Class { name, .. } => Some(name),
            SemanticItem::Import { source, .. } => Some(source),
            SemanticItem::Variable { name, .. } => Some(name),
            SemanticItem::TypeDef { name, .. } => Some(name),
            SemanticItem::Other { .. } => None,
        }
    }

    pub fn span(&self) -> &crate::manifest::Span {
        match self {
            SemanticItem::Function { span, .. }
            | SemanticItem::Class { span, .. }
            | SemanticItem::Import { span, .. }
            | SemanticItem::Variable { span, .. }
            | SemanticItem::TypeDef { span, .. }
            | SemanticItem::Other { span, .. } => span,
        }
    }

    pub fn span_mut(&mut self) -> &mut crate::manifest::Span {
        match self {
            SemanticItem::Function { span, .. }
            | SemanticItem::Class { span, .. }
            | SemanticItem::Import { span, .. }
            | SemanticItem::Variable { span, .. }
            | SemanticItem::TypeDef { span, .. }
            | SemanticItem::Other { span, .. } => span,
        }
    }

    /// Hash for quick equality check. Available for functions (body hash),
    /// Other (content hash). For classes, imports, variables, typedefs we
    /// return None, and the diff engine falls back to raw text comparison.
    pub fn body_hash(&self) -> Option<u64> {
        match self {
            SemanticItem::Function { body_hash, .. } => Some(*body_hash),
            SemanticItem::Other { content_hash, .. } => Some(*content_hash),
            _ => None,
        }
    }
}

/// Hash a string for body comparison.
pub fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Parse source code into a SemanticTree using the given language support.
pub fn parse(source: &str, lang: &dyn LanguageSupport) -> Result<SemanticTree, Error> {
    let mut ts_parser = tree_sitter::Parser::new();
    ts_parser
        .set_language(&lang.tree_sitter_language())
        .map_err(|e| Error::Parse(format!("failed to set language: {e}")))?;

    let tree = ts_parser
        .parse(source, None)
        .ok_or_else(|| Error::Parse("tree-sitter parse returned None".into()))?;

    let mut sem = lang.extract_semantic_tree(&tree, source);
    fingerprint(&mut sem, &tree, source, lang);
    Ok(sem)
}

/// The bare identifier of a possibly-qualified name (`Type::method` → `method`).
pub fn bare_name(name: &str) -> &str {
    let after_colons = name.rsplit("::").next().unwrap_or(name);
    after_colons.rsplit('.').next().unwrap_or(after_colons)
}

/// Hash an identifier for `ItemMeta::refs` lookups.
pub fn ident_hash(name: &str) -> u64 {
    hash_str(name)
}

fn find_node<'t>(root: tree_sitter::Node<'t>, span: &crate::manifest::Span) -> Option<tree_sitter::Node<'t>> {
    let start = tree_sitter::Point { row: span.start_line - 1, column: span.start_col };
    let end = tree_sitter::Point { row: span.end_line - 1, column: span.end_col };
    let node = root.descendant_for_point_range(start, end)?;
    if node.start_position() == start && node.end_position() == end {
        Some(node)
    } else {
        None
    }
}

/// Walk a subtree collecting a structural hash, a name-masked hash, token hashes and
/// referenced identifiers. Comments are skipped entirely.
struct TokenWalk {
    norm: DefaultHasher,
    shape: DefaultHasher,
    tokens: Vec<u32>,
    refs: HashSet<u64>,
}

/// Marker hashed when leaving a named node, so the hash captures nesting
/// (`if c { a(); b(); }` vs `if c { a(); } b();`), not just pre-order sequence.
const SUBTREE_END: u16 = u16::MAX;

/// `skip` excludes one descendant subtree (e.g. a function body for its declaration hash).
fn walk_tokens(node: tree_sitter::Node, source: &str, mask: Option<&str>, skip: Option<tree_sitter::Node>) -> TokenWalk {
    let mut w = TokenWalk {
        norm: DefaultHasher::new(),
        shape: DefaultHasher::new(),
        tokens: Vec::new(),
        refs: HashSet::new(),
    };
    let mut cursor = node.walk();
    'outer: loop {
        let n = cursor.node();
        let kind = n.kind();
        let skip = kind.contains("comment") || skip == Some(n);
        if !skip {
            if n.child_count() == 0 {
                let text = &source[n.byte_range()];
                text.hash(&mut w.norm);
                if mask == Some(text) { "\u{0}NAME".hash(&mut w.shape) } else { text.hash(&mut w.shape) }
                w.tokens.push(hash_str(text) as u32);
                if kind.contains("identifier") {
                    w.refs.insert(ident_hash(text));
                }
            } else if n.is_named() {
                n.kind_id().hash(&mut w.norm);
                n.kind_id().hash(&mut w.shape);
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        loop {
            if cursor.node() == node {
                break 'outer;
            }
            if cursor.goto_next_sibling() {
                continue 'outer;
            }
            if !cursor.goto_parent() {
                break 'outer;
            }
            // Leaving the parent's subtree (it was descended into, so it isn't skipped).
            if cursor.node().is_named() {
                SUBTREE_END.hash(&mut w.norm);
                SUBTREE_END.hash(&mut w.shape);
            }
        }
    }
    w
}

/// A function's body: its own `body` field, or (for `const f = (…) => {…}`)
/// the body of the function value inside the declaration.
fn function_body(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    if let Some(b) = node.child_by_field_name("body") {
        return Some(b);
    }
    let mut cursor = node.walk();
    for decl in node.named_children(&mut cursor) {
        if decl.kind() != "variable_declarator" { continue; }
        let value = decl.child_by_field_name("value")?;
        if matches!(value.kind(), "arrow_function" | "function_expression" | "function" | "generator_function") {
            return value.child_by_field_name("body");
        }
    }
    None
}

fn body_token_hash(node: tree_sitter::Node, source: &str) -> Option<u64> {
    let body = function_body(node)?;
    Some(walk_tokens(body, source, None, None).norm.finish())
}

/// Hash of a function's declaration outside its body (visibility, `async`,
/// modifiers, generics, name …) plus any attached attributes.
fn decl_token_hash(node: tree_sitter::Node, attrs: &[tree_sitter::Node], source: &str) -> u64 {
    let mut h = DefaultHasher::new();
    for a in attrs {
        walk_tokens(*a, source, None, None).norm.finish().hash(&mut h);
    }
    walk_tokens(node, source, None, function_body(node)).norm.finish().hash(&mut h);
    h.finish()
}

/// Attribute nodes (`#[derive(..)]`, `#[test]`) directly preceding an item.
/// They belong to that item: its span and fingerprint include them.
fn leading_attributes(node: tree_sitter::Node) -> Vec<tree_sitter::Node> {
    let mut attrs = Vec::new();
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev.filter(|p| p.kind() == "attribute_item") {
        attrs.push(p);
        prev = p.prev_named_sibling();
    }
    attrs.reverse();
    attrs
}

/// The node or an enclosing item carries a test attribute (`#[test]`,
/// `#[tokio::test]`, `#[cfg(test)]` …).
fn is_test_node(node: tree_sitter::Node, source: &str) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        let tested = leading_attributes(n).iter().any(|a| {
            let text = &source[a.byte_range()];
            let inner = text.trim_start_matches("#[").trim_end_matches(']');
            inner == "test" || inner.ends_with("::test") || inner.starts_with("test(") || inner.contains("cfg(test)")
        });
        if tested {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// Widen a span to start at the first leading attribute.
fn extend_span_to(span: &mut crate::manifest::Span, attrs: &[tree_sitter::Node]) {
    if let Some(first) = attrs.first() {
        span.start_line = first.start_position().row + 1;
        span.start_col = first.start_position().column;
    }
}

fn fingerprint(sem: &mut SemanticTree, tree: &tree_sitter::Tree, source: &str, lang: &dyn LanguageSupport) {
    let root = tree.root_node();
    let mut metas = Vec::with_capacity(sem.items.len());
    for item in &mut sem.items {
        let node = find_node(root, item.span());
        let mut meta = ItemMeta::default();
        if let Some(node) = node {
            meta.is_comment = node.kind().contains("comment");
            let mask = item.name().map(bare_name);
            let attrs = leading_attributes(node);
            let mut w = walk_tokens(node, source, mask, None);
            for a in &attrs {
                let aw = walk_tokens(*a, source, None, None);
                let h = aw.norm.finish();
                h.hash(&mut w.norm);
                h.hash(&mut w.shape);
                w.tokens.extend(aw.tokens);
                w.refs.extend(aw.refs);
            }
            meta.is_test = is_test_node(node, source);
            meta.norm_hash = w.norm.finish();
            meta.shape_hash = w.shape.finish();
            meta.tokens = w.tokens;
            meta.tokens.sort_unstable();
            meta.refs = w.refs;
            if let Some(name) = item.name() {
                meta.exported = bare_name(name) == "main" || lang.is_exported(&node, name, source);
            }
            // Comment-insensitive body and declaration hashes for functions and class methods.
            match item {
                SemanticItem::Function { body_hash, decl_hash, .. } => {
                    if let Some(h) = body_token_hash(node, source) { *body_hash = h; }
                    *decl_hash = decl_token_hash(node, &attrs, source);
                }
                SemanticItem::Class { methods, .. } => {
                    for m in methods {
                        let span = m.span().clone();
                        let Some(mn) = find_node(root, &span) else {
                            meta.method_refs.push(HashSet::new());
                            continue;
                        };
                        meta.method_refs.push(walk_tokens(mn, source, None, None).refs);
                        let m_attrs = leading_attributes(mn);
                        if let SemanticItem::Function { body_hash, decl_hash, .. } = m {
                            if let Some(h) = body_token_hash(mn, source) { *body_hash = h; }
                            *decl_hash = decl_token_hash(mn, &m_attrs, source);
                        }
                        extend_span_to(m.span_mut(), &m_attrs);
                    }
                }
                _ => {}
            }
            extend_span_to(item.span_mut(), &attrs);
        } else {
            meta.norm_hash = hash_str(&source_text(item, source).split_whitespace().collect::<String>());
            meta.shape_hash = meta.norm_hash;
        }
        metas.push(meta);
    }
    // Drop top-level comments: they are never semantic items.
    let mut items = Vec::with_capacity(sem.items.len());
    let mut kept = Vec::with_capacity(metas.len());
    for (item, meta) in sem.items.drain(..).zip(metas) {
        if !meta.is_comment {
            items.push(item);
            kept.push(meta);
        }
    }
    sem.items = items;
    sem.meta = kept;
}

fn source_text(item: &SemanticItem, source: &str) -> String {
    let span = item.span();
    let lines: Vec<&str> = source.lines().collect();
    let start = span.start_line.saturating_sub(1);
    let end = span.end_line.min(lines.len());
    if start >= end { return String::new(); }
    lines[start..end].join("\n")
}
