use serde::{Deserialize, Serialize};

pub type ManifestEntryId = u32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start_line: usize, // 1-indexed
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineRange {
    pub start: usize, // 1-indexed
    pub end: usize,
}

/// Which version of the file a location refers to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Old,
    #[default]
    New,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub file: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(default)]
    pub side: Side,
}

impl Location {
    pub fn new(line_start: usize, line_end: usize) -> Self {
        Location { file: None, line_start, line_end, side: Side::New }
    }
    pub fn old(line_start: usize, line_end: usize) -> Self {
        Location { file: None, line_start, line_end, side: Side::Old }
    }
    pub fn contains(&self, side: Side, line: usize) -> bool {
        self.side == side && line >= self.line_start && line <= self.line_end
    }
}

/// Why a changed line (or a whole hunk) needs little reviewer attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Noise {
    /// Whitespace, blank lines, or reformatting: same tokens.
    Formatting,
    /// Only comments changed.
    Comment,
    /// The line differs only by a detected rename.
    Rename,
    /// The line was moved elsewhere unchanged.
    Moved,
    /// The file is generated (lockfiles, minified bundles, snapshots).
    Generated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    Moved,
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Variable,
    Type,
    Module,
    Class,
    Method,
    Import,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyChangeType {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub kind: ChangeKind,
    pub old_span: Option<Span>,
    pub new_span: Option<Span>,
    pub content_old: Option<String>,
    pub content_new: Option<String>,
    /// Set when this added/removed line is mechanical noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise: Option<Noise>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_range: LineRange,
    pub new_range: LineRange,
    pub changes: Vec<Change>,
    /// Manifest entries whose locations overlap this hunk's changed lines.
    pub manifest_refs: Vec<ManifestEntryId>,
    /// Set when every added/removed line in the hunk is mechanical noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise: Option<Noise>,
    /// Every changed line is test code (a test file, or tests inside a source file).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test: bool,
}

// --- Manifest entry types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameEntry {
    pub id: ManifestEntryId,
    pub old_name: String,
    pub new_name: String,
    pub kind: SymbolKind,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureChangeEntry {
    pub id: ManifestEntryId,
    pub name: String,
    pub kind: SymbolKind,
    pub description: String,
    pub location: Location,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionEntry {
    pub id: ManifestEntryId,
    pub original_name: String,
    pub extracted_names: Vec<String>,
    pub location_original: Location,
    pub locations_new: Vec<Location>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveEntry {
    pub id: ManifestEntryId,
    pub name: String,
    pub kind: SymbolKind,
    pub from_location: Location,
    pub to_location: Location,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyChange {
    pub id: ManifestEntryId,
    pub change_type: DependencyChangeType,
    pub name: String,
    pub used_in: Vec<Location>,
    /// Symbols added to / removed from an existing import (for `changed`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols_added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols_removed: Vec<String>,
    /// Relative / crate-internal import rather than an external package.
    #[serde(default)]
    pub internal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadCodeEntry {
    pub id: ManifestEntryId,
    pub name: String,
    pub kind: SymbolKind,
    pub location: Location,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogicChangeEntry {
    pub id: ManifestEntryId,
    pub name: String,
    pub kind: SymbolKind,
    pub description: String,
    pub location: Location,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormattingEntry {
    pub id: ManifestEntryId,
    pub location: Location,
    pub description: String,
}

/// The structured table of contents for all classified changes.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ChangeManifest {
    pub renames: Vec<RenameEntry>,
    pub signature_changes: Vec<SignatureChangeEntry>,
    pub extracted_functions: Vec<ExtractionEntry>,
    pub moved_code: Vec<MoveEntry>,
    pub dependency_changes: Vec<DependencyChange>,
    pub dead_code: Vec<DeadCodeEntry>,
    pub logic_changes: Vec<LogicChangeEntry>,
    pub formatting_only: Vec<FormattingEntry>,
    /// Entries that are changes to test code.
    #[serde(default)]
    pub test_entries: Vec<ManifestEntryId>,
}

impl ChangeManifest {
    /// Every entry id in this manifest.
    pub fn ids(&self) -> Vec<ManifestEntryId> {
        let mut ids = Vec::new();
        ids.extend(self.renames.iter().map(|e| e.id));
        ids.extend(self.signature_changes.iter().map(|e| e.id));
        ids.extend(self.extracted_functions.iter().map(|e| e.id));
        ids.extend(self.moved_code.iter().map(|e| e.id));
        ids.extend(self.dependency_changes.iter().map(|e| e.id));
        ids.extend(self.dead_code.iter().map(|e| e.id));
        ids.extend(self.logic_changes.iter().map(|e| e.id));
        ids.extend(self.formatting_only.iter().map(|e| e.id));
        ids
    }

    /// (id, location) for every entry, including every location of multi-location entries.
    pub fn locations(&self) -> Vec<(ManifestEntryId, &Location)> {
        let mut out = Vec::new();
        for e in &self.renames { for l in &e.locations { out.push((e.id, l)); } }
        for e in &self.signature_changes { out.push((e.id, &e.location)); }
        for e in &self.extracted_functions {
            out.push((e.id, &e.location_original));
            for l in &e.locations_new { out.push((e.id, l)); }
        }
        for e in &self.moved_code { out.push((e.id, &e.from_location)); out.push((e.id, &e.to_location)); }
        for e in &self.dependency_changes { for l in &e.used_in { out.push((e.id, l)); } }
        for e in &self.dead_code { out.push((e.id, &e.location)); }
        for e in &self.logic_changes { out.push((e.id, &e.location)); }
        for e in &self.formatting_only { out.push((e.id, &e.location)); }
        out
    }

    /// Mutable access to every location, used to stamp file paths.
    pub fn locations_mut(&mut self) -> Vec<&mut Location> {
        let mut out: Vec<&mut Location> = Vec::new();
        for e in &mut self.renames { out.extend(e.locations.iter_mut()); }
        for e in &mut self.signature_changes { out.push(&mut e.location); }
        for e in &mut self.extracted_functions {
            out.push(&mut e.location_original);
            out.extend(e.locations_new.iter_mut());
        }
        for e in &mut self.moved_code { out.push(&mut e.from_location); out.push(&mut e.to_location); }
        for e in &mut self.dependency_changes { out.extend(e.used_in.iter_mut()); }
        for e in &mut self.dead_code { out.push(&mut e.location); }
        for e in &mut self.logic_changes { out.push(&mut e.location); }
        for e in &mut self.formatting_only { out.push(&mut e.location); }
        out
    }

    /// Re-number every entry sequentially starting at `start`. Returns the next free id.
    pub fn renumber(&mut self, start: ManifestEntryId) -> ManifestEntryId {
        let mut id = start;
        let mut next = || { let v = id; id += 1; v };
        for e in &mut self.renames { e.id = next(); }
        for e in &mut self.signature_changes { e.id = next(); }
        for e in &mut self.extracted_functions { e.id = next(); }
        for e in &mut self.moved_code { e.id = next(); }
        for e in &mut self.dependency_changes { e.id = next(); }
        for e in &mut self.dead_code { e.id = next(); }
        for e in &mut self.logic_changes { e.id = next(); }
        for e in &mut self.formatting_only { e.id = next(); }
        id
    }
}
