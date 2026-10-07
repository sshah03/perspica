use crate::languages::LanguageSupport;
use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// Language-agnostic code structure extracted from tree-sitter CST.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticTree {
    pub items: Vec<SemanticItem>,
    /// Per-item fingerprints, parallel to `items`. Filled in by `parse`.
    #[serde(skip)]
    pub meta: Vec<ItemMeta>,
    /// Lines (1-based) holding nothing but comments, from the syntax tree. Filled in by `parse`.
    #[serde(skip)]
    pub comment_lines: HashSet<usize>,
    /// Lines (1-based) inside a multi-line string, where whitespace matters.
    #[serde(skip)]
    pub string_lines: HashSet<usize>,
    /// Lines with a regex or a C `#define`, where some whitespace matters. Holds the regexes on
    /// the line, or `None` for a `#define` where the whole line counts.
    #[serde(skip)]
    pub literal_lines: LiteralLines,
    /// Functions defined inside other functions, which only their scope can call.
    #[serde(skip)]
    pub local_fns: Vec<LocalFn>,
}

/// A function defined inside another function, or in Python inside a module-level `if` or `try`.
/// Only code inside `scope` can call it.
#[derive(Debug, Clone)]
pub struct LocalFn {
    pub name: String,
    pub params: Vec<Param>,
    pub span: crate::manifest::Span,
    pub scope: crate::manifest::Span,
    /// The function it's defined in, or empty for module-level blocks.
    pub scope_name: String,
}

/// Regexes on each line, or `None` for a `#define` line.
pub type LiteralLines = HashMap<usize, Option<Vec<String>>>;

