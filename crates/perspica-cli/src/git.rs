use perspica_core::Language;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

/// Files larger than this are shown as plain diffs without semantic analysis.
const MAX_PARSE_BYTES: usize = 1_500_000;

/// A single file's diff content extracted from git.
pub struct GitFileDiff {
    pub old_path: String,
    pub new_path: String,
    pub old_content: String,
    pub new_content: String,
    pub language: Language,
    /// Raw unified diff for this file (git diff -U3).
    pub raw_unified_diff: String,
    /// A, M, D, R (renamed), C (copied), T (type change).
    pub status: char,
    pub binary: bool,
    /// Role set in `.gitattributes`; detected from the path when `None`.
    pub role: Option<perspica_core::roles::FileRole>,
}

/// What to compare.
#[derive(Debug, Clone)]
pub enum Target {
    /// Index vs HEAD.
    Staged,
    /// Working tree vs a ref.
    WorkingTree(String),
    /// Two revisions.
    Range(String, String),
    /// Everything on the current branch: the working tree, untracked files
    /// included, against the merge-base with `base`.
    Branch { merge_base: String, base: String },
}

impl Target {
    /// Parse `--git` input: none/HEAD → working tree vs HEAD; `a..b`; `a...b` (merge-base); a single ref.
    pub fn from_spec(spec: Option<&str>) -> Result<Target, String> {
        let spec = spec.unwrap_or("HEAD");
        if let Some((a, b)) = spec.split_once("...") {
            let b = if b.is_empty() { "HEAD" } else { b };
            let base = git_cmd(&["merge-base", a, b])?;
            return Ok(Target::Range(base.trim().to_string(), b.to_string()));
        }
        if let Some((a, b)) = spec.split_once("..") {
            let b = if b.is_empty() { "HEAD" } else { b };
            return Ok(Target::Range(a.to_string(), b.to_string()));
        }
        Ok(Target::WorkingTree(spec.to_string()))
    }

    /// The current branch against its merge-base with `base` (default: the
    /// remote's default branch, else `main`/`master`).
    pub fn branch(base: Option<&str>) -> Result<Target, String> {
        let base = match base {
            Some(b) => b.to_string(),
            None => default_base().ok_or("couldn't find a base branch (origin/HEAD, origin/main, main, master); pass one: --branch <base>")?,
        };
        let merge_base = git_cmd(&["merge-base", &base, "HEAD"])?.trim().to_string();
        if merge_base.is_empty() {
            return Err(format!("no common ancestor between {base} and HEAD"));
        }
        Ok(Target::Branch { merge_base, base })
    }

    fn diff_args(&self) -> Vec<String> {
        match self {
            Target::Staged => vec!["--cached".into()],
            Target::WorkingTree(r) => vec![r.clone()],
            Target::Range(a, b) => vec![a.clone(), b.clone()],
            Target::Branch { merge_base, .. } => vec![merge_base.clone()],
        }
    }

