use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures").join(name);
    dir.to_string_lossy().into_owned()
}

fn perspica(args: &[&str]) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_perspica"))
        .args(args)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("failed to run perspica");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// Run on a fixture pair and return the JSON output.
fn json(fixture_dir: &str, ext: &str) -> serde_json::Value {
    let before = fixture(&format!("{fixture_dir}/before.{ext}"));
    let after = fixture(&format!("{fixture_dir}/after.{ext}"));
    let (out, err, ok) = perspica(&[&before, &after, "--json"]);
    assert!(ok, "perspica failed: {err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("bad json ({e}): {out}"))
}

fn manifest(v: &serde_json::Value) -> &serde_json::Value {
    &v["results"][0]["manifest"]
}

#[test]
fn test_rename_detection_ts() {
    let v = json("rename_simple", "ts");
    let renames = manifest(&v)["renames"].as_array().unwrap();
    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0]["old_name"], "processData");
}

#[test]
fn test_signature_change_ts() {
    let v = json("signature_change", "ts");
    let sigs = manifest(&v)["signature_changes"].as_array().unwrap();
    assert!(!sigs.is_empty());
    assert!(sigs[0]["description"].as_str().unwrap().contains("added param"));
}

#[test]
fn test_add_dependency_ts() {
    let v = json("add_dependency", "ts");
    let deps = manifest(&v)["dependency_changes"].as_array().unwrap();
    assert!(deps.iter().any(|d| d["change_type"] == "added"));
}

#[test]
fn test_remove_dependency_ts() {
    let v = json("remove_dependency", "ts");
    let deps = manifest(&v)["dependency_changes"].as_array().unwrap();
    assert!(deps.iter().any(|d| d["change_type"] == "removed"));
}

#[test]
fn test_formatting_only_ts() {
    let v = json("formatting_only", "ts");
    let m = manifest(&v);
    assert!(m["logic_changes"].as_array().unwrap().is_empty());
    assert!(!m["formatting_only"].as_array().unwrap().is_empty());
    let review = &v["results"][0]["review"];
    assert_eq!(review["changed_lines"], review["mechanical_lines"], "all formatting lines are mechanical");
}

#[test]
fn test_logic_change_ts() {
    let v = json("logic_change_simple", "ts");
    assert!(!manifest(&v)["logic_changes"].as_array().unwrap().is_empty());
}

#[test]
fn test_mixed_changes_ts() {
    let v = json("mixed_changes", "ts");
    let m = manifest(&v);
    let total: usize = ["renames", "signature_changes", "dependency_changes", "logic_changes"]
        .iter()
        .map(|k| m[*k].as_array().unwrap().len())
        .sum();
    assert!(total >= 2, "mixed fixture has several kinds of change: {m}");
}

#[test]
fn test_extract_function_ts() {
    let v = json("extract_function", "ts");
    assert!(!manifest(&v)["extracted_functions"].as_array().unwrap().is_empty());
}

#[test]
fn test_dead_code_ts() {
    let v = json("dead_code", "ts");
    assert!(!manifest(&v)["dead_code"].as_array().unwrap().is_empty());
}

#[test]
fn test_rename_detection_py() {
    let v = json("rename_simple_py", "py");
    assert_eq!(manifest(&v)["renames"].as_array().unwrap().len(), 1);
}

#[test]
fn test_signature_change_py() {
    let v = json("signature_change_py", "py");
    assert!(!manifest(&v)["signature_changes"].as_array().unwrap().is_empty());
}

#[test]
fn test_add_dependency_py() {
    let v = json("add_dependency_py", "py");
    let deps = manifest(&v)["dependency_changes"].as_array().unwrap();
    assert!(deps.iter().any(|d| d["change_type"] == "added"));
}

#[test]
fn test_rename_detection_scala() {
    let v = json("rename_simple_scala", "scala");
    let renames = manifest(&v)["renames"].as_array().unwrap();
    assert_eq!(renames.len(), 1, "{renames:?}");
    assert_eq!(renames[0]["new_name"], "Text.normalizeInput");
}

#[test]
fn test_signature_change_scala() {
    let v = json("signature_change_scala", "scala");
    assert!(!manifest(&v)["signature_changes"].as_array().unwrap().is_empty());
}

#[test]
fn test_add_dependency_scala() {
    let v = json("add_dependency_scala", "scala");
    let deps = manifest(&v)["dependency_changes"].as_array().unwrap();
    assert!(deps.iter().any(|d| d["change_type"] == "added" && d["name"] == "java.time"), "{deps:?}");
}