impl SemanticTree {
    pub fn new(items: Vec<SemanticItem>) -> Self {
        SemanticTree { items, meta: Vec::new(), comment_lines: HashSet::new(), string_lines: HashSet::new(), literal_lines: HashMap::new(), local_fns: Vec::new() }
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
    /// Names the item calls, with what qualifies each call (`x.name(`, `Type::name(`, `name(`).
    pub calls: HashSet<CallRef>,
    /// Local names bound to a type by an obvious initializer: `let b = Builder::new(…)`,
    /// `x := T{…}`, `const c = new C(…)`, `val v: V = …`, `Foo f = …`. Variable hash → type hash.
    pub binds: HashMap<u64, u64>,
    /// Local names bound to what a call returns: `ctx = self.request_context(…)`. Variable hash → the call.
    pub returns: HashMap<u64, CallRef>,
    /// The bindings that come from a declared type (`x: T`, `var x T`), not a guess from an initializer.
    pub declared: HashSet<u64>,
    /// Visible outside the file (export / pub / capitalized / non-static …).
    pub exported: bool,
    /// For classes: `norm_hash` without the methods, so changes elsewhere in the class
    /// (a nested class, a field, the header) still show up when a method changed too.
    pub shell_hash: u64,
    /// The item is a comment (dropped from the tree after parsing).
    pub is_comment: bool,
    /// Test code inside a source file: `#[test]` / `#[cfg(test)]` on the item or an
    /// enclosing module.
    pub is_test: bool,
    /// For classes: identifiers referenced by each method, parallel to `methods`.
    pub method_refs: Vec<HashSet<u64>>,
    /// Per method, parallel to `methods`: what each one calls.
    pub method_calls: Vec<HashSet<CallRef>>,
    pub method_binds: Vec<HashMap<u64, u64>>,
    pub method_returns: Vec<HashMap<u64, CallRef>>,
    pub method_declared: Vec<HashSet<u64>>,
    /// Per method: fields of `self` assigned from a call (`self.p = Parser(…)`). Field hash → the call.
    pub method_self_fields: Vec<HashMap<u64, CallRef>>,
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
        /// The names it brings into the file, as (imported name, local name). The imported name is
        /// "default" or "*" for a default or namespace import. Only filled in for JavaScript and TypeScript.
        #[serde(default)]
        bindings: Vec<(String, String)>,
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
    (sem.comment_lines, sem.string_lines, sem.literal_lines) = comment_and_string_lines(&tree, source);
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
/// What stands before a called name: nothing (`name(`), the receiver's own type
/// (`self.name(`, `this.name(`), a named thing (`x.name(`, `Type::name(`), or an
/// expression (`a.b().name(`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Qualifier {
    None,
    SelfType,
    /// `x.name(`: a receiver (a variable or a type).
    Named(u64),
    /// `a::name(`: a path (a type or a module), never a variable.
    Path(u64),
    /// `self.f.name(`: a field of the receiver's own type.
    Field(u64),
    /// `a.f.name(`: field f of variable a.
    Member(u64, u64),
    /// `Foo(…).name(` / `new Foo().name(` / `x.Foo(…).name(`: whatever constructing Foo gives (a Foo,
    /// if Foo is a type), or, when Foo isn't a type (`canvas.Compose(c)`, a Go method), what the
    /// chain's receiver (`canvas`, 0 for none) would.
    Constructed(u64, u64),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallRef {
    pub qualifier: Qualifier,
    pub name: u64,
    /// Followed by an argument list; otherwise a member access (`x.name`).
    pub parens: bool,
}

struct TokenWalk {
    norm: DefaultHasher,
    /// `norm` without the `members` subtrees (the class without its methods).
    shell: DefaultHasher,
    /// `norm` of just the function body, when one was given.
    body: Option<u64>,
    shape: DefaultHasher,
    tokens: Vec<u32>,
    refs: HashSet<u64>,
    calls: HashSet<CallRef>,
    binds: HashMap<u64, u64>,
    returns: HashMap<u64, CallRef>,
    declared: HashSet<u64>,
    self_fields: HashMap<u64, CallRef>,
}

/// Spots calls in the token stream as it goes by, with no tree lookups: a name
/// followed by an argument list is a call (`name(`, `name::<T>(`), the separator
/// before the name says what qualifies it (`x.name(`, `Type::name(`, `self.name(`,
/// `f().name(`), and a qualified name with no arguments is a member access, which
/// Scala uses for parameterless calls.
#[derive(Default)]
struct CallSpotter {
    /// The last identifier seen, with its qualifier, until the next token says what it was.
    pending: Option<CallRef>,
    /// The identifier before a `.` / `::`, if any, for the next name's qualifier.
    before_sep: Option<Qualifier>,
    /// Inside `<…>` type arguments (`foo::<T>(`), which don't end a pending call.
    type_args: usize,
    /// The name a separator just turned into a qualifier: still the callee if `::<T>(` follows.
    sep_from: Option<CallRef>,
    /// A variable just assigned (`x =`, `x :=`, `x:`), waiting for the type on the right.
    assign_lhs: Option<u64>,
    /// The right side has started: a second unqualified name there begins a new expression.
    rhs_started: bool,
    returns: HashMap<u64, CallRef>,
    /// The left side was declared with a type (`x: T`, `var x T`) rather than initialized.
    assign_declared: bool,
    /// Just after Go's `var`: the next name is being declared.
    var_decl: bool,
    declared: HashSet<u64>,
    /// The left side is a field of `self` / `this`.
    assign_field: bool,
    /// The last name seen was capitalized: if it's called, it's probably a constructor.
    callee_capitalized: bool,
    /// The last call and the nesting it was at, and the call a Python `with … as name` binds.
    last_call: Option<(CallRef, usize)>,
    as_from: Option<CallRef>,
    /// Just bound a `with … as name`: the `:` that follows ends the header, it isn't an annotation.
    after_as: bool,
    self_fields: HashMap<u64, CallRef>,
    /// The previous leaf, if it was a capitalized identifier (`Foo x` declares x as a Foo).
    prev_type: Option<u64>,
    binds: HashMap<u64, u64>,
    /// `b.go().again()`: a fluent chain keeps its root's qualifier; one slot per argument nesting.
    chains: Vec<Option<Qualifier>>,
    /// The binding just made and the nesting it was made at, while its right side lasts.
    fresh_bind: Option<(u64, usize)>,
    /// `x = T::new().build()`: the chain went on past `T`'s call, so `x` may not be a `T`.
    chain_check: Option<u64>,
    /// Per named node being walked: is it a place a function can be passed as a value?
    in_args: Vec<u8>,
    /// Just after `(` or `,` in an argument list: a lone name here may be a function passed as a value.
    arg_slot: bool,
    /// The bare name that started an argument, until `)` or `,` shows it stood alone.
    bare_arg: Option<u64>,
    /// Inside a parameter list or pattern: the names there are locals, not functions.
    pattern_depth: usize,
    locals: HashSet<u64>,
    /// The name before each open `[`, and after a `]` the one it closed: `f[T](` calls f (Go generics).
    brackets: Vec<Option<CallRef>>,
    bracketed: Option<CallRef>,
}

/// Calls that keep their receiver's type for the purposes of a binding (`T::new(…).unwrap()`).
const PASSTHROUGH: &[&str] = &["unwrap", "expect", "unwrap_or_default", "unwrap_or_else", "unwrap_or", "clone", "to_owned"];

const ARGUMENT_LISTS: &[&str] = &["arguments", "argument_list", "arguments_list"];

/// Where a lone name is a value handed on: `f(g)`, `{ onError: g }`, `onClick={g}`.
const SLOT_NONE: u8 = 0;
const SLOT_ARGS: u8 = 1;
const SLOT_PAIR: u8 = 2;
const SLOT_JSX: u8 = 3;

/// Where names are introduced rather than used: parameters, closure parameters, destructuring.
const PATTERNS: &[&str] = &["parameters", "formal_parameters", "parameter_list", "class_parameters", "closure_parameters",
    "lambda_parameters", "tuple_struct_pattern", "tuple_pattern", "struct_pattern", "slice_pattern", "pattern_list",
    "object_pattern", "array_pattern", "required_parameter", "optional_parameter"];

fn capitalized(text: &str) -> bool {
    text.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

impl CallSpotter {
    fn leaf(&mut self, kind: &str, text: &str, out: &mut HashSet<CallRef>) {
        if self.type_args > 0 { return; }
        // Only an argument list right after `f[T]` makes f the callee.
        let bracketed = self.bracketed.take();
        // Python's `self` and `cls`, and Rust's `Self`, are plain identifiers, not keywords.
        if kind.contains("identifier") && !matches!(text, "self" | "cls" | "Self") {
            self.flush_member(out);
            self.sep_from = None;
            let qualifier = self.before_sep.take().unwrap_or(Qualifier::None);
            let h = ident_hash(text);
            // `with open(p) as f`: f is what the call returns.
            self.after_as = false;
            if let Some(c) = self.as_from.take() {
                if qualifier == Qualifier::None && !capitalized(text) { self.returns.insert(h, c); self.locals.insert(h); self.after_as = true; }
            }
            if kind == "shorthand_property_identifier" { out.insert(CallRef { qualifier: Qualifier::None, name: h, parens: false }); }
            let slot = std::mem::take(&mut self.arg_slot);
            // `var wg sync.WaitGroup`: wg is declared with the type that follows.
            if std::mem::take(&mut self.var_decl) && qualifier == Qualifier::None {
                self.assign_lhs = Some(h); self.assign_declared = true; self.rhs_started = false;
                self.pending = Some(CallRef { qualifier, name: h, parens: false });
                return;
            }
            if qualifier == Qualifier::None && self.assign_lhs.is_some() {
                if self.rhs_started { self.assign_lhs = None; } else { self.rhs_started = true; }
            }
            self.bare_arg = (slot && qualifier == Qualifier::None).then_some(h);
            if self.pattern_depth > 0 { self.locals.insert(h); }
            if let Some(lhs) = self.chain_check.take() {
                if PASSTHROUGH.contains(&text) { self.chain_check = None; } else { self.binds.remove(&lhs); self.returns.remove(&lhs); self.self_fields.remove(&lhs); self.fresh_bind = None; }
            } else if qualifier == Qualifier::None {
                self.fresh_bind = None;
            }
            // Bindings: `x = Foo…` / `x: Foo` (the type on the right), or `Foo x` (the type on the left).
            if capitalized(text) {
                // `x = T(` / `x = a::T::new(` / `x = new pkg.T(`: the type is the capitalized name.
                // Kept for a call that follows: `x := NewServer(…)` binds x to what NewServer returns.
                if let Some(lhs) = self.assign_lhs.filter(|_| !self.assign_field) {
                    // The first type named wins: `k = Kind::Literal(…)` is a Kind.
                    // A declared type stands: `var s Service` then `s = &NoOp{}` is still a Service.
                    if self.fresh_bind.map(|(l, _)| l) != Some(lhs) && (self.assign_declared || !self.declared.contains(&lhs)) {
                        self.binds.insert(lhs, h);
                        if self.assign_declared { self.declared.insert(lhs); }
                        self.fresh_bind = Some((lhs, self.depth()));
                    }
                }
                self.prev_type = if qualifier == Qualifier::None { Some(h) } else { None };
            } else {
                if let (Some(ty), Qualifier::None) = (self.prev_type, qualifier) { self.binds.insert(h, ty); }
                self.prev_type = None;
            }
            self.pending = Some(CallRef { qualifier, name: h, parens: false });
            self.callee_capitalized = capitalized(text);
        } else if text == ":" && std::mem::take(&mut self.after_as) {
            self.pending = None; self.before_sep = None; self.assign_lhs = None;
        } else if text == ":" && self.slot() == SLOT_PAIR {
            // `{ onError: handleError }`: the key isn't a variable; the value may be a function.
            self.pending = None; self.before_sep = None; self.assign_lhs = None;
            self.arg_slot = true;
        } else if matches!(text, "=" | ":=" | ":") {
            // `x =` / `x :=` / `x:`: the type may follow.
            self.assign_field = false;
            self.assign_lhs = match self.pending.take() {
                Some(c) if c.qualifier == Qualifier::None && c.name != 0 => Some(c.name),
                // `self.x = Foo(…)`: the field's type, when the class doesn't declare it.
                Some(c) if c.qualifier == Qualifier::SelfType && c.name != 0 && text == "=" => { self.assign_field = true; Some(c.name) }
                _ => None,
            };
            if let Some(lhs) = self.assign_lhs { if !self.assign_field { self.locals.insert(lhs); } }
            self.rhs_started = false;
            self.assign_declared = text == ":";
            self.before_sep = None;
            self.prev_type = None;
            self.fresh_bind = None;
            self.arg_slot = false;
        } else if text == "as" {
            self.flush_member(out);
            let depth = self.depth();
            self.as_from = self.last_call.filter(|&(_, d)| d == depth).map(|(c, _)| c);
            self.pending = None; self.before_sep = None;
        } else if text == "var" && self.slot() != SLOT_ARGS {
            self.var_decl = true;
        } else if matches!(text, "new" | "mut" | "&" | "*" | "await" | "const") {
            // Between a binding's `=` and its type.
            self.flush_member(out);
            self.pending = None;
            self.arg_slot = false;
            if text == "await" { self.chain_check = None; }
        } else if text == "[" {
            let c = self.pending.take();
            if let Some(m) = c { if m.qualifier != Qualifier::None && m.name != 0 { out.insert(m); } }
            self.brackets.push(c);
            self.before_sep = None; self.assign_lhs = None; self.prev_type = None; self.arg_slot = false;
        } else if text == "]" {
            self.bracketed = self.brackets.pop().flatten();
            self.pending = None; self.before_sep = None;
        } else if text == "(" {
            // A bare `(` after a name: a call inside a macro body or a grammar without an arguments node.
            if let Some(mut c) = self.pending.take().or(bracketed) {
                if c.name != 0 { c.parens = true; self.start_chain(&c); self.note_returns(&c); self.last_call = Some((c, self.depth())); out.insert(c); }
            }
            self.before_sep = None;
            self.arg_slot = self.slot() == SLOT_ARGS;
        } else if text == "." || text == "::" || text == "->" {
            // The identifier before the separator qualifies the next one.
            let path = text == "::";
            let named = |h: u64| if path { Qualifier::Path(h) } else { Qualifier::Named(h) };
            self.before_sep = Some(match self.pending.take() {
                // `self.` / `this.`: the marker left by the keyword.
                Some(c) if c.name == 0 => Qualifier::SelfType,
                Some(c) if matches!(c.qualifier, Qualifier::None) => { self.sep_from = Some(c); named(c.name) }
                // `a.b.c`: `b` was a member access; `c` is qualified by `b`.
                Some(c) => {
                    out.insert(c);
                    match c.qualifier {
                        Qualifier::SelfType if !path => Qualifier::Field(c.name),
                        Qualifier::Named(a) if !path => Qualifier::Member(a, c.name),
                        _ => named(c.name),
                    }
                }
                // `f().g(`: the chain's root, if there is one at this nesting.
                None => {
                    if let Some((lhs, depth)) = self.fresh_bind { if depth == self.depth() { self.chain_check = Some(lhs); } }
                    self.chains.last().copied().flatten().unwrap_or(Qualifier::Unknown)
                }
            });
        } else if matches!(text, "self" | "this" | "Self" | "cls" | "super") {
            if self.assign_lhs.is_some() {
                if self.rhs_started { self.assign_lhs = None; } else { self.rhs_started = true; }
            }
            self.flush_member(out);
            self.pending = Some(CallRef { qualifier: Qualifier::SelfType, name: 0, parens: false });
        } else {
            // `f(g)` / `f(a, g)`: a lone name as an argument, which may be a function passed as a value.
            if (matches!(text, ")" | ",") && self.slot() == SLOT_ARGS) || (text == "}" && self.slot() == SLOT_JSX) {
                if let (Some(h), Some(c)) = (self.bare_arg, self.pending) {
                    if c.name == h && c.qualifier == Qualifier::None { out.insert(c); }
                }
            }
            // `for x in`: x is a local.
            if text == "in" { if let Some(c) = self.pending { if c.qualifier == Qualifier::None { self.locals.insert(c.name); } } }
            self.bare_arg = None;
            self.after_as = false;
            self.flush_member(out);
            self.pending = None;
            self.before_sep = None;
            self.assign_lhs = None;
            self.prev_type = None;
            self.arg_slot = (text == "," && self.slot() == SLOT_ARGS) || (text == "{" && self.slot() == SLOT_JSX);
            if text != ")" {
                self.fresh_bind = None;
                if let Some(slot) = self.chains.last_mut() { *slot = None; }
            }
        }
    }

    /// `x = f(…)` / `x = a.f(…)`: x is whatever f returns (unless the chain goes on).
    fn note_returns(&mut self, c: &CallRef) {
        if let Some(lhs) = self.assign_lhs.take() {
            if std::mem::take(&mut self.assign_field) {
                self.self_fields.insert(lhs, *c);
                self.fresh_bind = Some((lhs, self.depth()));
                return;
            }
            if self.declared.contains(&lhs) { return; }
            self.returns.insert(lhs, *c);
            if self.fresh_bind.map(|(l, _)| l) != Some(lhs) { self.fresh_bind = Some((lhs, self.depth())); }
        }
    }

    fn slot(&self) -> u8 {
        self.in_args.last().copied().unwrap_or(SLOT_NONE)
    }

    /// The argument nesting, the same before a chain's first call and after it.
    fn depth(&self) -> usize { self.chains.len().max(1) }

    fn start_chain(&mut self, c: &CallRef) {
        if self.chains.is_empty() { self.chains.push(None); }
        let constructor = self.callee_capitalized && matches!(c.qualifier, Qualifier::None | Qualifier::Named(_));
        let slot = self.chains.last_mut().unwrap();
        // `Foo(…).go(` / `pkg.Foo(…).go(`: a capitalized callee is likely a constructor.
        if constructor { *slot = Some(Qualifier::Constructed(c.name, if let Qualifier::Named(q) = c.qualifier { q } else { 0 })); }
        else if !matches!(c.qualifier, Qualifier::None | Qualifier::Unknown) { *slot = Some(c.qualifier); }
    }

    /// A qualified name that wasn't called: a member access (a Scala parameterless call).
    fn flush_member(&mut self, out: &mut HashSet<CallRef>) {
        if let Some(c) = self.pending.take() {
            if c.qualifier != Qualifier::None && c.name != 0 { out.insert(c); }
        }
    }

    fn enter(&mut self, kind: &str, out: &mut HashSet<CallRef>) {
        self.in_args.push(if ARGUMENT_LISTS.contains(&kind) { SLOT_ARGS } else if kind == "pair" { SLOT_PAIR } else if kind == "jsx_expression" { SLOT_JSX } else { SLOT_NONE });
        if PATTERNS.contains(&kind) { self.pattern_depth += 1; }
        if kind == "type_arguments" {
            // `name::<T>(`: the name before the `::` is still the callee.
            if let Some(c) = self.sep_from.take() { self.pending = Some(c); self.before_sep = None; }
            self.type_args += 1;
            return;
        }
        // A parameter list follows a definition's name, not a call.
        if matches!(kind, "parameters" | "formal_parameters" | "parameter_list" | "class_parameters" | "type_parameters") {
            self.pending = None; self.before_sep = None; return;
        }
        if matches!(kind, "arguments" | "argument_list" | "arguments_list") {
            if let Some(mut c) = self.pending.take().or(self.bracketed.take()) {
                if c.name != 0 { c.parens = true; self.start_chain(&c); self.note_returns(&c); self.last_call = Some((c, self.depth())); out.insert(c); }
            }
            self.before_sep = None;
            if self.chains.is_empty() { self.chains.push(None); }
            self.chains.push(None);
        }
    }

    fn leave(&mut self, kind: &str, out: &mut HashSet<CallRef>) {
        // `{ key: g }`: the pair ends right after its value.
        if kind == "pair" {
            if let (Some(h), Some(c)) = (self.bare_arg.take(), self.pending) {
                if c.name == h && c.qualifier == Qualifier::None { out.insert(c); }
            }
        }
        self.in_args.pop();
        if PATTERNS.contains(&kind) { self.pattern_depth = self.pattern_depth.saturating_sub(1); }
        if kind == "type_arguments" { self.type_args -= 1; }
        if matches!(kind, "arguments" | "argument_list" | "arguments_list") {
            self.chains.pop();
            // Back at the chain's level: whatever the arguments did is forgotten.
            self.pending = None; self.before_sep = None; self.assign_lhs = None; self.prev_type = None;
        }
    }
}

/// Marker hashed when leaving a named node, so the hash captures nesting
/// (`if c { a(); b(); }` vs `if c { a(); } b();`), not just pre-order sequence.
const SUBTREE_END: u16 = u16::MAX;

/// `skip` excludes one descendant subtree (e.g. a function body for its declaration hash).
fn walk_tokens(node: tree_sitter::Node, source: &str, mask: Option<&str>, skip: &[tree_sitter::Node]) -> TokenWalk {
    walk_tokens_with(node, source, mask, skip, &HashSet::new(), None, None)
}

/// Same as `walk_tokens`, but also hashes the item without the `members` subtrees into `shell`,
/// and the `body` subtree on its own into `body`, the same as walking the body by itself would.
#[allow(clippy::too_many_arguments)]
fn walk_tokens_with<'t>(
    node: tree_sitter::Node<'t>,
    source: &str,
    mask: Option<&str>,
    skip: &[tree_sitter::Node],
    members: &HashSet<usize>,
    body: Option<tree_sitter::Node>,
    mut defs: Option<(&[u16], &mut Vec<tree_sitter::Node<'t>>)>,
) -> TokenWalk {
    let mut w = TokenWalk {
        norm: DefaultHasher::new(),
        shell: DefaultHasher::new(),
        body: None,
        shape: DefaultHasher::new(),
        tokens: Vec::new(),
        refs: HashSet::new(),
        calls: HashSet::new(),
        binds: HashMap::new(),
        returns: HashMap::new(),
        declared: HashSet::new(),
        self_fields: HashMap::new(),
    };
    let mut cursor = node.walk();
    let mut spotter = CallSpotter::default();
    // How deep we are inside `members` subtrees. The shell hash only counts what's outside them.
    let mut in_member = 0usize;
    let is_member = |n: &tree_sitter::Node| !members.is_empty() && members.contains(&n.id());
    let body_id = body.map(|b| b.id());
    let mut in_body = false;
    let mut body_hash = DefaultHasher::new();
    'outer: loop {
        let n = cursor.node();
        if is_member(&n) { in_member += 1; }
        if let Some((kinds, found)) = defs.as_mut() {
            if kinds.contains(&n.kind_id()) { found.push(n); }
        }
        if body_id == Some(n.id()) { in_body = true; }
        let kind = n.kind();
        let comment = is_comment_kind(kind);
        // Directive comments like `//go:noinline` count as code. Strings are hashed from the source
        // since some grammars leave their whitespace out of the tokens.
        if (comment && is_build_directive(&source[n.byte_range()])) || is_string_kind(kind) {
            let text = &source[n.byte_range()];
            text.hash(&mut w.norm);
            text.hash(&mut w.shape);
            if in_member == 0 { text.hash(&mut w.shell); }
            if in_body { text.hash(&mut body_hash); }
        }
        let skip = comment || skip.contains(&n);
        if !skip {
            if n.child_count() == 0 {
                let text = &source[n.byte_range()];
                // PHP calls its identifiers `name`, and Ruby its capitalized ones `constant`.
                let kind = if matches!(kind, "name" | "constant") { "identifier" } else { kind };
                text.hash(&mut w.norm);
                if in_member == 0 { text.hash(&mut w.shell); }
                if in_body { text.hash(&mut body_hash); }
                if mask == Some(text) { "\u{0}NAME".hash(&mut w.shape) } else { text.hash(&mut w.shape) }
                w.tokens.push(hash_str(text) as u32);
                if kind.ends_with("identifier") || kind.ends_with("identifier_pattern") {
                    w.refs.insert(ident_hash(text));
                }
                spotter.leaf(kind, text, &mut w.calls);
            } else if n.is_named() {
                n.kind_id().hash(&mut w.norm);
                n.kind_id().hash(&mut w.shape);
                if in_member == 0 { n.kind_id().hash(&mut w.shell); }
                if in_body { n.kind_id().hash(&mut body_hash); }
                spotter.enter(kind, &mut w.calls);
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        loop {
            // Done with this node's subtree.
            if is_member(&cursor.node()) { in_member -= 1; }
            if body_id == Some(cursor.node().id()) {
                in_body = false;
                w.body = Some(body_hash.finish());
            }
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
                if in_member == 0 { SUBTREE_END.hash(&mut w.shell); }
                if in_body { SUBTREE_END.hash(&mut body_hash); }
                spotter.leave(cursor.node().kind(), &mut w.calls);
            }
        }
    }
    spotter.flush_member(&mut w.calls);
    // A lone argument that is a parameter or local is a value, not a function.
    let locals = std::mem::take(&mut spotter.locals);
    w.calls.retain(|c| c.parens || c.qualifier != Qualifier::None || !locals.contains(&c.name));
    w.binds = spotter.binds;
    w.returns = spotter.returns;
    w.declared = spotter.declared;
    w.self_fields = spotter.self_fields;
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

/// Hash of a function's declaration outside its body (visibility, `async`,
/// modifiers, generics, name …) plus any attached attributes.
fn decl_token_hash(node: tree_sitter::Node, attrs: &[tree_sitter::Node], source: &str) -> u64 {
    let mut h = DefaultHasher::new();
    for a in attrs {
        walk_tokens(*a, source, None, &[]).norm.finish().hash(&mut h);
    }
    walk_tokens(node, source, None, function_body(node).as_slice()).norm.finish().hash(&mut h);
    h.finish()
}

/// Attribute nodes (`#[derive(..)]`, `#[test]`) directly preceding an item.
/// They belong to that item: its span and fingerprint include them.
/// Comment node kinds end in "comment" in every grammar. This runs on every node, so it's a
/// suffix check instead of a substring search.
fn is_comment_kind(kind: &str) -> bool {
    kind.ends_with("comment")
}

/// A full string literal. Not its parts, and not `"a" "b"` since each part is hashed on its own.
fn is_string_kind(kind: &str) -> bool {
    matches!(kind, "string" | "template_string" | "text_block" | "interpolated_string" | "interpolated_string_expression" | "system_lib_string")
        || kind.ends_with("string_literal")
}

/// Comments that tools act on, like build constraints, type checker and linter switches,
/// and bundler hints. These should show up in the diff.
pub fn is_directive(comment: &str) -> bool {
    is_build_directive(comment)
        || comment.trim_start().starts_with("# type:")
        || [
            "@ts-ignore", "@ts-expect-error", "@ts-nocheck", "@ts-check", "@flow", "eslint-disable", "eslint-enable",
            "noqa", "nolint", "pragma:", "type: ignore", "pyright:", "mypy:", "rubocop:", "NOLINT", "clippy::",
            "prettier-ignore", "biome-ignore",
        ].iter().any(|d| comment.contains(d))
}

/// Directives that change what gets built, like `//go:noinline`, build constraints, shebangs,
/// JSX pragmas and bundler hints. They count as code in fingerprints. Linter and type checker
/// switches don't.
pub fn is_build_directive(comment: &str) -> bool {
    let t = comment.trim_start();
    t.starts_with("//go:") || t.starts_with("// +build") || t.starts_with("//export ") || t.starts_with("//line ")
        || t.starts_with("#!")
        // Check single characters first since this runs on every comment.
        || (t.contains('@') && (t.contains("@jsx") || t.contains("@__PURE__")))
        || (t.contains('#') && t.contains("#__PURE__"))
        || (t.contains('*') && t.contains("-*- coding"))
        || (t.contains("webpack") && t.contains("webpackChunkName"))
}

/// Attributes, decorators and build directives (on their own line) right before an item.
fn leading_attributes_and_directives<'t>(node: tree_sitter::Node<'t>, source: &str) -> Vec<tree_sitter::Node<'t>> {
    let own_line = |p: &tree_sitter::Node| {
        let start = p.start_byte();
        source[..start].rsplit('\n').next().is_none_or(|before| before.trim().is_empty())
    };
    let mut attrs = Vec::new();
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev.filter(|p| {
        matches!(p.kind(), "attribute_item" | "decorator")
            || (is_comment_kind(p.kind()) && own_line(p) && is_build_directive(&source[p.byte_range()]))
    }) {
        attrs.push(p);
        prev = p.prev_named_sibling();
    }
    attrs.reverse();
    attrs
}

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

/// Lines that only have comments, lines inside multi-line strings, and lines with a regex or
/// `#define`. Uses the syntax tree, so `* rate` continuing an expression or `#define` is code.
fn comment_and_string_lines(tree: &tree_sitter::Tree, source: &str) -> (HashSet<usize>, HashSet<usize>, LiteralLines) {
    let bytes = source.as_bytes();
    let mut in_comment = vec![false; bytes.len()];
    let mut strings = HashSet::new();
    let mut literals = LiteralLines::new();
    // Classify each node kind once, since this visits every node in the file.
    const OTHER: u8 = 1;
    const COMMENT: u8 = 2;
    const STRING: u8 = 3;
    const REGEX: u8 = 4;
    const DEFINE: u8 = 5;
    let mut kinds = vec![0u8; tree.language().node_kind_count() + 1];
    let mut cursor = tree.root_node().walk();
    'outer: loop {
        let n = cursor.node();
        let id = n.kind_id() as usize;
        if id < kinds.len() && kinds[id] == 0 {
            let kind = n.kind();
            kinds[id] = if is_comment_kind(kind) { COMMENT }
                else if is_string_kind(kind) { STRING }
                else if kind == "regex" { REGEX }
                else if kind.starts_with("preproc_def") || kind == "preproc_function_def" { DEFINE }
                else { OTHER };
        }
        let what = kinds.get(id).copied().unwrap_or(OTHER);
        if what == COMMENT {
            in_comment[n.start_byte()..n.end_byte().min(bytes.len())].iter_mut().for_each(|b| *b = true);
        } else if what == STRING && n.end_position().row > n.start_position().row {
            strings.extend(n.start_position().row + 2..=n.end_position().row + 1);
            if cursor.goto_first_child() { continue; }
        } else if what == REGEX {
            // Whitespace in a regex is content: `/ +/` vs `/  +/`.
            if let Some(found) = literals.entry(n.start_position().row + 1).or_insert_with(|| Some(Vec::new())) {
                found.push(source[n.byte_range()].to_string());
            }
        } else if what == DEFINE {
            // `#define F(x)` is a function-like macro, `#define F (x)` is a constant.
            for row in n.start_position().row + 1..=n.end_position().row + 1 { literals.insert(row, None); }
            if cursor.goto_first_child() { continue; }
        } else if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() { continue 'outer; }
            if !cursor.goto_parent() { break 'outer; }
        }
    }
    let mut out = HashSet::new();
    let (mut line, mut any, mut code) = (1, false, false);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            if any && !code { out.insert(line); }
            line += 1; any = false; code = false;
        } else if !b.is_ascii_whitespace() {
            if in_comment[i] { any = true } else { code = true }
        }
    }
    if any && !code { out.insert(line); }
    (out, strings, literals)
}