    /// Human-readable label for headers.
    pub fn label(&self) -> String {
        match self {
            Target::Staged => "staged changes".into(),
            Target::WorkingTree(r) if r == "HEAD" => "working tree".into(),
            Target::WorkingTree(r) => format!("working tree vs {r}"),
            Target::Range(a, b) => format!("{}..{}", short(a), short(b)),
            Target::Branch { base, .. } => {
                let head = git_cmd(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
                let head = head.trim();
                let head = if head.is_empty() || head == "HEAD" { "HEAD" } else { head };
                let dirty = git_cmd(&["status", "--porcelain", "--untracked-files=normal"]).is_ok_and(|s| !s.trim().is_empty());
                format!("{head} vs {base}{}", if dirty { " (with uncommitted)" } else { "" })
            }
        }
    }

    /// Everything the short label abbreviates: full commit ids and refs.
    pub fn detail(&self) -> String {
        let full = |r: &str| git_cmd(&["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")])
            .ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let named = |r: &str| match full(r) {
            Some(sha) if sha != r => format!("{r} ({sha})"),
            Some(sha) => sha,
            None => r.to_string(),
        };
        match self {
            Target::Staged => format!("Staged changes against HEAD ({})", full("HEAD").unwrap_or_default()),
            Target::WorkingTree(r) => format!("Working tree against {}", named(r)),
            Target::Range(a, b) => format!("{} → {}", named(a), named(b)),
            Target::Branch { merge_base, base } => {
                let head = git_cmd(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
                format!("{} with uncommitted changes, against its merge-base with {base} ({merge_base})", named(head.trim()))
            }
        }
    }

    /// The new side is the working tree.
    fn is_worktree(&self) -> bool {
        matches!(self, Target::WorkingTree(_) | Target::Branch { .. })
    }
}

fn default_base() -> Option<String> {
    for cand in ["origin/HEAD", "origin/main", "origin/master", "main", "master"] {
        if git_cmd(&["rev-parse", "--verify", "--quiet", &format!("{cand}^{{commit}}")]).is_ok_and(|o| !o.trim().is_empty()) {
            return Some(cand.to_string());
        }
    }
    None
}

/// Commit time (unix seconds) of the target's base: agent activity before it
/// can't have produced this diff.
pub fn base_time(target: &Target) -> Option<i64> {
    let rev = match target {
        Target::Staged => "HEAD".to_string(),
        Target::WorkingTree(r) => r.clone(),
        Target::Range(a, _) => a.clone(),
        Target::Branch { merge_base, .. } => merge_base.clone(),
    };
    git_cmd(&["show", "-s", "--format=%ct", &format!("{rev}^{{commit}}")]).ok()?.trim().parse().ok()
}

/// Commit time of the target's head when it is a commit (not the working tree or index).
pub fn head_time(target: &Target) -> Option<i64> {
    let rev = match target {
        Target::Range(_, b) => b.clone(),
        // A branch with nothing uncommitted ends at its last commit.
        Target::Branch { .. } if git_cmd(&["status", "--porcelain", "--untracked-files=normal"]).is_ok_and(|s| s.trim().is_empty()) => "HEAD".into(),
        _ => return None,
    };
    git_cmd(&["show", "-s", "--format=%ct", &format!("{rev}^{{commit}}")]).ok()?.trim().parse().ok()
}

/// Whether this change is the local user's own: uncommitted work in this
/// checkout, or commits in the range authored with the configured name/email.
/// Someone else's PR or commits aren't: the local agent sessions aren't theirs.
pub fn is_own_change(target: &Target) -> bool {
    let dirty = || git_cmd(&["status", "--porcelain", "--untracked-files=normal"]).is_ok_and(|s| !s.trim().is_empty());
    let range = match target {
        Target::Staged | Target::WorkingTree(_) => return true,
        Target::Branch { merge_base, .. } => {
            if dirty() { return true; }
            format!("{merge_base}..HEAD")
        }
        Target::Range(a, b) => format!("{a}..{b}"),
    };
    // The identity git actually signs commits with: configured, or its
    // default (full name, user@host) when nothing is configured.
    let mut me: Vec<String> = Vec::new();
    if let Ok(ident) = git_cmd(&["var", "GIT_AUTHOR_IDENT"]) {
        if let (Some(lt), Some(gt)) = (ident.find('<'), ident.find('>')) {
            me.push(ident[..lt].trim().to_lowercase());
            me.push(ident[lt + 1..gt].trim().to_lowercase());
        }
    }
    me.retain(|v| !v.is_empty());
    if me.is_empty() {
        return false; // no identity to compare against
    }
    let Ok(authors) = git_cmd(&["log", "--format=%ae%x09%an", "-n", "500", &range]) else { return false };
    authors.lines().any(|l| l.split('\t').any(|who| me.contains(&who.trim().to_lowercase())))
}

/// The repository's name for display: `owner/name` from the `origin` remote
/// (GitHub, GitLab, … URLs, SSH or HTTPS), else the checkout's folder name.
/// With a web URL for the remote when there is one.
pub fn repo_identity() -> Option<(String, Option<String>)> {
    let remote = git_cmd(&["remote", "get-url", "origin"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(parsed) = remote.as_deref().and_then(parse_remote) {
        return Some(parsed);
    }
    let root = repo_root()?;
    Some((root.file_name()?.to_string_lossy().into_owned(), None))
}

/// `owner/name` and a web URL from a remote URL (SSH, scp-style or HTTPS).
fn parse_remote(url: &str) -> Option<(String, Option<String>)> {
    let cleaned = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let (host, path) = if let Some(rest) = cleaned.strip_prefix("git@") {
        rest.split_once(':').map(|(h, p)| (h.to_string(), p.to_string()))
    } else {
        cleaned.split_once("://").and_then(|(_, rest)| {
            let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
            rest.split_once('/').map(|(h, p)| (h.to_string(), p.to_string()))
        })
    }?;
    let host = host.split(':').next().unwrap_or(&host).to_string();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    (segments.len() >= 2).then(|| (segments[segments.len() - 2..].join("/"), Some(format!("https://{host}/{}", segments.join("/")))))
}

/// Top-level directory of the current repository.
pub fn repo_root() -> Option<std::path::PathBuf> {
    let out = git_cmd(&["rev-parse", "--show-toplevel"]).ok()?;
    let out = out.trim_end();
    if out.is_empty() { None } else { Some(std::path::PathBuf::from(out)) }
}

fn short(rev: &str) -> &str {
    if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) { &rev[..8] } else { rev }
}

/// Collect every changed file for a target: one `git diff` for patches, one
/// `git cat-file --batch` for contents.
pub fn collect(target: &Target) -> Result<Vec<GitFileDiff>, String> {
    let args = target.diff_args();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    // Pin options user config could change: root-relative paths, a/ b/ prefixes.
    let mut ns_args = vec!["diff", "-M", "--no-relative", "--name-status", "-z"];
    ns_args.extend(&arg_refs);
    let entries = parse_name_status_z(&git_cmd(&ns_args)?);
    if entries.is_empty() {
        return Ok(vec![]);
    }

    let mut patch_args = vec![
        "diff", "-M", "--no-relative", "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/", "-U3",
    ];
    patch_args.extend(&arg_refs);
    let patches = split_patches(&git_cmd(&patch_args)?);
    // Working-tree paths from git are relative to the repo root, not the cwd.
    let root = if target.is_worktree() { Some(repo_root().ok_or("not inside a git work tree")?) } else { None };

    // Blob requests for old/new contents.
    let (old_rev, new_rev): (Option<String>, Option<String>) = match target {
        Target::Staged => (Some("HEAD".into()), Some(String::new())), // "" → index (":path")
        Target::WorkingTree(r) => (Some(r.clone()), None),           // None → read from disk
        Target::Branch { merge_base, .. } => (Some(merge_base.clone()), None),
        Target::Range(a, b) => (Some(a.clone()), Some(b.clone())),
    };
    let mut requests = Vec::new();
    for (status, old_path, new_path) in &entries {
        let old_spec = if matches!(status, 'A' | 'C') { None } else { old_rev.as_ref().map(|r| format!("{r}:{old_path}")) };
        let new_spec = if *status == 'D' { None } else { new_rev.as_ref().map(|r| format!("{r}:{new_path}")) };
        requests.push((old_spec, new_spec));
    }
    let specs: Vec<&str> = requests.iter()
        .flat_map(|(o, n)| [o.as_deref(), n.as_deref()])
        .flatten()
        .collect();
    let blobs = cat_file_batch(&specs)?;

    let mut out = Vec::with_capacity(entries.len());
    for (i, (status, old_path, new_path)) in entries.into_iter().enumerate() {
        let (old_spec, new_spec) = &requests[i];
        let read_blob = |spec: &Option<String>| -> Vec<u8> {
            spec.as_ref().and_then(|s| blobs.get(s).cloned()).unwrap_or_default()
        };
        let old_bytes = read_blob(old_spec);
        let mut unreadable = false;
        let new_bytes = if status == 'D' {
            Vec::new()
        } else if let Some(root) = &root {
            let path = root.join(&new_path);
            match std::fs::read(&path) {
                Ok(b) => b,
                // Submodules show up as directories; they have no content to diff.
                Err(_) if path.is_dir() => Vec::new(),
                Err(e) => {
                    eprintln!("warning: couldn't read {new_path}: {e}; skipping semantic analysis for it");
                    unreadable = true;
                    Vec::new()
                }
            }
        } else {
            read_blob(new_spec)
        };
        let raw = patch_for(&patches, status, &old_path, &new_path);
        let binary = unreadable
            || raw.contains("\nBinary files ") || raw.contains("\nGIT binary patch")
            || old_bytes.contains(&0) || new_bytes.contains(&0);
        let too_big = old_bytes.len().max(new_bytes.len()) > MAX_PARSE_BYTES;
        let language = if binary || too_big { Language::Unknown } else { Language::from_path(&new_path) };
        out.push(GitFileDiff {
            old_path,
            new_path,
            old_content: if binary { String::new() } else { String::from_utf8_lossy(&old_bytes).into_owned() },
            new_content: if binary { String::new() } else { String::from_utf8_lossy(&new_bytes).into_owned() },
            language,
            raw_unified_diff: raw,
            status,
            binary,
            role: None,
        });
    }
    if let (Target::Branch { .. }, Some(root)) = (target, &root) {
        out.extend(untracked_files(root)?);
    }
    apply_attribute_roles(&mut out);
    Ok(out)
}

/// New files git doesn't track yet (respecting .gitignore), as additions.
fn untracked_files(root: &std::path::Path) -> Result<Vec<GitFileDiff>, String> {
    let list = git_cmd(&["ls-files", "--others", "--exclude-standard", "-z", "--full-name", ":/"])?;
    let mut out = Vec::new();
    for path in list.split('\0').filter(|p| !p.is_empty()) {
        let full = root.join(path);
        if full.is_dir() {
            continue; // nested repositories
        }
        let Ok(bytes) = std::fs::read(&full) else { continue };
        let binary = bytes.contains(&0);
        let too_big = bytes.len() > MAX_PARSE_BYTES;
        let content = if binary { String::new() } else { String::from_utf8_lossy(&bytes).into_owned() };
        out.push(GitFileDiff {
            old_path: path.to_string(),
            new_path: path.to_string(),
            raw_unified_diff: if binary { String::new() } else { addition_patch(path, &content) },
            old_content: String::new(),
            language: if binary || too_big { Language::Unknown } else { Language::from_path(path) },
            new_content: content,
            status: 'A',
            binary,
            role: None,
        });
    }
    Ok(out)
}

/// A unified diff adding `content` as a new file.
fn addition_patch(path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut p = format!("diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n", lines.len());
    for l in lines {
        p.push('+');
        p.push_str(l);
        p.push('\n');
    }
    p
}

/// Roles from `.gitattributes`: `perspica-role=<source|test|docs|generated|vendored>`,
/// or GitHub Linguist's `linguist-generated`, `linguist-vendored`, `linguist-documentation`.
/// One `git check-attr` process for all files.
fn apply_attribute_roles(files: &mut [GitFileDiff]) {
    use perspica_core::roles::{detect_role, FileRole};
    if files.is_empty() {
        return;
    }
    let Ok(mut child) = Command::new("git")
        .args(["check-attr", "-z", "--stdin", "perspica-role", "linguist-generated", "linguist-vendored", "linguist-documentation"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let input: Vec<u8> = files.iter().flat_map(|f| f.new_path.bytes().chain([0])).collect();
    let mut stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(s) = stdin.as_mut() {
            let _ = s.write_all(&input);
        }
    });
    let Ok(output) = child.wait_with_output() else { return };
    let _ = writer.join();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut attrs: std::collections::HashMap<&str, Vec<(&str, &str)>> = std::collections::HashMap::new();
    let fields: Vec<&str> = text.split('\0').collect();
    for chunk in fields.chunks(3) {
        if let [path, attr, value] = chunk {
            if *value != "unspecified" {
                attrs.entry(path).or_default().push((attr, value));
            }
        }
    }
    let truthy = |v: &str| matches!(v, "set" | "true");
    for f in files.iter_mut() {
        let Some(list) = attrs.get(f.new_path.as_str()) else { continue };
        let get = |name: &str| list.iter().find(|(a, _)| *a == name).map(|(_, v)| *v);
        if let Some(role) = get("perspica-role").and_then(FileRole::parse) {
            f.role = Some(role);
            continue;
        }
        let detected = detect_role(&f.new_path, if f.new_content.is_empty() { &f.old_content } else { &f.new_content });
        let mut role = detected;
        for (attr, r) in [("linguist-generated", FileRole::Generated), ("linguist-vendored", FileRole::Vendored), ("linguist-documentation", FileRole::Docs)] {
            match get(attr) {
                Some(v) if truthy(v) => role = r,
                // Explicitly not generated/vendored/docs: fall back to source.
                Some(_) if detected == r => role = FileRole::Source,
                _ => {}
            }
        }
        if role != detected {
            f.role = Some(role);
        }
    }
}

/// Parse `git diff --name-status -z` into (status, old_path, new_path).
fn parse_name_status_z(out: &str) -> Vec<(char, String, String)> {
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut entries = Vec::new();
    while let Some(status) = fields.next() {
        let code = status.chars().next().unwrap_or('M');
        match code {
            'R' | 'C' => {
                let (Some(old), Some(new)) = (fields.next(), fields.next()) else { break };
                entries.push((code, old.to_string(), new.to_string()));
            }
            _ => {
                let Some(path) = fields.next() else { break };
                entries.push((code, path.to_string(), path.to_string()));
            }
        }
    }
    entries
}

/// Split a multi-file patch into per-file sections keyed by their header line.
/// A file can have several sections (a type change is a deletion plus an
/// addition), so sections with the same header are concatenated.
fn split_patches(patch: &str) -> std::collections::HashMap<String, String> {
    let mut sections: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in patch.split_inclusive('\n') {
        if ["diff --git ", "diff --cc ", "diff --combined "].iter().any(|p| line.starts_with(p)) {
            current = Some(line.trim_end_matches('\n').to_string());
        }
        if let Some(key) = &current {
            sections.entry(key.clone()).or_default().push_str(line);
        }
    }
    sections
}

/// The patch for one name-status entry, found by its `diff --git` header
/// (git C-quotes paths with special characters) or, when unmerged, `diff --cc`.
fn patch_for(patches: &std::collections::HashMap<String, String>, status: char, old: &str, new: &str) -> String {
    let (a, b) = (format!("a/{old}"), format!("b/{new}"));
    let mut keys = vec![
        format!("diff --git {} {}", c_quote(&a), c_quote(&b)),
        format!("diff --git {a} {b}"),
    ];
    if status == 'U' {
        keys.push(format!("diff --cc {}", c_quote(new)));
        keys.push(format!("diff --combined {}", c_quote(new)));
    }
    keys.iter().find_map(|k| patches.get(k)).cloned().unwrap_or_default()
}

/// Quote a path the way git does in patch headers (core.quotePath=true).
fn c_quote(path: &str) -> String {
    let needs = path.bytes().any(|b| !(0x20..0x7f).contains(&b) || b == b'"' || b == b'\\');
    if !needs {
        return path.to_string();
    }
    let mut out = String::from("\"");
    for b in path.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push('"');
    out
}

/// Fetch many blobs in one process. Missing objects map to nothing.
fn cat_file_batch(specs: &[&str]) -> Result<std::collections::HashMap<String, Vec<u8>>, String> {
    cat_file_batch_in(None, specs)
}

fn cat_file_batch_in(dir: Option<&std::path::Path>, specs: &[&str]) -> Result<std::collections::HashMap<String, Vec<u8>>, String> {
    let mut map = std::collections::HashMap::new();
    if specs.is_empty() {
        return Ok(map);
    }
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to run git cat-file: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    // In a partial clone git fetches each file from the remote first, which can
    // take a minute with nothing on screen: say so if it's taking a while.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let n = specs.len();
    std::thread::spawn(move || {
        if done_rx.recv_timeout(std::time::Duration::from_secs(2)) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            eprintln!("perspica: reading {n} file versions from git is taking a while (a partial clone fetches them from the remote first)…");
        }
    });
    let _done = done_tx;
    let input: String = specs.iter().map(|s| format!("{s}\n")).collect();
    // Write on a thread so a large response can't deadlock against our reads.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let mut reader = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    for spec in specs {
        let mut header = String::new();
        if reader.read_line(&mut header).map_err(|e| e.to_string())? == 0 {
            break;
        }
        // "<oid> <type> <size>", or "<spec> missing" / "<spec> ambiguous", where
        // the spec itself may contain spaces, so parse from the right and validate.
        let mut parts = header.trim_end_matches('\n').rsplitn(3, ' ');
        let (size, kind, oid) = (parts.next(), parts.next(), parts.next());
        let is_object = matches!(kind, Some("blob" | "tree" | "commit" | "tag"))
            && oid.is_some_and(|o| o.len() >= 40 && o.bytes().all(|c| c.is_ascii_hexdigit()));
        if let (true, Some(Ok(size))) = (is_object, size.map(str::parse::<usize>)) {
            let mut buf = vec![0u8; size + 1]; // content + trailing newline
            reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
            buf.pop();
            map.insert(spec.to_string(), buf);
        }
    }
    let _ = writer.join();
    let _ = child.wait();
    Ok(map)
}

