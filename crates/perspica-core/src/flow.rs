//! Call relationships between the items of the changed files, derived from
//! identifier references (no type information, so edges are by name).
//!
//! Two uses:
//! - a **reading order**: changed code walked from entry points down to the
//!   changed functions they reach, the order a reviewer should read it in;
//! - **test reach**: which changed functions the changed tests exercise,
//!   directly or through other functions.

use crate::cross_file::InternalAnalysis;
use crate::manifest::{Location, ManifestEntryId, Side};
use crate::parser::{bare_name, ident_hash, CallRef, Qualifier, SemanticItem};
use crate::roles::FileRole;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

/// Paths through unchanged code are followed this far when linking changed items.
const MAX_HOPS: usize = 3;
/// Test reach is followed this far from a test.
const MAX_TEST_DEPTH: usize = 8;
/// Shorter names are linked only when the qualifier names their type (`self.b(`).
const MIN_NAME_LEN: usize = 3;
/// Methods of the built-in containers, strings and promises in Python and JavaScript. On a
/// receiver of unknown type (`d.get(`, `xs.map(`) they are far likelier the built-in than a
/// same-named method in the change.
const BUILTIN_METHODS: &[&str] = &[
    "get", "pop", "popitem", "setdefault", "update", "copy", "clear", "keys", "values", "items", "append", "extend",
    "insert", "remove", "index", "count", "sort", "reverse", "add", "discard", "union", "intersection", "difference",
    "join", "split", "rsplit", "splitlines", "strip", "lstrip", "rstrip", "replace", "format", "startswith", "endswith",
    "lower", "upper", "encode", "decode", "find", "read", "write", "readline", "flush", "seek",
    "push", "shift", "unshift", "map", "filter", "forEach", "reduce", "some", "every", "findIndex", "includes", "indexOf",
    "slice", "splice", "concat", "entries", "set", "has", "delete", "then", "catch", "finally", "toString", "trim",
    "startsWith", "endsWith", "match", "test", "apply", "call", "bind",
];
/// Methods of the standard interfaces and traits in Go, Rust, Java and C# (`String()`, `Error()`,
/// `fmt`, `clone`, `next`, `toString`…). Nearly every type has them, so on a receiver of unknown
/// type one changed type defining it says nothing about which runs.
const INTERFACE_METHODS: &[&str] = &[
    "String", "Error", "ServeHTTP", "Read", "Write", "Close", "Len", "Less", "Swap", "Unwrap", "Format",
    "MarshalJSON", "UnmarshalJSON", "MarshalText", "UnmarshalText", "Is", "As", "Seek", "Flush", "Lock", "Unlock",
    "Wait", "Done", "Err", "Value", "Deadline",
    "fmt", "clone", "eq", "ne", "cmp", "partial_cmp", "hash", "from", "into", "try_from", "try_into", "default", "drop",
    "next", "deref", "deref_mut", "as_ref", "as_mut", "borrow", "borrow_mut", "to_string", "to_owned", "write", "read",
    "flush", "len", "is_empty", "iter", "into_iter",
    "toString", "equals", "hashCode", "compareTo", "close",
    "ToString", "Equals", "GetHashCode", "CompareTo", "Dispose", "DisposeAsync", "GetEnumerator", "MoveNext",
];
/// Names defined in more places than this are too ambiguous to link by name.
const MAX_DEFS_PER_NAME: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Types and data definitions. Read first: they're what the code manipulates.
    Type,
    /// Functions and methods, walked from entry points down their calls.
    Function,
    /// Constants, variables, top-level statements.
    Other,
}

/// One changed item in the reading order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingStep {
    pub name: String,
    pub file: String,
    pub kind: StepKind,
    /// Nesting under the step that calls it (0 = entry point).
    pub depth: usize,
    /// Manifest entries describing this item's changes.
    pub entry_ids: Vec<ManifestEntryId>,
    /// Changed items that call this one (directly or through unchanged code).
    pub called_by: Vec<String>,
    /// Changed items this one calls, in the order they first appear in its body.
    pub calls: Vec<String>,
    /// Already shown earlier in the order; listed again only to show the call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub repeat: bool,
}

/// How the changed tests reach a changed function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestReach {
    pub entry_id: ManifestEntryId,
    pub name: String,
    pub file: String,
    /// Test → … → function, shortest found. Empty when no changed test reaches it.
    pub via: Vec<String>,
    /// Changed tests that reach it (up to 3), as `file` + test name.
    pub tests: Vec<TestRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRef {
    pub file: String,
    pub name: String,
    pub line: usize,
}

struct Node<'a> {
    file: usize,
    name: String,
    kind: StepKind,
    span: (usize, usize),
    refs: &'a HashSet<u64>,
    calls: &'a HashSet<CallRef>,
    /// Receiver variable → its type, from parameter annotations and obvious initializers.
    binds: HashMap<u64, u64>,
    /// Receiver variable → the call it was assigned from (`ctx = self.request_context(…)`).
    returns: HashMap<u64, CallRef>,
    /// The bindings that come from a declared type rather than a guess from an initializer.
    declared: HashSet<u64>,
    /// The type a function declares it returns, unwrapped (`-> Result<Foo>` is a Foo).
    ret_type: Option<u64>,
    is_test: bool,
    /// Top-level unnamed statement (e.g. a `describe(...)` block in a test file).
    unnamed: bool,
    source_text: &'a str,
}