#[test]
fn test_rename_detection_csharp() {
    let v = json("rename_simple_cs", "cs");
    let renames = manifest(&v)["renames"].as_array().unwrap();
    assert_eq!(renames.len(), 1, "{renames:?}");
    assert_eq!(renames[0]["new_name"], "Text.NormalizeInput");
}

#[test]
fn test_signature_change_csharp() {
    let v = json("signature_change_cs", "cs");
    assert!(!manifest(&v)["signature_changes"].as_array().unwrap().is_empty());
}

#[test]
fn test_add_dependency_csharp() {
    let v = json("add_dependency_cs", "cs");
    let deps = manifest(&v)["dependency_changes"].as_array().unwrap();
    assert!(deps.iter().any(|d| d["change_type"] == "added" && d["name"] == "System.Globalization"), "{deps:?}");
}

#[test]
fn test_rename_detection_kotlin() {
    let v = json("rename_simple_kt", "kt");
    let renames = manifest(&v)["renames"].as_array().unwrap();
    assert_eq!(renames.len(), 1, "{renames:?}");
    assert_eq!(renames[0]["new_name"], "Text.normalizeInput");
}

#[test]
fn test_signature_change_kotlin() {
    let v = json("signature_change_kt", "kt");
    assert!(!manifest(&v)["signature_changes"].as_array().unwrap().is_empty());
}

#[test]
fn test_add_dependency_kotlin() {
    let v = json("add_dependency_kt", "kt");
    let deps = manifest(&v)["dependency_changes"].as_array().unwrap();
    assert!(deps.iter().any(|d| d["change_type"] == "added" && d["name"] == "java.time"), "{deps:?}");
}

#[test]
fn test_hunks_are_linked_to_manifest() {
    let v = json("mixed_changes", "ts");
    let hunks = v["results"][0]["hunks"].as_array().unwrap();
    assert!(hunks.iter().any(|h| !h["manifest_refs"].as_array().unwrap().is_empty()));
}

#[test]
fn test_tty_output() {
    let (out, _, ok) = perspica(&[&fixture("rename_simple/before.ts"), &fixture("rename_simple/after.ts"), "--no-color"]);
    assert!(ok);
    assert!(out.contains("processData"));
    assert!(out.contains("rename only"), "rename-only hunk collapses: {out}");
}

#[test]
fn test_identical_files() {
    let f = fixture("rename_simple/before.ts");
    let (_, err, ok) = perspica(&[&f, &f]);
    assert!(ok);
    assert!(err.contains("No changes"));
}

#[test]
fn test_missing_file_errors() {
    let (_, err, ok) = perspica(&["/nonexistent/a.ts", "/nonexistent/b.ts"]);
    assert!(!ok);
    assert!(err.contains("reading"));
}

/// The local viewer only answers its own pages: no foreign Host (DNS rebinding)
/// and no cross-origin or non-JSON POSTs (drive-by analysis runs).
#[test]
fn test_web_server_rejects_foreign_requests() {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_perspica"))
        .args([&fixture("rename_simple/before.ts"), &fixture("rename_simple/after.ts"), "--web", "--no-open", "--port", "7981"])
        .env("PATH", "") // no `claude` CLI: no LLM provider needed for this test
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env("OLLAMA_HOST", "127.0.0.1:9") // nor a local model
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start perspica --web");
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut addr = String::new();
    for _ in 0..20 {
        let mut line = String::new();
        if stderr.read_line(&mut line).unwrap_or(0) == 0 { break; }
        if let Some(rest) = line.split("http://").nth(1) {
            addr = rest.split_whitespace().next().unwrap_or("").to_string();
            break;
        }
    }
    assert!(!addr.is_empty(), "server address");
    let status = |raw: String| -> u16 {
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);
        resp.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
    };
    let get = |host: &str| status(format!("GET /api/diff HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"));
    let post = |origin: &str, ctype: &str| status(format!(
        "POST /api/analyze HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\nContent-Type: {ctype}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"));
    let own = format!("http://{addr}");
    let results = (get(&addr), get("evil.example"), post("http://evil.example", "application/json"), post(&own, "text/plain"), post(&own, "application/json"));
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(results.0, 200, "own host");
    assert_eq!(results.1, 403, "rebinding host");
    assert_eq!(results.2, 403, "foreign origin");
    assert_eq!(results.3, 415, "form-style body");
    // Passes the guard; with no LLM provider the analysis itself is unavailable.
    assert_eq!(results.4, 503, "own origin, JSON");
}