/// Unified diff between two files on disk (works outside a repo).
pub fn no_index_diff(old: &str, new: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["diff", "--no-index", "--no-color", "--no-ext-diff", "-U3", "--", old, new])
        .output()
        .ok()?;
    // Exit code 1 means "files differ" for --no-index.
    if output.status.code() == Some(0) || output.status.code() == Some(1) {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// PR metadata from the GitHub CLI.
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
}

/// Resolve a GitHub PR to a merge-base range, fetching refs as needed.
pub fn pr_target(number: u64) -> Result<(Target, PrInfo), String> {
    let n = number.to_string();
    let output = Command::new("gh")
        .args(["pr", "view", &n, "--json", "title,body,url,baseRefName,baseRefOid,headRefOid,mergeCommit"])
        // Plain JSON even when the user forces color (`CLICOLOR_FORCE` makes gh colorize it).
        .env_remove("CLICOLOR_FORCE")
        .env_remove("GH_FORCE_TTY")
        .env("NO_COLOR", "1")
        .output()
        .map_err(|e| format!("Failed to run gh (install the GitHub CLI): {e}"))?;
    if !output.status.success() {
        return Err(format!("gh pr view failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|e| format!("gh output: {e}"))?;
    let head = v["headRefOid"].as_str().ok_or("gh: missing headRefOid")?.to_string();
    let base_ref = v["baseRefName"].as_str().ok_or("gh: missing baseRefName")?.to_string();

    if !object_exists(&head) {
        eprintln!("Fetching PR #{number}…");
        git_cmd(&["fetch", "--quiet", "origin", &format!("pull/{number}/head")])?;
    }
    let remote_base = format!("origin/{base_ref}");
    let _ = git_cmd(&["fetch", "--quiet", "origin", &base_ref]);
    // Where the PR branched off. Once a PR is merged with a merge commit its head
    // is part of the base branch, so the merge-base with the branch *today* is
    // the head itself (an empty diff): use the base as it was just before the
    // merge, or GitHub's recorded base commit.
    let merge_parent = v["mergeCommit"]["oid"].as_str().map(|m| format!("{m}^1"));
    let recorded_base = v["baseRefOid"].as_str().map(str::to_string);
    let candidates: Vec<String> = [merge_parent, Some(remote_base), Some(base_ref.clone()), recorded_base].into_iter().flatten().collect();
    let base = candidates.iter()
        .filter_map(|c| git_cmd(&["merge-base", c, &head]).ok())
        .map(|b| b.trim().to_string())
        .find(|b| !b.is_empty() && *b != head)
        .ok_or_else(|| format!("couldn't find where PR #{number} branched off {base_ref}"))?;
    let info = PrInfo {
        number,
        title: v["title"].as_str().unwrap_or("").to_string(),
        body: v["body"].as_str().unwrap_or("").to_string(),
        url: v["url"].as_str().unwrap_or("").to_string(),
    };
    Ok((Target::Range(base.trim().to_string(), head), info))
}

fn object_exists(rev: &str) -> bool {
    Command::new("git").args(["cat-file", "-e", rev]).status().map(|s| s.success()).unwrap_or(false)
}

/// Commit subjects and bodies in a range: the author's own account of intent.
pub fn commit_messages(target: &Target) -> Option<String> {
    let range = match target {
        Target::Range(a, b) => format!("{a}..{b}"),
        Target::Branch { merge_base, .. } => format!("{merge_base}..HEAD"),
        _ => return None,
    };
    let log = git_cmd(&["log", "--no-merges", "--format=- %s%n%b", "-n", "50", &range]).ok()?;
    let log = log.lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join("\n");
    if log.is_empty() { None } else { Some(log) }
}

/// Word-matches of any of `names` across the whole repository at the target's
/// new side. Returns (name, path, line, text). One git process for all names.
pub fn grep_words(target: &Target, names: &[&str], limit: usize) -> Vec<(String, String, usize, String)> {
    if names.is_empty() {
        return vec![];
    }
    let mut args: Vec<String> = vec!["grep".into(), "-n".into(), "-w".into(), "-I".into(), "-F".into(), "--no-color".into()];
    let rev_prefix = match target {
        Target::Staged => { args.push("--cached".into()); None }
        Target::WorkingTree(_) | Target::Branch { .. } => { args.push("--untracked".into()); None }
        Target::Range(_, b) => Some(b.clone()),
    };
    for n in names {
        args.push("-e".into());
        args.push(n.to_string());
    }
    if let Some(rev) = &rev_prefix {
        args.push(rev.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let Ok(out) = git_cmd(&arg_refs) else { return vec![] };
    let mut hits = Vec::new();
    for line in out.lines() {
        let rest = match &rev_prefix {
            Some(rev) => match line.strip_prefix(&format!("{rev}:")) { Some(r) => r, None => continue },
            None => line,
        };
        let mut parts = rest.splitn(3, ':');
        let (Some(path), Some(ln), Some(text)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Ok(ln) = ln.parse::<usize>() else { continue };
        for n in names {
            if perspica_core::classify::contains_identifier(text, n) {
                hits.push((n.to_string(), path.to_string(), ln, text.trim().chars().take(160).collect()));
            }
        }
        if hits.len() >= limit {
            break;
        }
    }
    hits
}

/// A file's contents on the new side of `target`.
pub fn read_new(target: &Target, path: &str) -> Option<String> {
    match target {
        Target::Range(_, b) => git_cmd(&["show", &format!("{b}:{path}")]).ok(),
        Target::Staged => git_cmd(&["show", &format!(":{path}")]).ok(),
        Target::WorkingTree(_) | Target::Branch { .. } => std::fs::read_to_string(repo_root()?.join(path)).ok(),
    }
}

/// Parse a unified diff string into DiffHunks that match our data model.
pub fn parse_unified_diff(raw: &str) -> Vec<perspica_core::manifest::DiffHunk> {
    use perspica_core::manifest::{Change, ChangeKind, DiffHunk, LineRange, Span};

    let mut hunks = Vec::new();
    let mut current_changes: Vec<Change> = Vec::new();
    let mut old_line: usize = 0;
    let mut new_line: usize = 0;
    let (mut hunk_old_start, mut hunk_old_end, mut hunk_new_start, mut hunk_new_end) = (0, 0, 0, 0);
    let mut in_hunk = false;

    let flush = |changes: &mut Vec<Change>, hunks: &mut Vec<DiffHunk>, os, oe, ns, ne| {
        if !changes.is_empty() {
            hunks.push(DiffHunk {
                old_range: LineRange { start: os, end: oe },
                new_range: LineRange { start: ns, end: ne },
                changes: std::mem::take(changes),
                manifest_refs: vec![],
                noise: None,
                test: false,
            });
        }
    };

    for line in raw.lines() {
        // A new section (a type change has two); its `---`/`+++` header lines aren't content.
        if line.starts_with("diff --") {
            if in_hunk {
                flush(&mut current_changes, &mut hunks, hunk_old_start, hunk_old_end, hunk_new_start, hunk_new_end);
            }
            in_hunk = false;
        } else if line.starts_with("@@") {
            if in_hunk {
                flush(&mut current_changes, &mut hunks, hunk_old_start, hunk_old_end, hunk_new_start, hunk_new_end);
            }
            if let Some((os, ns)) = parse_hunk_header(line) {
                old_line = os;
                new_line = ns;
                hunk_old_start = os;
                hunk_new_start = ns;
                hunk_old_end = os;
                hunk_new_end = ns;
                in_hunk = true;
            }
        } else if in_hunk {
            let span = |n: usize, len: usize| Some(Span { start_line: n, start_col: 0, end_line: n, end_col: len });
            if let Some(content) = line.strip_prefix('+') {
                current_changes.push(Change { kind: ChangeKind::Added, old_span: None, new_span: span(new_line, content.len()), content_old: None, content_new: Some(content.to_string()), noise: None });
                hunk_new_end = new_line;
                new_line += 1;
            } else if let Some(content) = line.strip_prefix('-') {
                current_changes.push(Change { kind: ChangeKind::Removed, old_span: span(old_line, content.len()), new_span: None, content_old: Some(content.to_string()), content_new: None, noise: None });
                hunk_old_end = old_line;
                old_line += 1;
            } else if line.starts_with(' ') || line.is_empty() {
                let content = line.strip_prefix(' ').unwrap_or("");
                current_changes.push(Change { kind: ChangeKind::Context, old_span: span(old_line, content.len()), new_span: span(new_line, content.len()), content_old: Some(content.to_string()), content_new: Some(content.to_string()), noise: None });
                hunk_old_end = old_line;
                hunk_new_end = new_line;
                old_line += 1;
                new_line += 1;
            }
            // "\ No newline at end of file": skip
        }
    }
    if in_hunk {
        flush(&mut current_changes, &mut hunks, hunk_old_start, hunk_old_end, hunk_new_start, hunk_new_end);
    }
    hunks
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    // "@@ -old_start,old_count +new_start,new_count @@"
    let line = line.strip_prefix("@@ ")?;
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() < 2 { return None; }
    let old_start: usize = parts[0].strip_prefix('-')?.split(',').next()?.parse().ok()?;
    let new_start: usize = parts[1].strip_prefix('+')?.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

fn git_cmd(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("fatal") || stderr.contains("error") {
            return Err(format!("git {}: {}", args.first().unwrap_or(&""), stderr.trim()));
        }
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_z_handles_renames() {
        let e = parse_name_status_z("M\0a.rs\0R087\0old.ts\0new.ts\0A\0b c.py\0");
        assert_eq!(e, vec![
            ('M', "a.rs".into(), "a.rs".into()),
            ('R', "old.ts".into(), "new.ts".into()),
            ('A', "b c.py".into(), "b c.py".into()),
        ]);
    }

    #[test]
    fn remote_names() {
        let n = |u: &str| parse_remote(u).map(|(name, url)| (name, url.unwrap_or_default()));
        assert_eq!(n("git@github.com:pallets/flask.git"), Some(("pallets/flask".into(), "https://github.com/pallets/flask".into())));
        assert_eq!(n("https://github.com/sindresorhus/ky"), Some(("sindresorhus/ky".into(), "https://github.com/sindresorhus/ky".into())));
        assert_eq!(n("ssh://git@gitlab.example.com:2222/group/sub/proj.git"), Some(("sub/proj".into(), "https://gitlab.example.com/group/sub/proj".into())));
        assert_eq!(n("https://user:token@github.com/a/b.git/"), Some(("a/b".into(), "https://github.com/a/b".into())));
        assert_eq!(n("/local/path"), None);
    }

    #[test]
    fn split_patches_by_file() {
        let p = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/y b/y\n@@ -1 +1 @@\n-c\n+d\n";
        let s = split_patches(p);
        assert_eq!(s.len(), 2);
        assert!(patch_for(&s, 'M', "y", "y").contains("+d"));
        assert!(!patch_for(&s, 'M', "x", "x").contains("+d"));
    }

    #[test]
    fn typechange_sections_stay_with_their_file() {
        // A symlink replaced by a regular file: two sections for `link`, then `z.txt`.
        let p = "diff --git a/link b/link\ndeleted file mode 120000\n@@ -1 +0,0 @@\n-target\n\
                 diff --git a/link b/link\nnew file mode 100644\n--- /dev/null\n+++ b/link\n@@ -0,0 +1 @@\n+content\n\
                 diff --git a/z.txt b/z.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let s = split_patches(p);
        let link = patch_for(&s, 'T', "link", "link");
        assert!(link.contains("-target") && link.contains("+content"));
        let z = patch_for(&s, 'M', "z.txt", "z.txt");
        assert!(z.contains("+new") && !z.contains("content"));
        let hunks = parse_unified_diff(&link);
        let lines: Vec<String> = hunks.iter().flat_map(|h| &h.changes)
            .map(|c| c.content_new.clone().or(c.content_old.clone()).unwrap_or_default())
            .collect();
        assert_eq!(lines, vec!["target", "content"]);
    }

    #[test]
    fn quoted_and_unmerged_headers() {
        let p = "diff --git \"a/q\\\"x\" \"b/q\\\"x\"\n@@ -1 +1 @@\n-a\n+b\ndiff --cc c.rs\n@@@ -1,1 -1,1 +1,1 @@@\n";
        let s = split_patches(p);
        assert!(patch_for(&s, 'M', "q\"x", "q\"x").contains("+b"));
        assert!(patch_for(&s, 'U', "c.rs", "c.rs").contains("@@@"));
    }

    #[test]
    fn cat_file_handles_missing_spec_with_spaces() {
        let dir = std::env::temp_dir().join(format!("perspica-catfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git").args(args).current_dir(&dir).output().unwrap().status.success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
        let blobs = cat_file_batch_in(Some(&dir), &["HEAD:my file.rs", "HEAD:a.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = blobs.unwrap();
        assert!(!blobs.contains_key("HEAD:my file.rs"));
        assert_eq!(blobs.get("HEAD:a.txt").map(|b| b.as_slice()), Some(&b"hello\n"[..]));
    }
}