/// Picks out the local functions from the definitions found while walking one item. Each one's
/// scope is the closest function around it. A definition with nothing around it is the item
/// itself, unless it's in a module-level block, where it belongs to the whole module.
fn scope_local_fns(found: &[tree_sitter::Node], root: tree_sitter::Node, module_block: bool, source: &str, lang: &dyn LanguageSupport) -> Vec<LocalFn> {
    let defs: Vec<(tree_sitter::Node, crate::languages::FunctionDef)> = found.iter()
        .filter_map(|n| lang.function_def(n, source).map(|d| (*n, d)))
        .collect();
    let span = |n: &tree_sitter::Node| crate::manifest::Span {
        start_line: n.start_position().row + 1,
        start_col: n.start_position().column,
        end_line: n.end_position().row + 1,
        end_col: n.end_position().column,
    };
    let mut out = Vec::new();
    for (n, d) in &defs {
        if !d.bare { continue; }
        let around = defs.iter()
            .filter(|(o, _)| o.id() != n.id() && o.start_byte() <= n.start_byte() && n.end_byte() <= o.end_byte())
            .min_by_key(|(o, _)| o.end_byte() - o.start_byte());
        let (scope, scope_name) = match around {
            Some((o, od)) => (span(o), od.name.clone()),
            None if module_block => (span(&root), String::new()),
            None => continue,
        };
        out.push(LocalFn { name: d.name.clone(), params: d.params.clone(), span: span(n), scope, scope_name });
    }
    out
}