pub(crate) struct Graph<'a> {
    nodes: Vec<Node<'a>>,
    /// node → nodes it references by name.
    out: Vec<Vec<usize>>,
    /// node → changed-entry ids located in it.
    entries: Vec<Vec<ManifestEntryId>>,
}

/// The graph as built, for measuring it against a compiler's answer (`PERSPICA_GRAPH_DEBUG=1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDebug {
    pub nodes: Vec<DebugNode>,
    /// (caller, callee) indexes into `nodes`.
    pub edges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugNode {
    pub file: String,
    pub name: String,
    pub kind: StepKind,
    pub start: usize,
    pub end: usize,
    pub is_test: bool,
}

impl<'a> Graph<'a> {
    pub(crate) fn debug(&self, analyses: &[InternalAnalysis]) -> GraphDebug {
        GraphDebug {
            nodes: self.nodes.iter().map(|n| DebugNode {
                file: analyses[n.file].path.clone(), name: n.name.clone(), kind: n.kind, start: n.span.0, end: n.span.1, is_test: n.is_test,
            }).collect(),
            edges: self.out.iter().enumerate().flat_map(|(i, ts)| ts.iter().map(move |&t| (i, t))).collect(),
        }
    }

    pub(crate) fn build(analyses: &'a [InternalAnalysis]) -> Graph<'a> {
        let mut nodes = Vec::new();
        for (fi, a) in analyses.iter().enumerate() {
            let role = a.result.review.role;
            if !a.result.review.parsed {
                continue;
            }
            for (item, meta) in a.new_tree.items.iter().zip(&a.new_tree.meta) {
                let text = span_text(&a.new_source, item.span().start_line, item.span().end_line);
                let is_test = meta.is_test || role == FileRole::Test;
                match item {
                    SemanticItem::Import { .. } => {}
                    SemanticItem::Class { name, methods, span, .. } => {
                        // The type's body holds its methods' calls, so it takes their receiver bindings too.
                        let mut binds = HashMap::new();
                        let mut returns = HashMap::new();
                        let mut declared = HashSet::new();
                        for (k, m) in methods.iter().enumerate() {
                            binds.extend(param_binds(m, meta.method_binds.get(k)));
                            declared.extend(declared_of(m, meta.method_declared.get(k)));
                            if let Some(r) = meta.method_returns.get(k) { returns.extend(r.iter().map(|(v, c)| (*v, *c))); }
                        }
                        nodes.push(Node { file: fi, name: name.clone(), kind: StepKind::Type, span: (span.start_line, span.end_line), refs: &meta.refs, calls: &meta.calls, binds, returns, declared, ret_type: None, is_test, unnamed: false, source_text: text });
                        for (k, m) in methods.iter().enumerate() {
                            let (Some(mname), Some(refs), Some(calls)) = (m.name(), meta.method_refs.get(k), meta.method_calls.get(k)) else { continue };
                            let binds = param_binds(m, meta.method_binds.get(k));
                            let s = m.span();
                            nodes.push(Node {
                                file: fi,
                                name: format!("{name}.{mname}"),
                                kind: StepKind::Function,
                                span: (s.start_line, s.end_line),
                                refs,
                                calls,
                                binds,
                                returns: meta.method_returns.get(k).cloned().unwrap_or_default(),
                                declared: declared_of(m, meta.method_declared.get(k)),
                                // `-> Self` is the method's own type.
                                ret_type: returned_type(m).map(|t| if t == ident_hash("Self") { ident_hash(bare_name(name)) } else { t }),
                                is_test,
                                unnamed: false,
                                source_text: span_text(&a.new_source, s.start_line, s.end_line),
                            });
                        }
                    }
                    other => {
                        let s = other.span();
                        let mut binds = param_binds(other, Some(&meta.binds));
                        // Go's receiver (`func (p *Program) flush()`) is the method's own type, like `self`.
                        if let (Some(owner), Some(recv)) = (other.name().and_then(crate::cross_file::owner_of), go_receiver(text)) {
                            if a.path.ends_with(".go") { binds.insert(ident_hash(recv), ident_hash(owner)); }
                        }
                        nodes.push(Node {
                            file: fi,
                            name: other.name().map(str::to_string).unwrap_or_default(),
                            kind: match other {
                                SemanticItem::Function { .. } => StepKind::Function,
                                SemanticItem::TypeDef { .. } => StepKind::Type,
                                _ => StepKind::Other,
                            },
                            span: (s.start_line, s.end_line),
                            refs: &meta.refs,
                            calls: &meta.calls,
                            binds,
                            returns: meta.returns.clone(),
                            declared: declared_of(other, Some(&meta.declared)),
                            ret_type: returned_type(other),
                            is_test,
                            unnamed: other.name().is_none(),
                            source_text: text,
                        });
                    }
                }
            }
        }

        let mut by_name: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            // One- and two-letter names (`p`, `ok`) collide with locals everywhere.
            if !n.unnamed && !n.is_test {
                by_name.entry(ident_hash(bare_name(&n.name))).or_default().push(i);
            }
        }
        // A function in another file is only reachable if the caller's file names
        // that file's module (an import, `mod`, `crate::x::`, `from './x'` …).
        let module_of: Vec<String> = analyses.iter().map(|a| module_name(&a.path)).collect();
        let sees = |caller: usize, callee: usize| -> bool {
            caller == callee || {
                let m = &module_of[callee];
                !m.is_empty() && crate::classify::contains_identifier(&analyses[caller].new_source, m)
            }
        };
        // Naming the callee's type (`ignore::WalkBuilder`) is as good as naming its module.
        // Once per file and type: the scan reads the caller's whole source.
        let named_types: std::cell::RefCell<HashMap<(usize, &str), bool>> = Default::default();
        let sees_type = |caller: usize, callee: usize| -> bool {
            crate::cross_file::owner_of(&nodes[callee].name).is_some_and(|o| {
                *named_types.borrow_mut().entry((caller, o)).or_insert_with(|| crate::classify::contains_identifier(&analyses[caller].new_source, o))
            })
        };
        // The directory a file is in, for telling same-named methods apart.
        let dir_of = |file: usize| analyses[file].path.rsplit('/').nth(1).unwrap_or("");
        let owner_hash = |d: usize| crate::cross_file::owner_of(&nodes[d].name).map(ident_hash);
        let module_hash: Vec<u64> = module_of.iter().map(|m| ident_hash(m)).collect();
        // Field types per type, from every type definition in the change: (type, field) → field's type,
        // or None when same-named types disagree.
        let mut field_types: HashMap<(u64, u64), Option<u64>> = HashMap::new();
        for a in analyses.iter().filter(|a| a.result.review.parsed) {
            for item in &a.new_tree.items {
                if let SemanticItem::Class { name, fields, .. } = item {
                    for f in fields {
                        if let Some(t) = f.type_annotation.as_deref().and_then(type_name) {
                            let slot = field_types.entry((ident_hash(bare_name(name)), ident_hash(&f.name))).or_insert(Some(t));
                            if *slot != Some(t) { *slot = None; }
                        }
                    }
                }
            }
        }
        // Each type's parents, as its header names them: `class A(B):`, `class A extends B`, `impl T for A`.
        let mut parents: HashMap<u64, HashSet<u64>> = HashMap::new();
        for n in nodes.iter().filter(|n| n.kind == StepKind::Type) {
            let own = bare_name(&n.name);
            let header = n.source_text.lines().next().unwrap_or("");
            let named = header.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| capitalized_word(w) && *w != own);
            parents.entry(ident_hash(own)).or_default().extend(named.map(ident_hash));
        }
        // Whether `t` is `root` or descends from it.
        let descends = |t: u64, root: u64| -> bool {
            let (mut stack, mut seen) = (vec![t], HashSet::new());
            while let Some(x) = stack.pop() {
                if x == root { return true; }
                if seen.insert(x) { if let Some(ps) = parents.get(&x) { stack.extend(ps.iter().copied()); } }
            }
            false
        };
        let builtin: HashSet<u64> = BUILTIN_METHODS.iter().map(|m| ident_hash(m)).collect();
        let interface_methods: HashSet<u64> = INTERFACE_METHODS.iter().map(|m| ident_hash(m)).collect();
        let short = |d: usize| bare_name(&nodes[d].name).chars().count() < MIN_NAME_LEN;
        let known_owners: HashSet<u64> = (0..nodes.len()).filter_map(owner_hash).collect();
        // Among same-named candidates: the ones whose type the caller names, then the
        // ones in its directory, then its own file.
        let narrow = |n: &Node, cands: Vec<usize>| -> Vec<usize> {
            let mut cands = cands;
            if cands.len() > 1 {
                let owned: Vec<usize> = cands.iter().copied().filter(|&d| owner_hash(d).is_some_and(|o| n.refs.contains(&o))).collect();
                if !owned.is_empty() && owned.len() < cands.len() { cands = owned; }
            }
            if cands.len() > 1 {
                let near: Vec<usize> = cands.iter().copied().filter(|&d| dir_of(nodes[d].file) == dir_of(n.file)).collect();
                if !near.is_empty() && near.len() < cands.len() { cands = near; }
            }
            prefer_local(&cands, &nodes, n.file)
        };
        // The one type the functions a call can mean all return, if the graph knows it.
        let returned_by = |binds: &HashMap<u64, u64>, my_owner: Option<u64>, rc: &CallRef| -> Option<u64> {
            let defs = by_name.get(&rc.name)?;
            let receiver = |q: u64| binds.get(&q).copied().filter(|t| known_owners.contains(t)).unwrap_or(q);
            let types: HashSet<u64> = defs.iter().copied()
                .filter(|&d| nodes[d].kind == StepKind::Function)
                .filter(|&d| match rc.qualifier {
                    Qualifier::SelfType => owner_hash(d).is_some() && owner_hash(d) == my_owner,
                    Qualifier::Named(q) | Qualifier::Path(q) => owner_hash(d) == Some(receiver(q)) || (owner_hash(d).is_none() && module_hash[nodes[d].file] == q),
                    Qualifier::Constructed(t, _) => owner_hash(d) == Some(t),
                    Qualifier::None => owner_hash(d).is_none(),
                    _ => false,
                })
                .filter_map(|d| nodes[d].ret_type)
                .collect();
            if types.len() != 1 { return None; }
            types.into_iter().next().filter(|t| known_owners.contains(t))
        };
        // `self.p = Parser(…)` / `self.p = self.make()`: a field the class doesn't declare
        // takes the type it's assigned; assignments that disagree cancel out.
        let type_names: HashSet<u64> = nodes.iter().filter(|n| n.kind == StepKind::Type).map(|n| ident_hash(bare_name(&n.name))).collect();
        // Aliases the change defines: `type AnyApi = Api<any>` (TS, Rust, Scala, Go `type A = B`).
        let aliases: HashMap<u64, u64> = nodes.iter().filter(|n| n.kind == StepKind::Type)
            .filter_map(|n| alias_target(n.source_text).map(|t| (ident_hash(bare_name(&n.name)), t)))
            .filter(|(a, t)| a != t)
            .collect();
        let unalias = |t: u64| -> u64 { let mut t = t; for _ in 0..3 { match aliases.get(&t) { Some(&u) => t = u, None => break } } t };
        let no_binds = HashMap::new();
        let mut assigned: HashMap<(u64, u64), Option<u64>> = HashMap::new();
        for a in analyses.iter().filter(|a| a.result.review.parsed) {
            for (item, meta) in a.new_tree.items.iter().zip(&a.new_tree.meta) {
                let SemanticItem::Class { name, .. } = item else { continue };
                let owner = ident_hash(bare_name(name));
                for (&f, rc) in meta.method_self_fields.iter().flatten() {
                    let constructed = rc.qualifier == Qualifier::None && type_names.contains(&rc.name);
                    let Some(t) = (if constructed { Some(rc.name) } else { returned_by(&no_binds, Some(owner), rc) }) else { continue };
                    let slot = assigned.entry((owner, f)).or_insert(Some(t));
                    if *slot != Some(t) { *slot = None; }
                }
            }
        }
        for (k, v) in assigned { field_types.entry(k).or_insert(v); }
        let typed_fields: HashSet<u64> = field_types.keys().map(|&(t, _)| t).collect();
        // Each type's constructor methods: Python's `__init__`, JavaScript's `constructor`.
        let mut constructors: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.kind == StepKind::Function && matches!(bare_name(&n.name), "__init__" | "constructor") {
                if let Some(o) = owner_hash(i) { constructors.entry(o).or_default().push(i); }
            }
        }
        let out: Vec<Vec<usize>> = nodes.iter().enumerate().map(|(i, n)| {
            // A type's own body (`self.` in a class) belongs to the type itself.
            let my_owner = if n.kind == StepKind::Type { Some(ident_hash(bare_name(&n.name))) } else { crate::cross_file::owner_of(&n.name).map(ident_hash) };
            let mut targets: Vec<usize> = Vec::new();
            // Calls resolve to functions. A qualifier that names the callee's type (or the
            // caller's own type via `self`) settles it outright, however common the name.
            // Scala calls parameterless methods without parentheses (`x.size`).
            let member_calls = analyses[n.file].path.ends_with(".scala");
            // Rust and Go can't call a method without a receiver: a bare `name(` is a free function.
            let bare_is_free = analyses[n.file].path.ends_with(".rs") || analyses[n.file].path.ends_with(".go");
            // Where a field can't share a method's name, `self.name` with no call is the method itself.
            let dynamic = [".py", ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts"].iter().any(|e| analyses[n.file].path.ends_with(e));
            let self_values = dynamic;
            for call in n.calls {
                // `x.Foo(…).go(` where Foo isn't a type here: the chain is x's, as if Foo were any method.
                let call = &match call.qualifier {
                    Qualifier::Constructed(t, root) if root != 0 && !known_owners.contains(&unalias(t)) => CallRef { qualifier: Qualifier::Named(root), ..*call },
                    _ => *call,
                };
                // Without arguments, only a function passed as a value: `f(g)`, `Type::name`, `self.name`.
                let value = !call.parens && !member_calls;
                if value && !matches!(call.qualifier, Qualifier::None | Qualifier::Path(_)) && !(self_values && call.qualifier == Qualifier::SelfType) { continue; }
                // `Foo(…)` / `new Foo(…)` runs Foo's constructor (`__init__`, `constructor`).
                if call.parens && matches!(call.qualifier, Qualifier::None | Qualifier::Named(_)) {
                    if let Some(cs) = constructors.get(&call.name) {
                        targets.extend(narrow(n, cs.clone()).into_iter().filter(|&t| sees(n.file, nodes[t].file) || sees_type(n.file, t)));
                    }
                }
                let Some(defs) = by_name.get(&call.name) else { continue };
                let funcs: Vec<usize> = defs.iter().copied().filter(|&d| nodes[d].kind == StepKind::Function).collect();
                if funcs.is_empty() { continue; }
                let mut bound = false;
                let exact: Vec<usize> = match call.qualifier {
                    Qualifier::SelfType => funcs.iter().copied().filter(|&d| owner_hash(d).is_some() && owner_hash(d) == my_owner).collect(),
                    Qualifier::Named(q) | Qualifier::Path(q) | Qualifier::Field(q) | Qualifier::Member(_, q) => {
                        // `b.go(` where `b` is known to be a B is `B.go(`: from `b: B`, `b = B::new()`, or
                        // `b = make_b()` where make_b returns a B. `self.f.go(` and `a.f.go(` go through
                        // f's declared type. A declared type the change doesn't define means the call
                        // leaves the change: nothing here is its target.
                        let known = |t: u64| known_owners.contains(&t);
                        // A struct the change declares, with or without methods, has known fields;
                        // an alias the change declares stands for its target.
                        let base = |a: u64| n.binds.get(&a).map(|&t| unalias(t)).filter(|&t| known(t) || typed_fields.contains(&t));
                        let declared_type: Option<Option<u64>> = match call.qualifier {
                            Qualifier::Field(f) => my_owner.and_then(|o| field_types.get(&(o, f)).copied()),
                            Qualifier::Member(a, f) => base(a).and_then(|o| field_types.get(&(o, f)).copied()),
                            Qualifier::Named(_) if n.declared.contains(&q) => Some(n.binds.get(&q).copied()),
                            _ => None,
                        }.map(|t| t.map(unalias));
                        // A declared type with no methods here (outside the change, or an interface the
                        // change declares) puts the call outside what the change shows.
                        if matches!(declared_type, Some(Some(t)) if !known(t)) { continue; }
                        let ty = match call.qualifier {
                            Qualifier::Field(_) | Qualifier::Member(..) => declared_type.flatten(),
                            Qualifier::Named(_) => base(q).or_else(|| n.returns.get(&q).and_then(|rc| returned_by(&n.binds, my_owner, rc))),
                            _ => n.binds.get(&q).copied(),
                        };
                        let q = match ty { Some(t) if known_owners.contains(&t) => { bound = true; t } _ => q };
                        let by_owner: Vec<usize> = funcs.iter().copied().filter(|&d| owner_hash(d) == Some(q)).collect();
                        if by_owner.is_empty() { funcs.iter().copied().filter(|&d| owner_hash(d).is_none() && module_hash[nodes[d].file] == q).collect() } else { by_owner }
                    }
                    // Unqualified: a free function, a method of the caller's own type, or a constructor (`new Foo(`).
                    Qualifier::None if value => funcs.iter().copied().filter(|&d| owner_hash(d).is_none()).collect(),
                    Qualifier::None => funcs.iter().copied().filter(|&d| owner_hash(d).is_none() || (!bare_is_free && owner_hash(d) == my_owner)
                        || crate::cross_file::owner_of(&nodes[d].name) == Some(bare_name(&nodes[d].name))).collect(),
                    // `Foo(…).go(`: Foo's go, if Foo is a type here; otherwise like any unknown receiver.
                    Qualifier::Constructed(t, _) => {
                        let t = unalias(t);
                        if known_owners.contains(&t) { bound = true; funcs.iter().copied().filter(|&d| owner_hash(d) == Some(t)).collect() } else { vec![] }
                    }
                    Qualifier::Unknown => vec![],
                };
                // Only a qualifier that names the type settles a call beyond the import gate.
                let named = !matches!(call.qualifier, Qualifier::None | Qualifier::Unknown);
                // A receiver of unknown type calling a built-in container method: the built-in.
                let untyped = exact.is_empty() && !bound && matches!(call.qualifier, Qualifier::Named(_) | Qualifier::Field(_) | Qualifier::Member(..) | Qualifier::Unknown | Qualifier::Constructed(..));
                if untyped && (if dynamic { builtin.contains(&call.name) } else { interface_methods.contains(&call.name) }) { continue; }
                // A path, or a receiver whose type is known, that resolves to nothing stays unresolved.
                // An unknown receiver may still be any same-named method, but never a free function.
                // A call on a result (`f().g(`) can only be a method; `x.g(` may also be a namespaced free function.
                let (cands, settled) = if !exact.is_empty() { (exact, named) }
                    else if bound || matches!(call.qualifier, Qualifier::None | Qualifier::Path(_)) { (vec![], false) }
                    else if matches!(call.qualifier, Qualifier::Unknown | Qualifier::Constructed(..)) { (funcs.into_iter().filter(|&d| owner_hash(d).is_some()).collect(), false) }
                    else { (funcs, false) };
                if cands.is_empty() || (!settled && (cands.len() > MAX_DEFS_PER_NAME || short(cands[0]))) { continue; }
                let picked: Vec<usize> = narrow(n, cands).into_iter().filter(|&t| settled || sees(n.file, nodes[t].file) || sees_type(n.file, t)).collect();
                // A receiver of unknown type with same-named methods left on unrelated types is a
                // guess either way. On one hierarchy (`param.get_default(` on Parameter and its
                // subclass Option) it is whichever override runs.
                let guessed = !settled && matches!(call.qualifier, Qualifier::Named(_) | Qualifier::Field(_) | Qualifier::Member(..) | Qualifier::Unknown | Qualifier::Constructed(..));
                if guessed {
                    let owners: Vec<Option<u64>> = picked.iter().map(|&t| owner_hash(t)).collect::<HashSet<_>>().into_iter().collect();
                    let one_hierarchy = owners.iter().all(Option::is_some)
                        && owners.iter().flatten().any(|&root| owners.iter().flatten().all(|&o| descends(o, root)));
                    if owners.len() > 1 && !one_hierarchy { continue; }
                }
                targets.extend(picked);
            }
            // Types and values are reached by any mention of their name.
            for h in n.refs {
                let Some(defs) = by_name.get(h) else { continue };
                let others: Vec<usize> = defs.iter().copied().filter(|&d| nodes[d].kind != StepKind::Function && !short(d)).collect();
                if others.is_empty() || others.len() > MAX_DEFS_PER_NAME { continue; }
                targets.extend(narrow(n, others).into_iter().filter(|&t| sees(n.file, nodes[t].file)));
            }
            targets.retain(|&t| t != i && !contains(&nodes[i], &nodes[t]));
            targets.sort_unstable();
            targets.dedup();
            targets
        }).collect();

        // Entries → the most specific node containing their new-side location.
        let mut entries = vec![Vec::new(); nodes.len()];
        for (fi, a) in analyses.iter().enumerate() {
            for (id, loc) in changed_locations(a) {
                let best = nodes.iter().enumerate()
                    .filter(|(_, n)| n.file == fi && n.span.0 <= loc.line_start && loc.line_start <= n.span.1)
                    .min_by_key(|(_, n)| n.span.1 - n.span.0)
                    .map(|(i, _)| i);
                if let Some(i) = best {
                    if !entries[i].contains(&id) {
                        entries[i].push(id);
                    }
                }
            }
        }
        Graph { nodes, out, entries }
    }

    fn changed(&self, i: usize) -> bool {
        !self.entries[i].is_empty()
    }

    /// Changed functions reachable from `i` through unchanged functions only,
    /// ordered by where the call leading to each first appears in `i`.
    fn changed_callees(&self, i: usize) -> Vec<usize> {
        let mut found: Vec<(usize, usize)> = Vec::new(); // (callee, first hop)
        let mut seen = HashSet::from([i]);
        let mut queue = VecDeque::from([(i, 0, usize::MAX)]);
        while let Some((n, d, hop)) = queue.pop_front() {
            if d >= MAX_HOPS {
                continue;
            }
            for &t in &self.out[n] {
                let node = &self.nodes[t];
                if node.kind != StepKind::Function || node.is_test || !seen.insert(t) {
                    continue;
                }
                let hop = if d == 0 { t } else { hop };
                if self.changed(t) {
                    found.push((t, hop));
                } else {
                    queue.push_back((t, d + 1, hop));
                }
            }
        }
        let text = self.nodes[i].source_text;
        let pos = |hop: usize| crate::classify::find_identifier(text, bare_name(&self.nodes[hop].name)).unwrap_or(usize::MAX);
        found.sort_by_key(|&(t, hop)| (pos(hop), self.nodes[t].file, self.nodes[t].span.0));
        found.into_iter().map(|(t, _)| t).collect()
    }

    pub(crate) fn reading_order(&self, analyses: &[InternalAnalysis]) -> Vec<ReadingStep> {
        let changed: Vec<usize> = (0..self.nodes.len()).filter(|&i| self.changed(i) && !self.nodes[i].is_test).collect();
        let (types, rest): (Vec<usize>, Vec<usize>) = changed.iter().partition(|&&i| self.nodes[i].kind == StepKind::Type);
        let (functions, others): (Vec<usize>, Vec<usize>) = rest.iter().partition(|&&i| self.nodes[i].kind == StepKind::Function);
        let changed = functions;
        let callees: HashMap<usize, Vec<usize>> = changed.iter().map(|&i| (i, self.changed_callees(i))).collect();
        let mut callers: HashMap<usize, Vec<usize>> = HashMap::new();
        for (&from, tos) in &callees {
            for &t in tos {
                callers.entry(t).or_default().push(from);
            }
        }
        for v in callers.values_mut() {
            v.sort_unstable();
        }
        let reach = |i: usize| -> usize {
            let mut seen = HashSet::from([i]);
            let mut stack = vec![i];
            while let Some(n) = stack.pop() {
                for &t in callees.get(&n).map(Vec::as_slice).unwrap_or(&[]) {
                    if seen.insert(t) { stack.push(t); }
                }
            }
            seen.len()
        };
        // Entry points: nothing changed calls them. Bigger flows first, then file order.
        let mut roots: Vec<usize> = changed.iter().copied().filter(|i| !callers.contains_key(i)).collect();
        roots.sort_by_key(|&i| (std::cmp::Reverse(reach(i)), self.nodes[i].file, self.nodes[i].span.0));

        let flat = |i: usize, kind: StepKind| ReadingStep {
            name: self.nodes[i].name.clone(),
            file: analyses[self.nodes[i].file].path.clone(),
            kind,
            depth: 0,
            entry_ids: self.entries[i].clone(),
            called_by: vec![],
            calls: vec![],
            repeat: false,
        };
        let mut steps: Vec<ReadingStep> = types.iter().map(|&i| flat(i, StepKind::Type)).collect();
        let mut shown = HashSet::new();
        let name_of = |i: usize| self.nodes[i].name.clone();
        let visit = |start: usize, steps: &mut Vec<ReadingStep>, shown: &mut HashSet<usize>| {
            let mut stack = vec![(start, 0usize)];
            while let Some((i, depth)) = stack.pop() {
                let repeat = !shown.insert(i);
                let calls = callees.get(&i).cloned().unwrap_or_default();
                steps.push(ReadingStep {
                    name: name_of(i),
                    file: analyses[self.nodes[i].file].path.clone(),
                    kind: StepKind::Function,
                    depth,
                    entry_ids: self.entries[i].clone(),
                    called_by: callers.get(&i).map(|v| v.iter().map(|&c| name_of(c)).collect()).unwrap_or_default(),
                    calls: calls.iter().map(|&c| name_of(c)).collect(),
                    repeat,
                });
                if !repeat {
                    for &c in calls.iter().rev() {
                        stack.push((c, depth + 1));
                    }
                }
            }
        };
        for r in roots {
            if !shown.contains(&r) {
                visit(r, &mut steps, &mut shown);
            }
        }
        // Cycles with no entry point.
        for &i in &changed {
            if !shown.contains(&i) {
                visit(i, &mut steps, &mut shown);
            }
        }
        steps.extend(others.iter().map(|&i| flat(i, StepKind::Other)));
        steps
    }

    /// For each changed non-test function, the shortest path from a changed test.
    pub(crate) fn test_reach(&self, analyses: &[InternalAnalysis]) -> Vec<TestReach> {
        const MAX_TESTS: usize = 3;
        let tests: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].is_test && self.changed(i))
            .collect();
        // BFS from every changed test at once; remember each node's parent and origin test.
        let mut parent: HashMap<usize, usize> = HashMap::new();
        let mut origin: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut depth: HashMap<usize, usize> = HashMap::new();
        let mut queue = VecDeque::new();
        for &t in &tests {
            depth.insert(t, 0);
            origin.insert(t, vec![t]);
            queue.push_back(t);
        }
        while let Some(n) = queue.pop_front() {
            let d = depth[&n];
            if d >= MAX_TEST_DEPTH {
                continue;
            }
            let from = origin[&n].clone();
            for &t in &self.out[n] {
                if self.nodes[t].is_test {
                    continue;
                }
                match depth.get(&t) {
                    None => {
                        depth.insert(t, d + 1);
                        parent.insert(t, n);
                        origin.insert(t, from.clone());
                        queue.push_back(t);
                    }
                    // Same distance from another test: record it as another origin.
                    Some(&dt) if dt == d + 1 => {
                        let o = origin.get_mut(&t).unwrap();
                        for f in &from {
                            if o.len() < MAX_TESTS && !o.contains(f) { o.push(*f); }
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut out = Vec::new();
        for i in 0..self.nodes.len() {
            let n = &self.nodes[i];
            if n.is_test || n.unnamed || !self.changed(i) {
                continue;
            }
            let a = &analyses[n.file];
            if a.result.review.role != FileRole::Source {
                continue;
            }
            // Only functions/methods: the thing a test exercises.
            let Some(entry_id) = self.entries[i].iter().copied().find(|id| is_function_entry(a, *id)) else { continue };
            let (via, tests) = if depth.contains_key(&i) {
                let label = |node: &Node| if node.unnamed { unnamed_label(node.source_text) } else { node.name.clone() };
                let mut path = vec![label(n)];
                let mut cur = i;
                while let Some(&p) = parent.get(&cur) {
                    path.push(label(&self.nodes[p]));
                    cur = p;
                }
                path.reverse();
                let tests = origin[&i].iter().map(|&t| {
                    let tn = &self.nodes[t];
                    TestRef {
                        file: analyses[tn.file].path.clone(),
                        name: if tn.unnamed { first_line(tn.source_text) } else { tn.name.clone() },
                        line: tn.span.0,
                    }
                }).collect();
                (path, tests)
            } else {
                (vec![], vec![])
            };
            out.push(TestReach { entry_id, name: n.name.clone(), file: a.path.clone(), via, tests });
        }
        out
    }
}

/// The name other files use for this one: its stem, or the directory for
/// `index` / `mod` / `__init__` / `lib` files.
/// The type a parameter annotation names: `&mut B` → B, `Option<Box<B>>` → Option (harmless),
/// `ignore::WalkBuilder` → WalkBuilder, `*pkg.T` → T. Primitives (lowercase) don't count.
fn type_name(annotation: &str) -> Option<u64> {
    let t = annotation.trim().trim_start_matches(['&', '*']).trim_start();
    let t = ["mut ", "dyn ", "impl ", "final "].iter().fold(t, |t, kw| t.strip_prefix(kw).unwrap_or(t));
    let head = t.split(['<', '[', '(', ' ', ',']).next().unwrap_or("");
    let last = head.rsplit("::").next().unwrap_or(head).rsplit('.').next().unwrap_or(head);
    last.chars().next().is_some_and(|c| c.is_ascii_uppercase()).then(|| ident_hash(last))
}

/// What a function says it returns, through the wrappers that don't change what's inside:
/// `Result<Foo, E>`, `Option<Foo>`, `Optional[Foo]`, `Foo | None`, `Promise<Foo>`, `(*Foo, error)`.
fn returned_type(item: &SemanticItem) -> Option<u64> {
    let SemanticItem::Function { return_type: Some(rt), .. } = item else { return None };
    let mut t = rt.trim().trim_start_matches("->").trim().trim_matches('"').trim_start_matches('(').trim();
    t = t.split('|').map(str::trim).find(|p| !matches!(*p, "None" | "null" | "undefined")).unwrap_or(t);
    for _ in 0..3 {
        let head = t.split(['<', '[']).next().unwrap_or("").trim();
        let head = head.rsplit("::").next().unwrap_or(head).rsplit('.').next().unwrap_or(head);
        if !matches!(head, "Result" | "Option" | "Optional" | "Box" | "Rc" | "Arc" | "Promise" | "Awaitable" | "Ref" | "RefMut") { break; }
        let Some(open) = t.find(['<', '[']) else { break };
        t = t[open + 1..].trim();
    }
    type_name(t)
}

/// What a type alias stands for: `type AnyApi = Api<any, any>` → `Api`.
fn alias_target(text: &str) -> Option<u64> {
    let header = text.lines().next()?.trim();
    let rest = header.strip_prefix("export ").unwrap_or(header).trim_start_matches("pub ").trim_start_matches("pub(crate) ");
    let rest = rest.strip_prefix("type ")?;
    let (_, rhs) = rest.split_once('=')?;
    type_name(rhs.trim().trim_end_matches(';'))
}

/// The receiver variable of a Go method: `func (p *Program) flush()` → `p`.
fn go_receiver(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("func")?.trim_start().strip_prefix('(')?;
    let mut words = rest.split(')').next()?.split_whitespace();
    let name = words.next()?;
    words.next().map(|_| name)
}

/// Which of a function's bindings come from a declared type: its typed parameters and its
/// declared locals.
fn declared_of(item: &SemanticItem, local: Option<&HashSet<u64>>) -> HashSet<u64> {
    let mut declared = local.cloned().unwrap_or_default();
    if let SemanticItem::Function { params, .. } = item {
        declared.extend(params.iter().filter(|p| p.type_annotation.as_deref().and_then(type_name).is_some()).map(|p| ident_hash(&p.name)));
    }
    declared
}

/// A function's receiver bindings: its typed parameters plus its local initializers.
fn param_binds(item: &SemanticItem, local: Option<&HashMap<u64, u64>>) -> HashMap<u64, u64> {
    let mut binds = local.cloned().unwrap_or_default();
    if let SemanticItem::Function { params, .. } = item {
        for p in params {
            if let Some(t) = p.type_annotation.as_deref().and_then(type_name) { binds.insert(ident_hash(&p.name), t); }
        }
    }
    binds
}

fn module_name(path: &str) -> String {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or("");
    let stem = file.split('.').next().unwrap_or("");
    if matches!(stem, "index" | "mod" | "__init__" | "lib" | "main") {
        parts.next().unwrap_or(stem).to_string()
    } else {
        stem.to_string()
    }
}

/// Prefer definitions in the caller's own file when a name is defined in several.
fn capitalized_word(w: &str) -> bool {
    w.chars().next().is_some_and(|c| c.is_uppercase())
}

fn prefer_local(defs: &[usize], nodes: &[Node], file: usize) -> Vec<usize> {
    let local: Vec<usize> = defs.iter().copied().filter(|&d| nodes[d].file == file).collect();
    if local.is_empty() { defs.to_vec() } else { local }
}

/// `inner` is nested inside `outer` (a method inside its class).
fn contains(outer: &Node, inner: &Node) -> bool {
    outer.file == inner.file && outer.span.0 <= inner.span.0 && inner.span.1 <= outer.span.1
}

fn span_text(source: &str, start: usize, end: usize) -> &str {
    let mut offset = 0;
    let mut begin = None;
    for (n, line) in source.split_inclusive('\n').enumerate() {
        let ln = n + 1;
        if ln == start {
            begin = Some(offset);
        }
        offset += line.len();
        if ln == end {
            return begin.map_or("", |b| &source[b..offset]);
        }
    }
    begin.map_or("", |b| &source[b..])
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().chars().take(80).collect()
}

/// A short name for an unnamed item, such as `test("adds numbers", () => …)`: its
/// first string literal (the test's title), else the start of its first line.
fn unnamed_label(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let short = |s: &str, n: usize| if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() };
    for q in ['"', '\'', '`'] {
        if let Some(start) = line.find(q) {
            if let Some(len) = line[start + 1..].find(q) {
                return format!("“{}”", short(&line[start + 1..start + 1 + len], 40));
            }
        }
    }
    short(line, 40)
}

/// New-side locations of entries describing changed (not removed) code.
fn changed_locations(a: &InternalAnalysis) -> Vec<(ManifestEntryId, Location)> {
    let m = &a.result.manifest;
    let mut v: Vec<(ManifestEntryId, Location)> = Vec::new();
    v.extend(m.logic_changes.iter().filter(|e| e.location.side == Side::New).map(|e| (e.id, e.location.clone())));
    v.extend(m.signature_changes.iter().map(|e| (e.id, e.location.clone())));
    v.extend(m.renames.iter().flat_map(|e| e.locations.iter().map(move |l| (e.id, l.clone()))));
    v.extend(m.extracted_functions.iter().flat_map(|e| e.locations_new.iter().map(move |l| (e.id, l.clone()))));
    v
}

fn is_function_entry(a: &InternalAnalysis, id: ManifestEntryId) -> bool {
    use crate::manifest::SymbolKind;
    let m = &a.result.manifest;
    m.logic_changes.iter().any(|e| e.id == id && matches!(e.kind, SymbolKind::Function | SymbolKind::Method))
        || m.signature_changes.iter().any(|e| e.id == id)
        || m.renames.iter().any(|e| e.id == id && matches!(e.kind, SymbolKind::Function | SymbolKind::Method))
}

#[cfg(test)]
mod tests {
    use super::unnamed_label;

    #[test]
    fn unnamed_items_are_labeled_by_their_title() {
        assert_eq!(unnamed_label("test.each([false, true])(\"keeps keys (jitless: %s)\", async (jitless) => {"), "“keeps keys (jitless: %s)”");
        assert_eq!(unnamed_label("it('adds numbers', () => {"), "“adds numbers”");
        assert_eq!(unnamed_label("describe(`a very long title that goes on and on and on for a while`, () => {"), "“a very long title that goes on and on an…”");
        assert_eq!(unnamed_label("setup();"), "setup();");
    }
}
