//! Real pull requests from other projects (tests/fixtures/real/*), checked for
//! what a reviewer relies on, not a snapshot of the full output, so ordinary
//! improvements don't break these tests.

use perspica_core::cross_file::{FileChange, MultiDiffResult};
use perspica_core::flow::StepKind;
use perspica_core::roles::FileRole;
use perspica_core::{analyze_multi, DiffResult, Language};
use std::path::{Path, PathBuf};

fn load(name: &str) -> MultiDiffResult {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/real").join(name);
    let mut paths = Vec::new();
    for side in ["before", "after"] {
        collect(&root.join(side), &root.join(side), &mut paths);
    }
    paths.sort();
    paths.dedup();
    let read = |side: &str, p: &str| std::fs::read_to_string(root.join(side).join(p)).unwrap_or_default();
    let files: Vec<FileChange> = paths.iter()
        .map(|p| FileChange::new(p.clone(), read("before", p), read("after", p), Language::from_path(p)))
        .collect();
    analyze_multi(&files).expect("analysis")
}

fn collect(base: &Path, dir: &Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() { collect(base, &p, out) } else { out.push(p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/")) }
    }
}

fn file<'a>(r: &'a MultiDiffResult, path: &str) -> &'a DiffResult {
    &r.file_results.iter().find(|(p, _)| p == path).unwrap_or_else(|| panic!("{path} in result")).1
}

fn logic(r: &DiffResult) -> Vec<String> {
    r.manifest.logic_changes.iter().map(|l| format!("{}: {}", l.name, l.description)).collect()
}

/// pallets/flask#5928: "all teardown callbacks are called despite errors" (Python).
#[test]
fn flask_5928_teardown_errors() {
    let r = load("flask-5928");
    assert_eq!(r.file_results.len(), 10);

    // The new helper and the functions it changed are found.
    assert!(logic(file(&r, "src/flask/helpers.py")).contains(&"_CollectErrors: added".to_string()), "{:?}", logic(file(&r, "src/flask/helpers.py")));
    let app = logic(file(&r, "src/flask/app.py"));
    assert!(app.iter().any(|l| l.starts_with("Flask.do_teardown_request")), "{app:?}");

    // Roles: tests and docs are tiered, not mixed in with source.
    assert_eq!(file(&r, "tests/test_basic.py").review.role, FileRole::Test);
    assert_eq!(file(&r, "CHANGES.rst").review.role, FileRole::Docs);

    // Reading order: the context's pop comes before the teardown methods it calls.
    let steps: Vec<(&str, usize)> = r.cross_file.reading_order.iter()
        .filter(|s| s.kind == StepKind::Function && !s.repeat)
        .map(|s| (s.name.as_str(), s.depth))
        .collect();
    let at = |n: &str| steps.iter().position(|(s, _)| *s == n).unwrap_or_else(|| panic!("{n} in {steps:?}"));
    assert!(at("AppContext.pop") < at("Flask.do_teardown_request"), "{steps:?}");
    assert!(steps[at("Flask.do_teardown_request")].1 > steps[at("AppContext.pop")].1, "called, so nested: {steps:?}");

    // The PR's new tests exercise every changed function.
    assert!(!r.cross_file.test_reach.is_empty());
    assert!(r.cross_file.test_reach.iter().all(|t| !t.via.is_empty()), "{:?}", r.cross_file.test_reach);

    // Nothing to flag: no stale references, no stale call sites.
    assert!(r.cross_file.broken_references.is_empty(), "{:?}", r.cross_file.broken_references);
    assert!(r.cross_file.signature_impacts.iter().all(|s| s.call_sites.iter().all(|c| c.updated)), "{:?}", r.cross_file.signature_impacts);
}

/// sindresorhus/ky#881: "Fix progress callbacks for empty request and response bodies" (TypeScript).
#[test]
fn ky_881_progress_callbacks() {
    let r = load("ky-881");
    assert_eq!(r.file_results.len(), 5);

    // `const withProgress = (…) => {…}` is a function, not a "value changed" variable.
    let body = logic(file(&r, "source/utils/body.ts"));
    assert!(body.iter().any(|l| l.starts_with("withProgress")), "{body:?}");
    assert!(!body.iter().any(|l| l.contains("value changed")), "{body:?}");
    assert!(r.cross_file.reading_order.iter().any(|s| s.name == "withProgress" && s.kind == StepKind::Function));

    assert_eq!(file(&r, "test/stream.ts").review.role, FileRole::Test);
    assert_eq!(file(&r, "readme.md").review.role, FileRole::Docs);
    assert!(file(&r, "test/stream.ts").review.test_lines > 0);
    assert!(r.cross_file.broken_references.is_empty());
}