fn fingerprint(sem: &mut SemanticTree, tree: &tree_sitter::Tree, source: &str, lang: &dyn LanguageSupport) {
    let root = tree.root_node();
    let mut metas = Vec::with_capacity(sem.items.len());
    let def_kinds: Vec<u16> = lang.function_kinds().iter()
        .map(|k| tree.language().id_for_node_kind(k, true))
        .filter(|&id| id != 0)
        .collect();
    let mut local_fns = Vec::new();
    for item in &mut sem.items {
        let node = find_node(root, item.span());
        let mut meta = ItemMeta::default();
        if let Some(node) = node {
            meta.is_comment = is_comment_kind(node.kind());
            let mask = item.name().map(bare_name);
            let attrs = leading_attributes_and_directives(node, source);
            // Find the class's methods (and their attributes) once. The walk below leaves them out of
            // the shell hash, and each one is fingerprinted on its own further down.
            let method_nodes: Vec<Option<(tree_sitter::Node, Vec<tree_sitter::Node>)>> = match &*item {
                SemanticItem::Class { methods, .. } => methods.iter()
                    .map(|m| find_node(root, m.span()).map(|mn| (mn, leading_attributes_and_directives(mn, source))))
                    .collect(),
                _ => Vec::new(),
            };
            let members: HashSet<usize> = method_nodes.iter().flatten()
                .flat_map(|(mn, m_attrs)| m_attrs.iter().chain([mn]).map(|n| n.id()))
                .collect();
            let body = match &*item { SemanticItem::Function { .. } => function_body(node), _ => None };
            // Look for local functions in functions and module-level statements. Classes are
            // handled one method at a time below.
            let module_block = matches!(&*item, SemanticItem::Other { .. }) && lang.module_blocks_define();
            let finds_defs = !def_kinds.is_empty()
                && (matches!(&*item, SemanticItem::Function { .. } | SemanticItem::Variable { .. }) || module_block);
            let mut found = Vec::new();
            let defs = finds_defs.then_some((def_kinds.as_slice(), &mut found));
            let mut w = walk_tokens_with(node, source, mask, &[], &members, body, defs);
            local_fns.extend(scope_local_fns(&found, root, module_block, source, lang));
            let w_body = w.body;
            let mut shell = DefaultHasher::new();
            for a in &attrs {
                let aw = walk_tokens(*a, source, None, &[]);
                let h = aw.norm.finish();
                h.hash(&mut w.norm);
                h.hash(&mut w.shape);
                h.hash(&mut shell);
                w.tokens.extend(aw.tokens);
                w.refs.extend(aw.refs);
                w.calls.extend(aw.calls);
            }
            meta.is_test = is_test_node(node, source);
            meta.norm_hash = w.norm.finish();
            meta.shape_hash = w.shape.finish();
            meta.tokens = w.tokens;
            meta.tokens.sort_unstable();
            meta.refs = w.refs;
            meta.calls = w.calls;
            meta.binds = w.binds;
            meta.returns = w.returns;
            meta.declared = w.declared;
            if let Some(name) = item.name() {
                meta.exported = bare_name(name) == "main" || lang.is_exported(&node, name, source);
            }
            // Comment-insensitive body and declaration hashes for functions and class methods.
            match item {
                SemanticItem::Function { body_hash, decl_hash, .. } => {
                    if let Some(h) = w_body { *body_hash = h; }
                    *decl_hash = decl_token_hash(node, &attrs, source);
                }
                SemanticItem::Class { methods, .. } => {
                    w.shell.finish().hash(&mut shell);
                    meta.shell_hash = shell.finish();
                    for (m, found) in methods.iter_mut().zip(&method_nodes) {
                        let Some((mn, m_attrs)) = found else {
                            meta.method_refs.push(HashSet::new());
                            meta.method_calls.push(HashSet::new());
                            meta.method_binds.push(HashMap::new());
                            meta.method_returns.push(HashMap::new());
                            meta.method_declared.push(HashSet::new());
                            meta.method_self_fields.push(HashMap::new());
                            continue;
                        };
                        let mut found = Vec::new();
                        let defs = (!def_kinds.is_empty()).then_some((def_kinds.as_slice(), &mut found));
                        let mw = walk_tokens_with(*mn, source, None, &[], &HashSet::new(), function_body(*mn), defs);
                        local_fns.extend(scope_local_fns(&found, root, false, source, lang));
                        meta.method_refs.push(mw.refs);
                        meta.method_calls.push(mw.calls);
                        meta.method_binds.push(mw.binds);
                        meta.method_returns.push(mw.returns);
                        meta.method_declared.push(mw.declared);
                        meta.method_self_fields.push(mw.self_fields);
                        if let SemanticItem::Function { body_hash, decl_hash, .. } = m {
                            if let Some(h) = mw.body { *body_hash = h; }
                            *decl_hash = decl_token_hash(*mn, m_attrs, source);
                        }
                        extend_span_to(m.span_mut(), m_attrs);
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
    sem.local_fns = local_fns;
}

fn source_text(item: &SemanticItem, source: &str) -> String {
    let span = item.span();
    let lines: Vec<&str> = source.lines().collect();
    let start = span.start_line.saturating_sub(1);
    let end = span.end_line.min(lines.len());
    if start >= end { return String::new(); }
    lines[start..end].join("\n")
}
