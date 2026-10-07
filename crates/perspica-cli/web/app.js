'use strict';

// --- State ---
const store = {
    get(key, fallback) {
        try { const v = localStorage.getItem('perspica:' + key); return v === null ? fallback : JSON.parse(v); } catch { return fallback; }
    },
    set(key, value) {
        try { localStorage.setItem('perspica:' + key, JSON.stringify(value)); } catch {}
    },
};

const S = {
    data: null,
    mode: store.get('mode', null),             // 'files' | 'intent' | 'flow'; null: pick from the data
    view: store.get('view', 'unified'),        // 'unified' | 'split'
    hideNoise: store.get('hideNoise', true),
    wrap: store.get('wrap', false),
    tab: 'outline',        // sidebar tab: 'outline' (maps the current review order) | 'changes' (flat index)
    overviewCollapsed: store.get('overviewCollapsed', false),
    onlyUnviewed: false,   // sidebar shows only what isn't marked viewed
    viewed: {},            // path → signature (persisted per source)
    groupsReviewed: {},    // group key → true (persisted per source)
    expandedNoise: new Set(), // "fi:hi" hunks the user opened despite hideNoise
    collapsedSections: new Set(),
    currentHunk: -1,
    running: false,
    checked: {},
    changedSinceViewed: {}, // path → true: marked viewed earlier, changed since (persisted per source)           // "check before merging" items marked done (persisted per source)
    checkOpen: false,
    askedOpen: false,
};

// Derived indexes (rebuilt by derive()).
let E = new Map();            // entry id → entry
let nameIndex = null;         // escaped item name → entry id, for links in model text
let instantScroll = false;    // set while a keyboard shortcut (or a restore) moves the view

// Smooth scrolls only run in a visible tab; a hidden one would never get there.
function scrollBehavior() { return instantScroll || document.visibilityState === 'hidden' ? 'auto' : 'smooth'; }
let entryHunks = new Map();   // entry id → [{fi, hi}]
let testIds = new Set();      // entry ids that are changes to test code
let fileByPath = new Map();   // path → fi
let groups = [];              // review groups for "By intent" / "Reading order"
let hunkGroup = new Map();    // "fi:hi" → group index
let order = [];               // display order of hunk keys (for j/k)
let sectionObserver = null, hunkObserver = null;
let renderGeneration = 0;
/** Changed lines rendered eagerly in the background before switching to lazy-only. */
const BACKGROUND_RENDER_LINES = 20000;
const hlCache = new Map();    // fi → {old: [], new: []} | null

const KIND = {
    rename:     { ic: '↔', label: 'Rename', one: 'rename', many: 'renames', cat: 'structure' },
    signature:  { ic: 'σ', label: 'Signature', one: 'signature change', many: 'signature changes', cat: 'api' },
    dependency: { ic: '±', label: 'Dependency', one: 'import change', many: 'import changes', cat: 'deps' },
    extraction: { ic: '⊕', label: 'Extraction', one: 'extraction', many: 'extractions', cat: 'structure' },
    dead_code:  { ic: '∅', label: 'Dead code', one: 'dead item', many: 'dead items', cat: 'attention' },
    logic:      { ic: 'Δ', label: 'Logic', one: 'logic change', many: 'logic changes', cat: 'logic' },
    moved:      { ic: '⇄', label: 'Moved', one: 'unchanged move', many: 'unchanged moves', cat: 'mechanical' },
    formatting: { ic: '~', label: 'Formatting', one: 'formatting-only item', many: 'formatting-only items', cat: 'mechanical' },
    cross_move: { ic: '⇄', label: 'Moved across files', one: 'cross-file move', many: 'cross-file moves', cat: 'structure' },
    broken:     { ic: '✗', label: 'Stale reference', one: 'stale reference', many: 'stale references', cat: 'attention' },
    call_sites: { ic: '!', label: 'Call sites not updated', one: 'stale call site group', many: 'stale call site groups', cat: 'attention' },
    swapped:    { ic: '⇢', label: 'Import source changed', one: 'import from a new source', many: 'imports from a new source', cat: 'attention' },
};
const CATEGORIES = [
    { key: 'attention', label: 'Possible breakage', note: 'Found in the code: stale references, calls not updated, imports from a new source, code left unused.' },
    { key: 'api', label: 'API & signature changes', note: 'Callers depend on these. Check compatibility.' },
    { key: 'deps', label: 'Dependencies & imports' },
    { key: 'logic', label: 'Logic changes' },
    { key: 'structure', label: 'Renames, moves & extractions', note: 'Structural edits. Mostly verify nothing else changed.' },
    { key: 'tests', label: 'Tests', note: 'Test code. Check it asserts the new behavior.' },
    { key: 'mechanical', label: 'Mechanical changes', note: 'Formatting, comments, renames and unchanged moves.', mechanical: true },
];
const MECH_NOTE = 'Formatting, comments, renames and unchanged moves.';
const ROLE_TAG = { test: 'test', docs: 'docs', vendored: 'vendored' };
/** Category of an entry for "By category" and the Changes tab. */
function entryCat(e) { return e.mechanical ? 'mechanical' : testIds.has(e.id) ? 'tests' : KIND[e.kind].cat; }
function grouped() { return S.mode !== 'files'; }
/** What the sidebar shows: the file tree, the current order's groups, or the change index. */
function sidebarShows() { return S.tab === 'changes' ? 'changes' : S.mode === 'files' ? 'files' : 'groups'; }
const MODE_NAME = { files: 'By file', flow: 'Reading order' };
function modeName() { return MODE_NAME[S.mode] || (hasLlmGroups() ? 'By intent' : 'By category'); }
const NOISE_LABEL = { formatting: 'formatting only', comment: 'comments only', rename: 'rename only', moved: 'moved, unchanged', generated: 'generated file' };

// --- Init ---
async function init() {
    applyBodyClasses();
    // A saved page carries its data. Otherwise ask the local server.
    const embedded = document.getElementById('perspica-data');
    S.static = !!embedded;
    try {
        S.data = embedded ? JSON.parse(embedded.textContent) : await (await fetch('/api/diff')).json();
    } catch (e) {
        document.getElementById('diff-scroll').innerHTML =
            `<div class="section"><div class="section-note">Could not load the diff (${esc(String(e))}). Is perspica still running?</div></div>`;
        return;
    }
    const key = sourceKey();
    S.viewed = store.get('viewed:' + key, {});
    S.groupsReviewed = store.get('groups:' + key, {});
    S.checked = store.get('checked:' + key, {});
    S.changedSinceViewed = store.get('changed:' + key, {});
    derive();
    // Drop "viewed" marks for files whose diff changed since they were marked.
    for (const [path, sig] of Object.entries(S.viewed)) {
        const fi = fileByPath.get(path);
        if (fi === undefined || fileSignature(fi) !== sig) {
            delete S.viewed[path];
            if (fi !== undefined) S.changedSinceViewed[path] = true;
        }
    }
    store.set('viewed:' + key, S.viewed);
    store.set('changed:' + key, S.changedSinceViewed);
    // A link or reload names the view and the change to show (see syncUrl).
    const hash = new URLSearchParams(location.hash.slice(1));
    if (['files', 'intent', 'flow'].includes(hash.get('view'))) { S.mode = hash.get('view'); buildGroups(); }
    // First visit: open on what perspica adds over a plain diff, not the alphabetical file list.
    if (!S.mode) { S.mode = hasLlmGroups() ? 'intent' : hasFlow() ? 'flow' : 'intent'; buildGroups(); }
    if (grouped() && !groups.length) { S.mode = 'files'; buildGroups(); }

    // Large diffs: start with generated/binary files collapsed.
    S.data.files.forEach((f, fi) => {
        const r = S.data.results[fi];
        if (f.binary || r.review.generated || S.viewed[f.new_path]) S.collapsedSections.add('f' + fi);
    });

    renderHeader();
    renderOverview();
    renderSidebar();
    renderMain();
    setupListeners();
    setupSidebarResize();
    restoreFromUrl(hash.get('at'));
    // Highlight.js loads deferred; re-render visible code once it arrives.
    if (typeof hljs === 'undefined') {
        window.addEventListener('load', () => { if (typeof hljs !== 'undefined') { hlCache.clear(); renderMain(true); } }, { once: true });
    }
}

/** `at=path:line`: go to the change there. */
function restoreFromUrl(at) {
    const m = at && at.match(/^(.*):(\d+)$/);
    if (!m) return;
    const fi = fileByPath.get(m[1]);
    if (fi === undefined) return;
    let hi = findHunkAt(fi, +m[2], 'new');
    if (hi < 0) hi = findHunkAt(fi, +m[2], 'old');
    if (hi < 0) return;
    instantScroll = true;
    try { revealHunk(fi, hi, null, false); } finally { instantScroll = false; }
    S.currentHunk = order.indexOf(fi + ':' + hi);
    updateNavCounter();
}

/** Keep the view and the current change in the URL, so reloads and shared links land there. */
let urlTimer = 0;
function syncUrl() {
    clearTimeout(urlTimer);
    urlTimer = setTimeout(() => {
        const k = order[S.currentHunk];
        let hash = `view=${S.mode}`;
        if (k) {
            const [fi, hi] = k.split(':').map(Number);
            const h = S.data.results[fi].hunks[hi];
            const line = h.new_range.start || h.old_range.start;
            hash += `&at=${encodeURIComponent(S.data.files[fi].new_path).replace(/%2F/g, '/')}:${line}`;
        }
        if (location.hash.slice(1) !== hash) history.replaceState(null, '', '#' + hash);
    }, 250);
}

function sourceKey() {
    const d = S.data;
    return (d.source?.label || '') + '|' + d.files.map(f => f.new_path).join(',').slice(0, 400);
}

// --- Derivation: entries, links, groups ---
function derive() {
    const d = S.data;
    E = new Map(); entryHunks = new Map(); fileByPath = new Map(); nameIndex = null;
    testIds = new Set(d.results.flatMap(r => r.manifest.test_entries || []));
    d.files.forEach((f, fi) => { fileByPath.set(f.new_path, fi); if (f.old_path !== f.new_path) fileByPath.set(f.old_path, fi); });

    const add = (e) => { E.set(e.id, e); };
    const at = (loc) => loc ? { fi: loc.file != null && fileByPath.has(loc.file) ? fileByPath.get(loc.file) : -1, line: loc.line_start, side: loc.side || 'new' } : { fi: -1 };

    d.results.forEach((r, fi) => {
        const m = r.manifest;
        const path = d.files[fi].new_path;
        const base = (kind, id, text, loc, extra = {}) => add({ id, kind, text, fi, line: loc?.line_start, side: loc?.side || 'new', path, ...extra });
        for (const x of m.renames) base('rename', x.id, `${x.old_name} → ${x.new_name}`, x.locations[0]);
        for (const x of m.signature_changes) base('signature', x.id, `${x.name}: ${x.description}`, x.location, { short: x.name });
        for (const x of m.dependency_changes) {
            const sym = x.change_type === 'added' ? '+' : x.change_type === 'removed' ? '−' : '~';
            let t = `${sym} ${x.name}`;
            if (x.change_type === 'changed') {
                const parts = [];
                if (x.symbols_added?.length) parts.push('+' + x.symbols_added.join(', +'));
                if (x.symbols_removed?.length) parts.push('−' + x.symbols_removed.join(', −'));
                if (parts.length) t += ` (${parts.join('; ')})`;
            }
            base('dependency', x.id, t + (x.internal ? '  · internal' : ''), x.used_in[0], { internal: x.internal });
        }
        for (const x of m.extracted_functions) base('extraction', x.id, `${x.original_name} → ${x.extracted_names.join(' + ')}`, x.location_original);
        for (const x of m.dead_code) base('dead_code', x.id, `${x.name}: ${x.reason}`, x.location, { short: x.name });
        for (const x of m.logic_changes) base('logic', x.id, `${x.name}: ${x.description}`, x.location, { short: `${x.name}${x.description === 'added' ? ' (new)' : x.description === 'removed' ? ' (removed)' : ''}` });
        for (const x of m.moved_code) base('moved', x.id, `${x.name} moved within file`, x.to_location, { short: x.name });
        for (const x of m.formatting_only) base('formatting', x.id, x.description, x.location);
    });
    const cf = d.cross_file || {};
    for (const x of cf.moves || []) {
        const t = at(x.to_location);
        let text = `${x.name}: ${shortPath(x.from_file)} → ${shortPath(x.to_file)}`;
        if (x.renamed_to) text += ` as ${x.renamed_to}`;
        if (x.modified) text += ' (edited)';
        add({ id: x.id, kind: 'cross_move', text, ...t, path: x.to_file, mechanical: !x.modified && !x.renamed_to });
    }
    for (const x of cf.broken_references || []) {
        add({ id: x.id, kind: 'broken', text: x.reason, ...at(x.reference_location), path: x.reference_file, code: x.line_text, inDiff: x.in_diff });
    }
    for (const x of cf.swapped_imports || []) {
        add({ id: x.id, kind: 'swapped', text: x.reason, ...at(x.location), path: x.file, sites: x.uses.map(u => ({ file: x.file, line: u.line, text: u.text })) });
    }
    for (const x of cf.signature_impacts || []) {
        const stale = x.call_sites.filter(c => !c.updated);
        if (!stale.length) continue;
        add({ id: x.id, kind: 'call_sites', text: `${x.name}: ${stale.length} call site${pl(stale.length)} not updated`, ...at(x.definition), path: x.definition.file, sites: stale, description: x.description });
    }

    // Entry → hunks, from the engine's links.
    d.results.forEach((r, fi) => r.hunks.forEach((h, hi) => {
        for (const id of h.manifest_refs || []) {
            if (!entryHunks.has(id)) entryHunks.set(id, []);
            entryHunks.get(id).push({ fi, hi });
        }
    }));
    // Cross-file entries: link by location.
    for (const e of E.values()) {
        if (entryHunks.has(e.id) || e.fi < 0) continue;
        const hi = findHunkAt(e.fi, e.line, e.side);
        if (hi >= 0) entryHunks.set(e.id, [{ fi: e.fi, hi }]);
    }

    buildGroups();
}

function findHunkAt(fi, line, side) {
    const hunks = S.data.results[fi]?.hunks || [];
    const key = side === 'old' ? 'old_range' : 'new_range';
    return hunks.findIndex(h => line >= h[key].start && line <= Math.max(h[key].end, h[key].start));
}

function buildGroups() {
    if (S.mode === 'flow') buildFlowGroups(); else buildIntentGroups();
}

/** "Reading order": types first, then one section per changed function in call order. */
function buildFlowGroups() {
    const d = S.data, cf = d.cross_file || {};
    groups = [];
    const steps = cf.reading_order || [];
    const reach = new Map((cf.test_reach || []).map(t => [t.file + '\0' + t.name, t]));
    const anyTests = d.results.some(r => r.review.test_lines > 0);
    const types = steps.filter(s => s.kind === 'type');
    if (types.length) groups.push({ key: 'flow:types', label: 'Types & data', note: `${types.length} changed type${pl(types.length)} and data definition${pl(types.length)} the code works with. Read these first.`, ids: types.flatMap(s => s.entry_ids), subs: [] });
    for (const s of steps) {
        if (s.kind !== 'function' || s.repeat) continue;
        groups.push({ key: `flow:${s.file}:${s.name}`, label: s.name, ids: s.entry_ids, subs: [], flow: s, reach: reach.get(s.file + '\0' + s.name), anyTests });
    }
    const others = steps.filter(s => s.kind === 'other');
    if (others.length) groups.push({ key: 'flow:other', label: 'Other changed code', note: 'Constants, variables and top-level statements.', ids: others.flatMap(s => s.entry_ids), subs: [] });

    const idGroup = new Map();
    groups.forEach((g, gi) => g.ids.forEach(id => { if (!idGroup.has(id)) idGroup.set(id, gi); }));
    const extra = {};
    const extraGroup = (key, label, note, mechanical) => {
        if (extra[key] === undefined) { groups.push({ key: 'flow:' + key, label, note, mechanical, ids: [], subs: [] }); extra[key] = groups.length - 1; }
        return extra[key];
    };
    const pending = [];
    hunkGroup = new Map();
    d.results.forEach((r, fi) => r.hunks.forEach((h, hi) => {
        // First in reading order wins: a hunk is read where its code is first reached.
        // Types & data only takes hunks that touch no changed function.
        let best = -1;
        const rank = (gi) => groups[gi].flow ? gi : gi + groups.length;
        for (const id of h.manifest_refs || []) { const gi = idGroup.get(id); if (gi !== undefined && (best < 0 || rank(gi) < rank(best))) best = gi; }
        if (best >= 0) hunkGroup.set(fi + ':' + hi, best); else pending.push({ fi, hi, h, r });
    }));
    // Everything the call flows don't cover, in attention order.
    const bucket = ({ h, r }) => h.noise || r.review.generated ? 'mech' : h.test || r.review.role === 'test' ? 'tests' : r.review.role === 'docs' ? 'docs' : 'other';
    for (const b of ['other', 'tests', 'docs', 'mech']) {
        for (const p of pending.filter(p => bucket(p) === b)) {
            const gi = b === 'other' ? extraGroup('other-changes', 'Other changes', 'Changes outside the call flows above (config, unsupported files, top-level code).')
                : b === 'tests' ? extraGroup('tests', 'Tests', 'Test code. Check it asserts the new behavior.')
                : b === 'docs' ? extraGroup('docs', 'Docs', 'Documentation changes.')
                : extraGroup('mech', 'Mechanical changes', MECH_NOTE, true);
            hunkGroup.set(p.fi + ':' + p.hi, gi);
        }
    }
    // Test entries belong with the test hunks.
    if (extra.tests !== undefined) groups[extra.tests].ids = [...testIds];
}

function buildIntentGroups() {
    const d = S.data;
    groups = [];
    const llm = Array.isArray(d.intent_groups) && d.intent_groups.length > 0;
    if (llm) {
        d.intent_groups.forEach((g, gi) => groups.push({
            key: 'llm:' + g.label, label: g.label, risk: g.risk, note: g.review_note, mechanical: !!g.mechanical,
            ids: g.entry_ids, subs: g.sub_groups || [], llm: true, origin: g.origin, requirement: g.requirement,
        }));
    } else {
        for (const c of CATEGORIES) {
            const ids = [...E.values()].filter(e => entryCat(e) === c.key).map(e => e.id);
            if (ids.length) groups.push({ key: 'cat:' + c.key, label: c.label, note: c.note, mechanical: !!c.mechanical, ids, subs: [], llm: false });
        }
    }
    // Every hunk goes to exactly one group: the one holding most of its entries.
    const idGroup = new Map();
    groups.forEach((g, gi) => g.ids.forEach(id => { if (!idGroup.has(id)) idGroup.set(id, gi); }));
    hunkGroup = new Map();
    let unclassified = -1, mechanical = groups.findIndex(g => g.mechanical), tests = groups.findIndex(g => g.key === 'cat:tests');
    d.results.forEach((r, fi) => r.hunks.forEach((h, hi) => {
        const votes = new Map();
        for (const id of h.manifest_refs || []) {
            const gi = idGroup.get(id);
            if (gi !== undefined) votes.set(gi, (votes.get(gi) || 0) + 1);
        }
        let best = -1, bestN = 0;
        if (llm) {
            for (const [gi, n] of votes) if (n > bestN || (n === bestN && gi < best)) { best = gi; bestN = n; }
        } else {
            // Categories are ordered by attention: the most important one wins.
            for (const gi of votes.keys()) if (best < 0 || gi < best) best = gi;
        }
        if (best < 0) {
            if (h.noise || r.review.generated) {
                if (mechanical < 0) { groups.push({ key: 'cat:mechanical', label: 'Mechanical changes', note: MECH_NOTE, mechanical: true, ids: [], subs: [] }); mechanical = groups.length - 1; }
                best = mechanical;
            } else if (!llm && (h.test || r.review.role === 'test')) {
                if (tests < 0) { groups.push({ key: 'cat:tests', label: 'Tests', note: 'Test code. Check it asserts the new behavior.', ids: [], subs: [] }); tests = groups.length - 1; }
                best = tests;
            } else {
                if (unclassified < 0) { groups.push({ key: 'cat:other', label: 'Other changes', note: 'Changes outside recognized items (unsupported files, top-level code, config).', ids: [], subs: [] }); unclassified = groups.length - 1; }
                best = unclassified;
            }
        }
        hunkGroup.set(fi + ':' + hi, best);
    }));
    // Keep mechanical last.
    if (mechanical >= 0 && mechanical !== groups.length - 1) {
        const [g] = groups.splice(mechanical, 1);
        groups.push(g);
        for (const [k, gi] of hunkGroup) hunkGroup.set(k, gi === mechanical ? groups.length - 1 : gi > mechanical ? gi - 1 : gi);
    }
}

/** Lines added and removed across some hunks, shown like GitHub's `+12 −3`. */
function lineStats(list) {
    let add = 0, del = 0;
    for (const { fi, hi } of list) for (const c of S.data.results[fi].hunks[hi].changes) { if (c.kind === 'added') add++; else if (c.kind === 'removed') del++; }
    return { add, del };
}
const statsHtml = ({ add, del }) => add || del ? `<span class="stats"><span class="a">+${add}</span><span class="d">−${del}</span></span>` : '';

function groupHunks(gi) {
    const out = [];
    for (const [k, g] of hunkGroup) if (g === gi) { const [fi, hi] = k.split(':').map(Number); out.push({ fi, hi }); }
    out.sort((a, b) => a.fi - b.fi || a.hi - b.hi);
    return out;
}

// --- Header & overview ---
function renderHeader() {
    const d = S.data;
    const title = document.getElementById('source-title');
    const label = d.source?.label || (d.files.length === 1 ? 'File comparison' : 'Diff');
    let sub = '';
    if (d.source?.pr_title) {
        sub = d.source.pr_url ? `<a class="sub" href="${esc(d.source.pr_url)}" target="_blank" rel="noopener">${esc(d.source.pr_title)}</a>` : `<span class="sub">${esc(d.source.pr_title)}</span>`;
    } else if (d.files.length === 1) {
        sub = `<span class="sub">${esc(d.files[0].old_path)} → ${esc(d.files[0].new_path)}</span>`;
    }
    // Which repository, first: several viewers can be open at once.
    const repo = d.source?.repo;
    const repoHtml = repo ? (d.source.repo_url
        ? `<a class="repo" href="${esc(d.source.repo_url)}" target="_blank" rel="noopener">${esc(repo)}</a>`
        : `<span class="repo">${esc(repo)}</span>`) : '';
    title.innerHTML = `${repoHtml}<span class="label">${esc(label)}</span>${sub}`;
    title.title = d.source?.detail || [label, d.source?.pr_title].filter(Boolean).join('\n');
    document.title = `${repo ? repo.split('/').pop() + ' ' : ''}${label} · perspica`;

    document.querySelectorAll('#mode-toggle .pill-btn').forEach(b => {
        b.classList.toggle('active', b.dataset.mode === S.mode);
        if (b.dataset.mode === 'intent') {
            b.textContent = hasLlmGroups() ? 'By intent' : 'By category';
            b.title = hasLlmGroups() ? 'Order by developer intent (i)' : 'Order by change category, attention first (i)';
        }
        if (b.dataset.mode === 'flow') b.classList.toggle('hidden', !hasFlow());
    });
    document.querySelectorAll('#view-toggle .pill-btn').forEach(b => b.classList.toggle('active', b.dataset.view === S.view));
    document.getElementById('noise-toggle').checked = S.hideNoise;
    document.getElementById('wrap-toggle').checked = S.wrap;

    const btn = document.getElementById('analyze-btn');
    const cap = d.capabilities || {};
    // Shown without a provider too: it opens the setup instructions, so the feature can be found.
    // A saved page has no server to run it.
    btn.classList.toggle('hidden', !!S.static);
    if (!S.running) btn.textContent = hasLlmGroups() ? 'Re-analyze…' : 'Analyze…';
    btn.title = cap.llm ? `Group changes by intent, rate risk and summarize with ${cap.llm}` : 'Group changes by intent, rate risk and summarize with an LLM (needs a one-time setup)';
    renderProgress();
}

function hasFlow() { return (S.data.cross_file?.reading_order || []).filter(s => s.kind === 'function').length >= 2; }

function hasLlmGroups() { return Array.isArray(S.data.intent_groups) && S.data.intent_groups.length > 0; }

/** Progress in the unit of the current view: files by file, sections otherwise. */
function renderProgress() {
    const bySection = grouped() && groups.length;
    const total = bySection ? groups.length : S.data.files.length;
    const done = bySection ? groups.filter(g => S.groupsReviewed[g.key]).length : S.data.files.filter(f => S.viewed[f.new_path]).length;
    const el = document.getElementById('review-progress');
    el.textContent = `${done} / ${total} ${bySection ? 'section' : 'file'}${pl(total)} viewed`;
    el.title = bySection ? 'Sections of this view marked as viewed (v)' : 'Files marked as viewed (v)';
    el.classList.toggle('done', done === total && total > 0);
}

function totals() {
    let add = 0, del = 0, changed = 0, mech = 0, tests = 0;
    S.data.results.forEach(r => {
        for (const h of r.hunks) for (const c of h.changes) { if (c.kind === 'added') add++; else if (c.kind === 'removed') del++; }
        changed += r.review?.changed_lines || 0;
        mech += r.review?.mechanical_lines || 0;
        tests += r.review?.test_lines || 0;
    });
    return { add, del, changed, mech, tests };
}

function renderOverview() {
    const d = S.data;
    const el = document.getElementById('overview');
    el.classList.toggle('collapsed', S.overviewCollapsed);
    const t = totals();
    const review = Math.max(0, t.changed - t.mech - t.tests);
    const pct = t.changed ? Math.round(t.mech * 100 / t.changed) : 0;
    const tpct = t.changed ? Math.round(t.tests * 100 / t.changed) : 0;
    const cap = d.capabilities || {};

    // Summary: the model's, or a plain count. Its source goes in the section label.
    const run = S.running;
    const summary = run
        ? `<p class="ov-summary analyzing"><span class="progress-line" aria-hidden="true"></span>Analyzing with ${esc(modelLabel(run.model))}${run.provider ? ` via ${esc(run.provider)}` : ''}${run.estimate ? `, ${esc(run.estimate.replace('≈', 'about'))}` : ''}. Keep reading; the summary and intent groups appear here when it's done.</p>`
        : d.summary
        ? `<p class="ov-summary">${richText(d.summary)}</p>`
        : `<p class="ov-summary muted">${d.files.length} file${pl(d.files.length)} changed. ${review ? `<b>${review}</b> line${pl(review)} carry real changes` : 'No behavior-bearing changes detected'}${t.mech ? `; ${t.mech} are mechanical` : ''}.</p>`;
    let source = '';
    if (d.llm_error) source = `<span class="error">analysis failed: ${esc(truncate(d.llm_error, 160))}</span>`;
    else if (hasLlmGroups()) source = `${esc(modelLabel(d.llm_model))}${d.llm_saved_at ? ` · ${esc(ago(d.llm_saved_at))}` : ''}`;
    let cta = '';
    if (!hasLlmGroups() && !d.llm_error && !run && !S.static) {
        cta = `<div class="ov-cta"><button class="link-btn" data-action="analyze">Analyze…</button> for a written summary, intent groups and risk${cap.llm ? '' : ' (with an LLM: a one-time setup)'}.</div>`;
    }

    // Stats live in the header row: useful open or collapsed.
    const stats = t.changed ? `<span class="ov-stats" title="${review} lines to review · ${t.tests} test lines · ${t.mech} mechanical">
        <span class="focus-bar" aria-hidden="true"><span class="review" style="width:${100 - pct - tpct}%"></span><span class="tests" style="width:${tpct}%"></span><span class="mech" style="width:${pct}%"></span></span>
        <span><b>${review}</b> to review</span>
        ${t.tests ? `<span><b>${t.tests}</b> test${pl(t.tests)}</span>` : ''}
        ${t.mech ? `<span><b>${t.mech}</b> mechanical</span>` : ''}
        ${testReachStat()}
    </span>` : '';
    const open = checkCount();
    el.innerHTML = `
        <div class="ov-head" data-action="toggle-overview">
            <span class="chev">▼</span><h2>Overview</h2>
            <span class="ov-files">${d.files.length} file${pl(d.files.length)} · <span class="add">+${t.add}</span> <span class="del">−${t.del}</span></span>
            ${stats}
            ${open ? `<span class="quick-open">${open} to check</span>` : ''}
        </div>
        <div class="ov-body">
            <div class="ov-main">
                <section>
                    <div class="ov-label">Summary${source ? ` <span class="muted">· ${source}</span>` : ''}${hasLlmGroups() && S.mode !== 'intent' ? ` <button class="link-btn label-link" data-action="mode-intent">Review by intent</button>` : ''}</div>
                    ${summary}${cta}
                </section>
                ${renderAskedFor()}
            </div>
            <aside class="ov-side">${renderCheck()}</aside>
        </div>`;
}

/** "Tests reach 17/26 changed functions"; hover for the ones they miss. */
function testReachStat() {
    const d = S.data, reach = d.cross_file?.test_reach || [];
    if (!reach.length) return '';
    if (!d.results.some(r => r.review.test_lines > 0)) return `<span class="reach-stat">no tests changed</span>`;
    const reached = reach.filter(t => t.via.length).length;
    const missed = reach.filter(t => !t.via.length).map(t => t.name);
    const tip = missed.length ? `Not reached by the changed tests: ${missed.join(', ')}` : 'Every changed function is reached by a changed test';
    return `<span class="reach-stat" title="${esc(tip)}">tests reach <b>${reached}</b>/${reach.length}</span>`;
}

/** "Claude Code", "Codex", or "Claude Code and Codex". */
function agentNames(sessions) {
    const names = [...new Set((sessions || []).map(s => ({ 'claude-code': 'Claude Code', codex: 'Codex' }[s.agent] || s.agent)))];
    return names.join(' and ') || 'agent';
}

/** The user's own prompts to the coding agent that made the change, as short quotes. */
function renderAskedFor() {
    const ctx = S.data.source?.sessions;
    const reqs = ctx?.requirements || [];
    if (!reqs.length) return '';
    const SHOWN = 2;
    const shown = S.askedOpen ? reqs : reqs.slice(-SHOWN);
    const n = ctx.sessions.length;
    const more = reqs.length - shown.length;
    return `<section class="ov-asked">
        <div class="ov-label">You asked <span class="muted">· ${reqs.length} prompt${pl(reqs.length)} in ${n === 1 ? `the ${agentNames(ctx.sessions)} session` : `${n} ${agentNames(ctx.sessions)} sessions`} that made this change</span></div>
        ${shown.map(r => `<blockquote class="req" id="req-${esc(r.id)}"><span class="rtext">${esc(r.text)}</span><span class="rid">${esc(r.id)}</span></blockquote>`).join('')}
        ${more > 0 || S.askedOpen ? `<button class="link-btn more" data-action="toggle-asked">${S.askedOpen ? 'Show fewer' : `${more} earlier prompt${pl(more)}`}</button>` : ''}
    </section>`;
}

/** Items a reviewer should settle before merging: engine findings first, then the model's notes. */
function checkItems() {
    const d = S.data;
    const facts = [], notes = [];
    for (const e of E.values()) {
        if (!['broken', 'call_sites', 'dead_code', 'swapped'].includes(e.kind)) continue;
        const site = e.kind === 'call_sites' || e.kind === 'swapped' ? e.sites[0] : null;
        const path = site ? site.file : e.path, line = site ? site.line : e.line;
        const more = site && e.sites.length > 1 ? ` +${e.sites.length - 1} more` : '';
        const code = site ? site.text : e.code;
        const label = e.kind === 'broken' ? 'Stale reference' : e.kind === 'call_sites' ? 'Call not updated' : e.kind === 'swapped' ? 'Import source changed' : 'Now unused';
        facts.push({ key: `e:${e.kind}:${e.text}`, ic: KIND[e.kind].ic, kind: e.kind, label, text: e.text, path, line, more, code, notInDiff: e.inDiff === false || site?.in_diff === false });
    }
    (d.concerns || []).forEach(c => notes.push({ key: 'c:' + c, text: c }));
    return { facts, notes };
}
function checkCount() {
    const { facts, notes } = checkItems();
    return [...facts, ...notes].filter(x => !S.checked[x.key]).length;
}

function renderCheck() {
    const { facts, notes } = checkItems();
    const all = [...facts, ...notes];
    const open = all.filter(x => !S.checked[x.key]).length;
    const haveModel = hasLlmGroups();
    if (!all.length) {
        return `<div class="check-card empty"><div class="ov-label">Before merging</div>
            <div class="check-empty"><span class="ok-mark">✓</span> Nothing found: no stale references, missed call sites or unused code.${haveModel || S.static ? '' : ' Analyze… adds the model’s notes.'}</div></div>`;
    }
    const byDone = (a, b) => (!!S.checked[a.key]) - (!!S.checked[b.key]);
    const box = (x) => `<input type="checkbox" data-check="${esc(x.key)}"${S.checked[x.key] ? ' checked' : ''} title="Mark as checked" aria-label="Mark as checked">`;
    const factRow = (x) => `<div class="check-row${S.checked[x.key] ? ' done' : ''}">${box(x)}
        <div class="check-body"><div class="check-text"><span class="ic k-${x.kind}">${x.ic}</span>${richText(x.text)}</div>
            <div class="check-where">${locLink(x.path, x.line)}${x.more}${x.notInDiff ? ' · not in this diff' : ''}</div></div></div>`;
    const noteRow = (x) => `<div class="check-row note${S.checked[x.key] ? ' done' : ''}">${box(x)}<div class="check-body"><div class="check-text">${richText(x.text)}</div></div></div>`;
    const breaks = facts.filter(x => x.kind === 'broken' || x.kind === 'call_sites').sort(byDone);
    const behavior = facts.filter(x => x.kind === 'swapped').sort(byDone);
    const cleanup = facts.filter(x => x.kind === 'dead_code').sort(byDone);
    const sortedNotes = [...notes].sort(byDone);
    const NOTES_SHOWN = 2;
    const shownNotes = S.checkOpen ? sortedNotes : sortedNotes.slice(0, NOTES_SHOWN);
    const group = (label, hint, rows, cls = '') => rows.length ? `<div class="check-group ${cls}"><div class="check-sub">${label} <span class="n">${rows.length}</span>${hint ? `<span class="hint">${hint}</span>` : ''}</div>` : '';
    return `<div class="check-card">
        <div class="ov-label">Before merging <span class="muted">· ${open ? `${open} open` : 'all checked'}</span></div>
        ${group('Likely to break', 'found in the code', breaks)}${breaks.map(factRow).join('')}${breaks.length ? '</div>' : ''}
        ${group('Behavior may change', 'same name, new source', behavior)}${behavior.map(factRow).join('')}${behavior.length ? '</div>' : ''}
        ${group('Cleanup', 'left unused by this change', cleanup)}${cleanup.map(factRow).join('')}${cleanup.length ? '</div>' : ''}
        ${group('Model notes', 'unverified', sortedNotes, 'notes')}${shownNotes.map(noteRow).join('')}
            ${sortedNotes.length > NOTES_SHOWN ? `<button class="link-btn more" data-action="toggle-check">${S.checkOpen ? 'Show fewer' : `${sortedNotes.length - NOTES_SHOWN} more`}</button>` : ''}${sortedNotes.length ? '</div>' : ''}
    </div>`;
}

function setChecked(key, on, input) {
    if (on) S.checked[key] = true; else delete S.checked[key];
    store.set('checked:' + sourceKey(), S.checked);
    const row = input?.closest('.check-row'), card = row?.closest('.check-card');
    if (!row) { renderOverview(); return; }
    // Update in place; re-sort once the pointer leaves, so rows don't move under it.
    row.classList.toggle('done', on);
    const { facts, notes } = checkItems();
    const open = [...facts, ...notes].filter(x => !S.checked[x.key]).length;
    const label = card.querySelector('.ov-label .muted');
    if (label) label.textContent = `· ${open ? `${open} open` : 'all checked'}`;
    const quick = document.querySelector('#overview .quick-open');
    if (quick) quick.textContent = open ? `${open} to check` : '';
    if (!card.dataset.resort) {
        card.dataset.resort = '1';
        card.addEventListener('mouseleave', () => renderOverview(), { once: true });
    }
}

function locLink(path, line) {
    if (!path) return '';
    const inDiff = fileByPath.has(path);
    const text = `${esc(path)}:${line}`;
    return inDiff ? `<button class="link-btn" data-jump-file="${esc(path)}" data-jump-line="${line}">${text}</button>` : `<span>${text}</span>`;
}

// --- Sidebar ---
function renderSidebar() {
    const el = document.getElementById('toc-content');
    document.querySelectorAll('.sidebar-tab').forEach(b => {
        b.classList.toggle('active', b.dataset.tab === S.tab);
        if (b.dataset.tab === 'changes') b.innerHTML = `Changes<span class="count">${changeListCount()}</span>`;
    });
    const shows = sidebarShows();
    if (shows === 'changes') { el.innerHTML = renderChangeList(); }
    else {
        // The outline names what it maps, so it's never unclear why it changed.
        const n = shows === 'files' ? S.data.files.length : groups.filter(g => !g.flow || !g.flow.repeat).length;
        const unit = shows === 'files' ? 'file' : S.mode === 'flow' ? 'step' : 'group';
        const hint = shows === 'files' ? ''
            : S.mode === 'flow' ? 'Entry points first, then the changed functions they call.'
            : hasLlmGroups() ? 'What each set of changes is for, in the order to read them.'
            : 'Grouped by kind of change, most important first. <button class="link-btn" data-action="analyze">Analyze…</button> groups them by intent.';
        const caption = `<div class="toc-caption"><div><b>${modeName()}</b> · ${n} ${unit}${pl(n)}</div>${hint ? `<div class="hint">${hint}</div>` : ''}</div>`;
        el.innerHTML = caption + (shows === 'files' ? renderFileTree() : renderGroupList());
    }
    applyFilter(document.getElementById('toc-filter').value);
    markCurrent();
}

/** The changed files as a directory tree, folders before files, each sorted by name. */
function fileTree() {
    const root = { dirs: new Map(), files: [] };
    S.data.files.forEach((f, fi) => {
        const parts = f.new_path.split('/');
        let node = root;
        for (const p of parts.slice(0, -1)) {
            if (!node.dirs.has(p)) node.dirs.set(p, { dirs: new Map(), files: [] });
            node = node.dirs.get(p);
        }
        node.files.push({ name: parts[parts.length - 1], fi });
    });
    const sortNode = (node) => {
        node.dirs = new Map([...node.dirs].sort((a, b) => a[0].localeCompare(b[0])));
        node.files.sort((a, b) => a.name.localeCompare(b.name));
        node.dirs.forEach(sortNode);
    };
    sortNode(root);
    return root;
}

/** File indexes in the order the tree lists them; the diff pane follows the same order. */
let fileOrderCache = null;
function fileOrder() {
    if (fileOrderCache) return fileOrderCache;
    const out = [];
    const walk = (node) => { node.dirs.forEach(walk); node.files.forEach(({ fi }) => out.push(fi)); };
    walk(fileTree());
    return (fileOrderCache = out);
}

function renderFileTree() {
    const root = fileTree();
    // Compact single-child directory chains: a/b/c → "a/b/c".
    const compact = (name, node) => {
        while (node.files.length === 0 && node.dirs.size === 1) {
            const [n, child] = [...node.dirs][0];
            name += '/' + n; node = child;
        }
        return [name, node];
    };
    // Directories containing only generated/binary files start collapsed.
    const allQuiet = (node) => node.files.every(({ fi }) => S.data.files[fi].binary || S.data.results[fi].review.generated)
        && [...node.dirs.values()].every(allQuiet);
    const walk = (node, depth) => {
        let html = '';
        for (const [n, child] of node.dirs) {
            const [name, c] = compact(n, child);
            const pad = 8 + depth * 14;
            const quiet = allQuiet(c);
            html += `<div class="tree-node${quiet ? ' collapsed' : ''}"><div class="tree-row tree-dir" data-dir style="padding-left:${pad}px" data-filter="${esc(name.toLowerCase())}"><span class="chev">▼</span><span class="name">${esc(name)}</span></div><div class="tree-children">${walk(c, depth + 1)}</div></div>`;
        }
        for (const { name, fi } of node.files) {
            const f = S.data.files[fi], r = S.data.results[fi];
            const { add, del } = fileCounts(fi);
            const allMech = r.review.changed_lines > 0 && r.review.mechanical_lines === r.review.changed_lines;
            const tag = f.binary ? 'binary' : r.review.generated ? 'generated' : allMech ? 'noise' : !r.review.parsed ? 'plain' : '';
            const viewed = !!S.viewed[f.new_path];
            const pad = 8 + depth * 14 + 12;
            html += `<div class="tree-row tree-file${viewed ? ' viewed' : ''}" data-fi="${fi}" style="padding-left:${pad}px" data-filter="${esc(f.new_path.toLowerCase())}" title="${esc(f.new_path)}">
                <span class="status ${esc(f.status || 'M')}">${esc(f.status || 'M')}</span>
                <span class="name">${esc(name)}</span>
                ${tag ? `<span class="tag">${tag}</span>` : ''}
                <span class="stats"><span class="a">+${add}</span><span class="d">−${del}</span></span>
                <span class="viewed-mark">${viewed ? '✓' : ''}</span>
            </div>`;
        }
        return html;
    };
    return walk(root, 0) || '<div class="toc-empty">No files</div>';
}

/** An entry's text with its owner type shrinking first: `Reader.readInt: body modified` keeps the method. */
function entryLabel(e) {
    const text = String(e.text);
    if (e.kind === 'dependency') return esc(text);
    const colon = text.indexOf(': ');
    const name = colon > 0 ? text.slice(0, colon) : text;
    const i = Math.max(name.lastIndexOf('.'), name.lastIndexOf('::'));
    if (i <= 0) return esc(text);
    const sep = name[i] === ':' ? '::' : '.';
    return `<span class="owner">${esc(name.slice(0, i))}${sep}</span><span class="rest">${esc(text.slice(i + sep.length))}</span>`;
}

/** `Owner.method` with the owner shrinking first, so long class names don't hide the method. */
function flowLabel(label) {
    const i = Math.max(label.lastIndexOf('.'), label.lastIndexOf('::'));
    if (i <= 0) return esc(label);
    const sep = label[i] === ':' ? '::' : '.';
    return `<span class="flow-owner">${esc(label.slice(0, i))}${sep}</span><span class="flow-method">${esc(label.slice(i + sep.length))}</span>`;
}

function renderGroupList() {
    let html = '';
    groups.forEach((g, gi) => {
        const reviewed = !!S.groupsReviewed[g.key];
        const hunks = groupHunks(gi);
        const st = lineStats(hunks);
        const entries = renderGroupEntries(g);
        // Reading-order steps are one line each; their entries are a click away.
        const folded = g.mechanical || g.key.startsWith('flow:') || reviewed;
        html += `<div class="toc-group${reviewed ? ' reviewed' : ''}${folded ? ' collapsed' : ''}" data-gi="${gi}">
            <div class="toc-head" data-jump-group="${gi}" title="Go to this group">
                <span class="chev" data-group-toggle title="Show entries">▼</span><span class="num">${gi + 1}</span>
                <span class="title${g.flow ? ' flow-title' : ''}"${g.flow ? ` style="padding-left:${Math.min(g.flow.depth, 6) * 10}px" title="${esc(g.label)} · ${esc(g.flow.file)}"` : ''}>${g.flow ? `<span class="flow-name">${g.flow.depth ? '<span class="flow-depth">└ </span>' : ''}${flowLabel(g.label)}</span><span class="flow-file">${esc(baseName(g.flow.file))}</span>` : esc(g.label)}${g.note && !g.flow ? `<span class="note" title="${esc(g.note)}">${richText(g.note, false)}</span>` : ''}</span>
                ${riskHtml(g.risk)}${g.origin === 'autonomous' ? originChip(g) : ''}
                <span class="n" title="+${st.add} −${st.del} lines">${g.ids.length || hunks.length}</span>
            </div>
            <div class="toc-body">${entries}</div>
        </div>`;
    });
    return html || '<div class="toc-empty">No changes</div>';
}

function renderGroupEntries(g) {
    const row = (id) => {
        const e = E.get(id);
        if (!e) return '';
        const target = entryHunks.has(id) || e.fi >= 0;
        return `<div class="toc-row entry${target ? '' : ' no-target'}" data-entry="${id}" data-filter="${esc((e.text + ' ' + (e.path || '')).toLowerCase())}" title="${esc(e.text)}${e.path ? '\n' + esc(e.path) : ''}">
            <span class="ic k-${e.kind}">${KIND[e.kind].ic}</span><span class="txt">${entryLabel(e)}</span>${e.path ? `<span class="loc">${esc(baseName(e.path))}</span>` : ''}</div>`;
    };
    if (g.subs && g.subs.length > 1) {
        return g.subs.map(sg => `<div class="toc-sub">${esc(sg.label)}</div>${sg.entry_ids.map(row).join('')}`).join('');
    }
    return g.ids.map(row).join('');
}

/** Hunks with no entry, by file, grouped the same way the reading order does. */
function unclassifiedFiles() {
    const d = S.data;
    const out = { other: [], tests: [], docs: [], mechanical: [] };
    d.results.forEach((r, fi) => {
        const buckets = {};
        r.hunks.forEach(h => {
            if ((h.manifest_refs || []).length) return;
            const b = h.noise || r.review.generated ? 'mechanical' : h.test || r.review.role === 'test' ? 'tests' : r.review.role === 'docs' ? 'docs' : 'other';
            (buckets[b] = buckets[b] || []).push(h);
        });
        for (const [b, hs] of Object.entries(buckets)) {
            const add = hs.reduce((n, h) => n + h.changes.filter(c => c.kind === 'added').length, 0);
            const del = hs.reduce((n, h) => n + h.changes.filter(c => c.kind === 'removed').length, 0);
            out[b].push({ fi, path: d.files[fi].new_path, add, del, line: hs[0].new_range.start || hs[0].old_range.start || 1 });
        }
    });
    return out;
}

function changeListCount() {
    const u = unclassifiedFiles();
    return E.size + Object.values(u).reduce((n, fs) => n + fs.length, 0);
}

function renderChangeList() {
    const files = unclassifiedFiles();
    const fileRow = (f) => `<div class="toc-row entry" data-jump-file="${esc(f.path)}" data-jump-line="${f.line}" data-filter="${esc(f.path.toLowerCase())}" title="${esc(f.path)}">
        <span class="ic">▤</span><span class="txt">${esc(baseName(f.path))} ${statsHtml(f)}</span><span class="loc">${esc(f.path.split('/').slice(0, -1).join('/'))}</span></div>`;
    const section = (label, n, body, collapsed) => `<div class="toc-group${collapsed ? ' collapsed' : ''}"><div class="toc-head" data-group-toggle><span class="chev">▼</span><span class="title">${esc(label)}</span><span class="n">${n}</span></div>
            <div class="toc-body">${body}</div></div>`;
    let html = '';
    for (const c of CATEGORIES) {
        const ids = [...E.values()].filter(e => entryCat(e) === c.key).map(e => e.id);
        const extra = files[c.key] || [];
        if (c.key === 'mechanical' || c.key === 'tests') {
            // Also add generated files, comment-only edits and test code that have no entry.
            if (!ids.length && !extra.length) continue;
            html += section(c.label, ids.length + extra.length, renderGroupEntries({ ids }) + extra.map(fileRow).join(''), c.mechanical);
            continue;
        }
        if (ids.length) html += section(c.label, ids.length, renderGroupEntries({ ids }), false);
        if (c.key === 'structure') {
            // Same spot as in the reading order, before tests and mechanical.
            if (files.other.length) html += section('Other changes', files.other.length, files.other.map(fileRow).join(''), false);
            if (files.docs.length) html += section('Docs', files.docs.length, files.docs.map(fileRow).join(''), false);
        }
    }
    return html || '<div class="toc-empty">No changes.</div>';
}

function applyFilter(q) {
    q = (q || '').toLowerCase().trim();
    const root = document.getElementById('toc-content');
    root.querySelectorAll('[data-filter]').forEach(el => el.classList.toggle('hidden', !!q && !el.dataset.filter.includes(q)));
    // "Unviewed": hide files and sections already marked as viewed.
    root.querySelectorAll('.tree-file.viewed, .toc-group.reviewed').forEach(el => el.classList.toggle('viewed-hidden', !!S.onlyUnviewed));
    if (sidebarShows() === 'files') {
        // Show directories that contain a visible file; expand them while filtering.
        root.querySelectorAll('.tree-node').forEach(n => {
            const any = n.querySelector('.tree-file:not(.hidden):not(.viewed-hidden)');
            n.classList.toggle('hidden', (!!q || !!S.onlyUnviewed) && !any);
            if (q && any) { n.classList.remove('collapsed'); n.querySelector('.tree-dir')?.classList.remove('hidden'); }
        });
    } else {
        root.querySelectorAll('.toc-group').forEach(g => {
            // Counts say how many of the group's entries match.
            const n = g.querySelector('.toc-head .n');
            if (n && n.dataset.total === undefined) n.dataset.total = n.textContent;
            if (!q) { g.classList.remove('hidden'); if (n) n.textContent = n.dataset.total; return; }
            const shown = g.querySelectorAll('.entry:not(.hidden)').length;
            g.classList.toggle('hidden', !shown);
            if (shown) g.classList.remove('collapsed');
            if (n) n.textContent = `${shown}/${g.querySelectorAll('.entry').length}`;
        });
    }
}

// --- Main pane ---
function renderMain(keepScroll) {
    const container = document.getElementById('diff-scroll');
    const y = window.scrollY;
    document.body.classList.toggle('split-view', viewMode() === 'split');
    container.classList.toggle('split', viewMode() === 'split');
    if (sectionObserver) sectionObserver.disconnect();
    if (hunkObserver) hunkObserver.disconnect();
    order = [];

    if (grouped()) {
        container.innerHTML = groups.map((g, gi) => sectionShell('g' + gi, groupHeadHtml(g, gi), groupIntroHtml(g))).join('')
            || '<div class="section"><div class="section-note">Nothing to group.</div></div>';
        groups.forEach((g, gi) => { for (const { fi, hi } of groupHunks(gi)) order.push(fi + ':' + hi); });
    } else {
        container.innerHTML = fileOrder().map(fi => sectionShell('f' + fi, fileHeadHtml(fi), '')).join('');
        fileOrder().forEach(fi => S.data.results[fi].hunks.forEach((_, hi) => order.push(fi + ':' + hi)));
    }

    // Render section bodies lazily as they approach the viewport.
    sectionObserver = new IntersectionObserver((entries) => {
        for (const en of entries) if (en.isIntersecting) renderSectionBody(en.target.closest('.section'));
    }, { rootMargin: '1500px 0px' });
    hunkObserver = new IntersectionObserver(onHunkVisible, { rootMargin: `-${88}px 0px -60% 0px` });
    const pending = [...container.querySelectorAll('.section-body[data-pending]')];
    pending.forEach(b => sectionObserver.observe(b));
    // Render the first sections now and the rest progressively in the background,
    // so in-page search (Ctrl+F) finds everything on normal-sized diffs. Very large
    // diffs stay lazy (rendered as they approach the viewport).
    const gen = ++renderGeneration;
    pending.slice(0, 4).forEach(b => renderSectionBody(b.closest('.section')));
    let budgetLines = BACKGROUND_RENDER_LINES;
    // Collapsed sections (generated, binary, viewed) render when opened.
    const queue = pending.slice(4).filter(b => !b.closest('.section').classList.contains('collapsed'));
    const step = () => {
        if (gen !== renderGeneration) return;
        const t0 = performance.now();
        while (queue.length && performance.now() - t0 < 12 && budgetLines > 0) {
            const section = queue.shift().closest('.section');
            budgetLines -= sectionLines(section.dataset.key);
            renderSectionBody(section);
        }
        if (queue.length && budgetLines > 0) setTimeout(step, 16);
    };
    setTimeout(step, 30);
    updateNavCounter();
    if (keepScroll) window.scrollTo(0, y);
}

function sectionShell(key, head, intro) {
    const collapsed = S.collapsedSections.has(key);
    return `<section class="section${collapsed ? ' collapsed' : ''}${key[0] === 'f' && S.viewed[S.data.files[+key.slice(1)].new_path] ? ' viewed' : ''}" id="s-${key}" data-key="${key}">
        ${head}<div class="section-body" data-pending="1">${intro}<div class="placeholder">…</div></div></section>`;
}

function fileHeadHtml(fi) {
    const f = S.data.files[fi], r = S.data.results[fi];
    const { add, del } = fileCounts(fi);
    const slash = f.new_path.lastIndexOf('/');
    const dir = slash >= 0 ? f.new_path.slice(0, slash + 1) : '';
    const tags = [];
    if (f.status === 'A') tags.push('<span class="tag" style="color:var(--diff-add-fg)">new</span>');
    if (f.status === 'D') tags.push('<span class="tag" style="color:var(--diff-del-fg)">deleted</span>');
    if (ROLE_TAG[r.review.role]) tags.push(`<span class="tag ${r.review.role === 'test' ? 'test' : 'mech'}">${ROLE_TAG[r.review.role]}</span>`);
    if (f.binary) tags.push('<span class="tag">binary</span>');
    else if (r.review.generated) tags.push('<span class="tag mech">generated</span>');
    else if (!r.review.parsed && f.language === 'Unknown') tags.push('<span class="tag" title="Not semantically analyzed">plain diff</span>');
    if (!r.review.generated && r.review.changed_lines && r.review.mechanical_lines === r.review.changed_lines) tags.push('<span class="tag mech">mechanical only</span>');
    else if (r.review.mechanical_lines) tags.push(`<span class="tag mech" title="Lines that are formatting, comments, renames or moves">${r.review.mechanical_lines} mechanical</span>`);
    const viewed = !!S.viewed[f.new_path];
    if (!viewed && S.changedSinceViewed[f.new_path]) tags.unshift('<span class="tag changed" title="You marked this file viewed; it has changed since">changed since viewed</span>');
    return `<div class="section-head" data-section-toggle>
        <span class="chev">▼</span>
        <span class="path"><span class="dir">${esc(dir)}</span>${esc(f.new_path.slice(slash + 1))}</span>
        ${f.status === 'R' ? `<span class="from">← ${esc(f.old_path)}</span>` : ''}
        <button class="copy-btn" data-copy="${esc(f.new_path)}" title="Copy path" aria-label="Copy path">${IC.copy}</button>
        ${tags.join('')}
        <span class="spacer"></span>
        <span class="stats"><span class="a">+${add}</span><span class="d">−${del}</span></span>
        <label class="viewed-toggle" title="Mark as viewed (v)"><input type="checkbox" data-viewed="${fi}"${viewed ? ' checked' : ''}><span>Viewed</span></label>
    </div>`;
}

function groupHeadHtml(g, gi) {
    const reviewed = !!S.groupsReviewed[g.key];
    const st = lineStats(groupHunks(gi));
    const f = g.flow;
    const depth = f ? `<span class="flow-depth">${'  '.repeat(Math.min(f.depth, 8))}${f.depth ? '└ ' : ''}</span>` : '';
    let reach = '';
    if (f && g.anyTests && g.reach) {
        reach = g.reach.via.length
            ? `<span class="reach ok" title="Reached by ${esc(g.reach.tests.map(t => t.name).join(', '))}: ${esc(g.reach.via.join(' → '))}">✓ tested</span>`
            : `<span class="reach" title="No changed test calls this, directly or through the changed files">untested</span>`;
    }
    return `<div class="section-head group-head" data-section-toggle>
        <span class="chev">▼</span><span class="num">${gi + 1}.</span>${depth}<span class="title">${esc(g.label)}</span>
        ${f ? `<span class="flow-file">${esc(shortPath(f.file))}</span>` : ''}${reach}
        ${riskHtml(g.risk)}${originChip(g)}
        <span class="spacer"></span>${statsHtml(st)}
        <label class="viewed-toggle" title="Mark as viewed (v)"><input type="checkbox" data-group-reviewed="${gi}"${reviewed ? ' checked' : ''}><span>Viewed</span></label>
    </div>`;
}

function riskHtml(risk) {
    if (!risk) return '';
    const label = { high: 'High risk', medium: 'Medium risk', low: 'Low risk' }[risk] || risk;
    return `<span class="risk ${esc(risk)}">${label}</span>`;
}

function originChip(g) {
    if (g.origin === 'autonomous') return `<span class="origin agent" title="Not something you asked for in your prompts; the coding agent decided it">agent's call</span>`;
    if (g.origin === 'mixed') return `<span class="origin mixed" title="Asked for, with significant choices the agent made on its own">partly asked</span>`;
    if (g.origin === 'requested') return `<span class="origin asked" title="Traced to your own words">asked</span>`;
    return '';
}

function groupIntroHtml(g) {
    const parts = [];
    if (g.flow) {
        const link = (name) => {
            const gi = groups.findIndex(x => x.flow && x.label === name);
            return gi >= 0 ? `<button class="link-btn" data-jump-group="${gi}">${esc(name)}</button>` : esc(name);
        };
        const bits = [];
        if (g.flow.called_by.length) bits.push(`Called by ${g.flow.called_by.map(link).join(', ')}`);
        if (g.flow.calls.length) bits.push(`Calls ${g.flow.calls.map(link).join(', ')}`);
        if (g.reach?.via.length > 2) bits.push(`Tested via ${esc(g.reach.via.join(' → '))}`);
        if (bits.length) parts.push(`<div class="flow-links">${bits.join(' · ')}</div>`);
    }
    if (g.requirement) parts.push(`<div class="asked">You asked: <q>${esc(g.requirement.quote)}</q> <button class="link-btn" data-req="${esc(g.requirement.id)}">${esc(g.requirement.id)}</button></div>`);
    else if (g.origin === 'autonomous') parts.push(`<div class="asked agent">Not in your prompts: the agent decided this. Check it matches what you want.</div>`);
    if (g.note && g.llm) parts.push(`<div class="verify"><b>Verify:</b> ${richText(g.note)}</div>`);
    else if (g.note) parts.push(`<div>${esc(g.note)}</div>`);
    if (g.subs && g.subs.length > 1) parts.push(`<div class="subs">${g.subs.map(s => `<span class="chip">${esc(s.label)}</span>`).join('')}</div>`);
    // Entries that have no hunk (e.g. stale references in unchanged files) are listed inline.
    // Entries whose hunks are shown in another group.
    const gi = groups.indexOf(g);
    const elsewhere = g.ids.map(id => E.get(id)).filter(e => e && entryHunks.has(e.id) && !entryHunks.get(e.id).some(({ fi, hi }) => hunkGroup.get(fi + ':' + hi) === gi));
    if (elsewhere.length) {
        parts.push(`<div>${elsewhere.slice(0, 8).map(e => {
            const { fi, hi } = entryHunks.get(e.id)[0];
            return `<div><span class="k-${e.kind}">${KIND[e.kind].ic}</span> ${esc(e.text)}, <button class="link-btn" data-entry="${e.id}">shown in ${esc(groups[hunkGroup.get(fi + ':' + hi)]?.label || 'another group')}</button></div>`;
        }).join('')}</div>`);
    }
    const orphans = g.ids.map(id => E.get(id)).filter(e => e && !entryHunks.has(e.id));
    if (orphans.length) {
        parts.push(`<div>${orphans.slice(0, 12).map(e => `<div><span class="k-${e.kind}">${KIND[e.kind].ic}</span> ${esc(e.text)} ${e.path ? `<span class="mono" style="color:var(--text-3)">${esc(e.path)}${e.line ? ':' + e.line : ''}</span>` : ''}${e.code ? `<code style="display:block;color:var(--text-3)">${esc(e.code)}</code>` : ''}</div>`).join('')}${orphans.length > 12 ? `<div>… ${orphans.length - 12} more</div>` : ''}</div>`);
    }
    return parts.length ? `<div class="group-intro">${parts.join('')}</div>` : '';
}

function sectionLines(key) {
    const hunks = key[0] === 'f' ? S.data.results[+key.slice(1)].hunks.map((_, hi) => ({ fi: +key.slice(1), hi })) : groupHunks(+key.slice(1));
    let n = 0;
    for (const { fi, hi } of hunks) n += S.data.results[fi].hunks[hi].changes.length;
    return n;
}

function renderSectionBody(section) {
    const body = section.querySelector('.section-body');
    if (!body || !body.dataset.pending) return;
    delete body.dataset.pending;
    sectionObserver?.unobserve(body);
    const key = section.dataset.key;
    const intro = body.querySelector('.group-intro')?.outerHTML || '';
    let html = '';
    if (key[0] === 'f') html = renderFileBody(+key.slice(1));
    else {
        const hs = groupHunks(+key.slice(1));
        html = hs.length ? renderHunkList(hs, true) : intro ? '' : '<div class="section-note">No changes to show in this group.</div>';
    }
    body.innerHTML = intro + html;
    body.querySelectorAll('.hunk').forEach(h => hunkObserver?.observe(h));
}

function renderFileBody(fi) {
    const f = S.data.files[fi], r = S.data.results[fi];
    if (f.binary) return '<div class="section-note">Binary file, contents not shown.</div>';
    if (!r.hunks.length) return `<div class="section-note">${f.status === 'R' ? 'Renamed without content changes.' : 'No content changes (mode or metadata only).'}</div>`;
    let html = '';
    const newLines = splitLines(f.new_source);
    let prevNewEnd = 0, delta = 0; // old = new + delta in unchanged gaps
    for (let hi = 0; hi < r.hunks.length; hi++) {
        const h = r.hunks[hi];
        const startNew = h.new_range.start || 0;
        if (f.new_source && startNew > 0) {
            const firstDelta = (h.old_range.start || 0) - startNew;
            const gapFrom = prevNewEnd + 1, gapTo = startNew - 1;
            if (gapTo >= gapFrom) html += expandRow(fi, gapFrom, gapTo, hi === 0 ? firstDelta : delta);
        }
        // A run of collapsed mechanical hunks becomes one row (the gaps between them stay hidden).
        let end = hi;
        while (isFolded(fi, hi) && end + 1 < r.hunks.length && isFolded(fi, end + 1)) end++;
        if (end > hi) {
            html += noiseRunHtml(Array.from({ length: end - hi + 1 }, (_, k) => ({ fi, hi: hi + k })), false);
        } else {
            html += renderHunk(fi, hi, false);
        }
        const last = r.hunks[end];
        if (last.new_range.end) {
            prevNewEnd = Math.max(prevNewEnd, last.new_range.end);
            delta = (last.old_range.end || 0) - last.new_range.end;
        }
        hi = end;
    }
    if (f.new_source && prevNewEnd > 0 && prevNewEnd < newLines.length) html += expandRow(fi, prevNewEnd + 1, newLines.length, delta);
    return html;
}

function isFolded(fi, hi) {
    const h = S.data.results[fi].hunks[hi];
    return !!h.noise && S.hideNoise && !S.expandedNoise.has(fi + ':' + hi);
}

/** Hunks in order, with runs of 2+ collapsed mechanical hunks folded into one row. */
function renderHunkList(list, withFile) {
    let html = '';
    for (let i = 0; i < list.length; i++) {
        let j = i;
        while (isFolded(list[i].fi, list[i].hi) && j + 1 < list.length && isFolded(list[j + 1].fi, list[j + 1].hi)) j++;
        html += j > i ? noiseRunHtml(list.slice(i, j + 1), withFile) : renderHunk(list[i].fi, list[i].hi, withFile);
        i = j;
    }
    return html;
}

function noiseRunHtml(run, withFile) {
    const lines = run.reduce((n, { fi, hi }) => n + S.data.results[fi].hunks[hi].changes.filter(c => c.kind === 'added' || c.kind === 'removed').length, 0);
    const kinds = [...new Set(run.map(({ fi, hi }) => NOISE_LABEL[S.data.results[fi].hunks[hi].noise] || 'mechanical'))];
    const keys = run.map(({ fi, hi }) => fi + ':' + hi).join(',');
    return `<div class="noise-run" data-run="${keys}" data-with-file="${withFile ? 1 : 0}">
        <div class="noise-row">${run.length} mechanical changes · ${lines} line${pl(lines)} · ${esc(kinds.join(', '))} <button class="link-btn" data-open-run>show</button></div>
        <div class="run-body">${run.map(({ fi, hi }) => renderHunk(fi, hi, withFile)).join('')}</div>
    </div>`;
}

function expandRow(fi, from, to, delta) {
    const n = to - from + 1;
    const attrs = `data-expand="${fi}" data-from="${from}" data-to="${to}" data-delta="${delta}"`;
    // Small gaps open in one go; big ones step 20 lines from either side (GitLab/Gerrit style).
    if (n <= GAP_STEP * 2) return `<div class="expand" ${attrs} data-dir="all" role="button" tabindex="0">↕ ${n} unchanged line${pl(n)}</div>`;
    return `<div class="expand" ${attrs}>
        <button class="gap-btn" data-dir="down" title="Show ${GAP_STEP} more lines below the code above">↓ ${GAP_STEP}</button>
        <span class="gap-n">${n} unchanged lines</span>
        <button class="gap-btn" data-dir="all">Show all</button>
        <button class="gap-btn" data-dir="up" title="Show ${GAP_STEP} more lines above the code below">↑ ${GAP_STEP}</button>
    </div>`;
}

function hunkChips(h, fi) {
    const ids = h.manifest_refs || [];
    const chips = ids.slice(0, 4).map(id => {
        const e = E.get(id); if (!e) return '';
        return `<span class="hunk-chip k-${e.kind}" title="${esc(e.text)}">${KIND[e.kind].ic} ${esc(truncate(e.short || e.text, 40))}</span>`;
    }).join('');
    return chips + (ids.length > 4 ? `<span class="hunk-chip">+${ids.length - 4}</span>` : '');
}

function renderHunk(fi, hi, withFile) {
    const f = S.data.files[fi];
    const h = S.data.results[fi].hunks[hi];
    const key = fi + ':' + hi;
    const n = h.changes.filter(c => c.kind === 'added' || c.kind === 'removed').length;
    const oldCount = h.old_range.end ? h.old_range.end - h.old_range.start + 1 : 0;
    const newCount = h.new_range.end ? h.new_range.end - h.new_range.start + 1 : 0;
    const range = `@@ −${h.old_range.start},${oldCount} +${h.new_range.start},${newCount} @@`;
    const fileLink = withFile ? `<span class="file-link" data-goto-file="${fi}" title="${esc(f.new_path)}">${esc(shortPath(f.new_path))}</span>` : '';
    const testTag = h.test ? '<span class="tag test">test</span>' : '';
    // What the hunk touches leads; the raw @@ range is a quiet line reference.
    const onNew = newCount > 0;
    const [a, b] = onNew ? [h.new_range.start, h.new_range.end] : [h.old_range.start, h.old_range.end];
    const lines = `${onNew ? '' : 'old '}L${a}${b > a ? '–' + b : ''}`;
    const head = `<div class="hunk-head">${fileLink}<span class="hunk-chips">${hunkChips(h, fi)}</span>${testTag}<span class="range" title="${range}">${lines}</span></div>`;
    const collapsed = h.noise && S.hideNoise && !S.expandedNoise.has(key);
    const body = collapsed
        ? `<div class="noise-row">${n} line${pl(n)} · ${NOISE_LABEL[h.noise] || h.noise} <button class="link-btn" data-show-noise="${key}">show</button></div>`
        : (viewMode() === 'split' ? renderSplitRows(fi, h) : renderUnifiedRows(fi, h));
    return `<div class="hunk${h.noise ? ' is-noise' : ''}" id="h-${fi}-${hi}" data-hunk="${key}">${head}${body}</div>`;
}

function rowNoise(c) {
    if (!c.noise || !S.hideNoise) return ['', ''];
    const blank = !(c.content_new ?? c.content_old ?? '').trim();
    return [' noise', blank ? '' : `<span class="ntag">${NOISE_LABEL[c.noise] || ''}</span>`];
}

function renderUnifiedRows(fi, h) {
    const lang = fileLang(fi);
    const items = collapseContext(h.changes);
    let html = '';
    for (let i = 0; i < items.length; i++) {
        const it = items[i];
        if (it.type === 'gap') { html += `<div class="expand" data-inline-expand>↕ ${it.count} unchanged line${pl(it.count)}</div>`; html += `<div class="hidden" data-inline-lines>${it.changes.map(c => ctxRow(fi, c, lang)).join('')}</div>`; continue; }
        const c = it.change;
        if (c.kind === 'context') { html += ctxRow(fi, c, lang); continue; }
        if (c.kind === 'removed') {
            const next = items[i + 1]?.change;
            const pairable = next && next.kind === 'added' && isSinglePair(items, i);
            const [ncls, ntag] = rowNoise(c);
            if (pairable) {
                const wd = wordDiff(c.content_old || '', next.content_new || '');
                const [ncls2, ntag2] = rowNoise(next);
                html += row('del' + ncls, c.old_span?.start_line, '', '−', wordHtml(fi, 'old', c.old_span?.start_line, c.content_old, wd, 'del', lang), ntag, 'o' + c.old_span?.start_line);
                html += row('add' + ncls2, '', next.new_span?.start_line, '+', wordHtml(fi, 'new', next.new_span?.start_line, next.content_new, wd, 'add', lang), ntag2, 'n' + next.new_span?.start_line);
                i++;
            } else {
                html += row('del' + ncls, c.old_span?.start_line, '', '−', hl(fi, 'old', c.old_span?.start_line, c.content_old, lang), ntag, 'o' + c.old_span?.start_line);
            }
        } else if (c.kind === 'added') {
            const [ncls, ntag] = rowNoise(c);
            html += row('add' + ncls, '', c.new_span?.start_line, '+', hl(fi, 'new', c.new_span?.start_line, c.content_new, lang), ntag, 'n' + c.new_span?.start_line);
        }
    }
    return html;
}

// Pair a removed line with the following added line only for 1:1 replacements.
function isSinglePair(items, i) {
    const prev = items[i - 1]?.change, after = items[i + 2]?.change;
    return (!prev || prev.kind !== 'removed') && (!after || after.kind !== 'added');
}

function ctxRow(fi, c, lang) {
    return row('ctx', c.old_span?.start_line, c.new_span?.start_line, ' ', hl(fi, 'new', c.new_span?.start_line, c.content_new ?? c.content_old, lang), '', 'n' + c.new_span?.start_line);
}

function row(cls, oldLn, newLn, prefix, code, tag, anchor) {
    return `<div class="row ${cls}" data-a="${anchor || ''}"><span class="gl">${oldLn || ''}</span><span class="gl">${newLn || ''}</span><span class="pf">${prefix}</span><span class="cd">${code}</span>${tag || ''}</div>`;
}

function renderSplitRows(fi, h) {
    const lang = fileLang(fi);
    const rows = [];
    const ch = h.changes;
    let i = 0;
    while (i < ch.length) {
        const c = ch[i];
        if (c.kind === 'context') { rows.push({ l: c, r: c, t: 'ctx' }); i++; continue; }
        if (c.kind === 'removed' || c.kind === 'added') {
            const dels = [], adds = [];
            while (i < ch.length && ch[i].kind === 'removed') dels.push(ch[i++]);
            while (i < ch.length && ch[i].kind === 'added') adds.push(ch[i++]);
            const n = Math.max(dels.length, adds.length);
            for (let k = 0; k < n; k++) rows.push({ l: dels[k] || null, r: adds[k] || null, t: 'chg' });
            continue;
        }
        i++;
    }
    // Collapse long context runs.
    const keep = rows.map(r => r.t !== 'ctx');
    const out = [];
    for (let k = 0; k < rows.length; k++) {
        const near = keep.slice(Math.max(0, k - 3), k + 4).some(Boolean);
        out.push(near || rows.length < 12 ? rows[k] : null);
    }
    let html = '', hidden = [];
    const flush = () => {
        if (!hidden.length) return;
        html += `<div class="expand" data-inline-expand>↕ ${hidden.length} unchanged line${pl(hidden.length)}</div><div class="hidden" data-inline-lines>${hidden.map(r => splitRow(fi, r, lang)).join('')}</div>`;
        hidden = [];
    };
    out.forEach((r, k) => { if (r) { flush(); html += splitRow(fi, r, lang); } else hidden.push(rows[k]); });
    flush();
    return html;
}

function splitRow(fi, r, lang) {
    const L = r.l, R = r.r;
    if (r.t === 'ctx') {
        const code = hl(fi, 'new', R.new_span?.start_line, R.content_new ?? R.content_old, lang);
        return `<div class="row ctx" data-a="n${R.new_span?.start_line}"><span class="gl">${L.old_span?.start_line || ''}</span><span class="pf"></span><span class="cd">${code}</span><span class="gl mid">${R.new_span?.start_line || ''}</span><span class="pf"></span><span class="cd">${code}</span></div>`;
    }
    const wd = L && R ? wordDiff(L.content_old || '', R.content_new || '') : null;
    const left = L ? (wd ? wordHtml(fi, 'old', L.old_span?.start_line, L.content_old, wd, 'del', lang) : hl(fi, 'old', L.old_span?.start_line, L.content_old, lang)) : '';
    const right = R ? (wd ? wordHtml(fi, 'new', R.new_span?.start_line, R.content_new, wd, 'add', lang) : hl(fi, 'new', R.new_span?.start_line, R.content_new, lang)) : '';
    const noisy = S.hideNoise && (!L || L.noise) && (!R || R.noise);
    const lc = L ? 'l-del' : 'empty', rc = R ? 'r-add' : 'empty';
    const anchor = R ? 'n' + R.new_span?.start_line : 'o' + L.old_span?.start_line;
    return `<div class="row${noisy ? ' noise' : ''}" data-a="${anchor}"><span class="gl ${lc}">${L?.old_span?.start_line || ''}</span><span class="pf ${lc}">${L ? '−' : ''}</span><span class="cd ${lc}">${left}</span><span class="gl mid ${rc}">${R?.new_span?.start_line || ''}</span><span class="pf ${rc}">${R ? '+' : ''}</span><span class="cd ${rc}">${right}</span></div>`;
}

// Keep 3 lines of context around changes; longer runs become an inline expander.
function collapseContext(changes) {
    const keep = new Uint8Array(changes.length);
    changes.forEach((c, i) => {
        if (c.kind !== 'context') for (let j = Math.max(0, i - 3); j <= Math.min(changes.length - 1, i + 3); j++) keep[j] = 1;
    });
    const out = [];
    for (let i = 0; i < changes.length;) {
        if (keep[i]) { out.push({ type: 'change', change: changes[i++] }); continue; }
        const start = i;
        while (i < changes.length && !keep[i]) i++;
        const run = changes.slice(start, i);
        if (run.length <= 4) run.forEach(c => out.push({ type: 'change', change: c }));
        else out.push({ type: 'gap', count: run.length, changes: run });
    }
    return out;
}

const GAP_STEP = 20;

/** Reveal unchanged lines in a gap: `down` from its top, `up` from its bottom, or `all`. */
function expandGap(el, dir = 'all') {
    const fi = +el.dataset.expand, from = +el.dataset.from, to = +el.dataset.to, delta = +el.dataset.delta;
    const lines = splitLines(S.data.files[fi].new_source);
    const lang = fileLang(fi);
    const rows = (a, b) => {
        let html = '';
        for (let n = a; n <= b; n++) {
            const code = hl(fi, 'new', n, lines[n - 1] ?? '', lang);
            html += viewMode() === 'split'
                ? `<div class="row ctx"><span class="gl">${n + delta}</span><span class="pf"></span><span class="cd">${code}</span><span class="gl mid">${n}</span><span class="pf"></span><span class="cd">${code}</span></div>`
                : row('ctx', n + delta, n, ' ', code, '', 'n' + n);
        }
        return html;
    };
    const MAX_ALL = 400; // very long gaps still open in chunks
    if (dir === 'down' || (dir === 'all' && to - from + 1 > MAX_ALL)) {
        const end = Math.min(to, from + (dir === 'down' ? GAP_STEP : MAX_ALL) - 1);
        el.insertAdjacentHTML('beforebegin', rows(from, end));
        if (end < to) el.insertAdjacentHTML('beforebegin', expandRow(fi, end + 1, to, delta));
    } else if (dir === 'up') {
        const start = Math.max(from, to - GAP_STEP + 1);
        if (start > from) el.insertAdjacentHTML('beforebegin', expandRow(fi, from, start - 1, delta));
        el.insertAdjacentHTML('beforebegin', rows(start, to));
    } else {
        el.insertAdjacentHTML('beforebegin', rows(from, to));
    }
    el.remove();
}

// --- Navigation ---
function navigable() {
    return order.filter(k => {
        const [fi, hi] = k.split(':').map(Number);
        const h = S.data.results[fi].hunks[hi];
        return !(S.hideNoise && h.noise);
    });
}

function navigateChange(dir) {
    const list = navigable();
    if (!list.length) { toast('No changes to navigate' + (S.hideNoise ? ' (mechanical changes are hidden, press m)' : '')); return; }
    // Continue from the hunk nearest the top of the viewport.
    let idx = list.indexOf(order[S.currentHunk]);
    if (idx < 0) idx = dir > 0 ? -1 : list.length;
    const next = Math.max(0, Math.min(list.length - 1, idx + dir));
    const [fi, hi] = list[next].split(':').map(Number);
    revealHunk(fi, hi, null, false);
    S.currentHunk = order.indexOf(list[next]);
    updateNavCounter();
}

function onHunkVisible(entries) {
    for (const en of entries) {
        if (!en.isIntersecting) continue;
        const idx = order.indexOf(en.target.dataset.hunk);
        if (idx >= 0) { S.currentHunk = idx; updateNavCounter(); }
    }
}

function updateNavCounter() {
    const list = navigable();
    const cur = list.indexOf(order[S.currentHunk]);
    document.getElementById('nav-counter').textContent = list.length
        ? `${cur >= 0 ? `Change ${cur + 1} of ${list.length}` : `${list.length} change${pl(list.length)}`}${S.hideNoise && list.length < order.length ? ` · ${order.length - list.length} mechanical hidden` : ''}`
        : (order.length ? `${order.length} mechanical change${pl(order.length)} hidden` : 'No changes');
    const k = order[S.currentHunk];
    document.getElementById('nav-location').textContent = k ? S.data.files[+k.split(':')[0]].new_path : '';
    markCurrent();
    syncUrl();
}

/** Show where the reader is: the current hunk, and its file or section in the sidebar. */
function markCurrent() {
    const k = order[S.currentHunk];
    if (!k) return;
    const [fi, hi] = k.split(':').map(Number);
    const hunk = document.getElementById(`h-${fi}-${hi}`);
    if (!hunk?.classList.contains('current')) {
        document.querySelectorAll('.hunk.current').forEach(h => h.classList.remove('current'));
        hunk?.classList.add('current');
    }
    const shows = sidebarShows();
    const row = shows === 'files' ? document.querySelector(`.tree-file[data-fi="${fi}"]`)
        : shows === 'groups' ? document.querySelector(`.toc-group[data-gi="${hunkGroup.get(k)}"] > .toc-head`)
        : null;
    if (!row || row.classList.contains('current')) return;
    document.querySelectorAll('#toc-content .current').forEach(r => r.classList.remove('current'));
    row.classList.add('current');
    // Keep it in view inside the sidebar, without scrolling the page.
    const box = document.getElementById('toc-content');
    const r = row.getBoundingClientRect(), b = box.getBoundingClientRect();
    if (r.top < b.top + 8) box.scrollTop -= b.top + 8 - r.top;
    else if (r.bottom > b.bottom - 8) box.scrollTop += r.bottom - (b.bottom - 8);
}

/** Render the lazily rendered sections above `section`, so a scroll to it lands where it stays. */
function renderAbove(section) {
    for (const b of document.querySelectorAll('.section-body[data-pending]')) {
        const s = b.closest('.section');
        if (s !== section && !s.classList.contains('collapsed') && (s.compareDocumentPosition(section) & Node.DOCUMENT_POSITION_FOLLOWING)) renderSectionBody(s);
    }
}

/** Scroll to a hunk (rendering and un-collapsing whatever is needed). */
function revealHunk(fi, hi, line, flash = true, side = 'new') {
    const secKey = grouped() ? 'g' + hunkGroup.get(fi + ':' + hi) : 'f' + fi;
    const section = document.getElementById('s-' + secKey);
    if (!section) return;
    if (section.classList.contains('collapsed')) { section.classList.remove('collapsed'); S.collapsedSections.delete(secKey); }
    renderAbove(section);
    renderSectionBody(section);
    let el = document.getElementById(`h-${fi}-${hi}`);
    if (!el) return;
    const key = fi + ':' + hi;
    if (el.querySelector('.noise-row')) {
        S.expandedNoise.add(key);
        el.outerHTML = renderHunk(fi, hi, grouped());
        el = document.getElementById(`h-${fi}-${hi}`);
        hunkObserver?.observe(el);
    }
    el.closest('.noise-run')?.classList.add('open');
    let target = el;
    if (line) {
        const rowEl = el.querySelector(`[data-a="${side === 'old' ? 'o' : 'n'}${line}"]`);
        if (rowEl) {
            if (rowEl.parentElement?.hasAttribute('data-inline-lines')) { rowEl.parentElement.classList.remove('hidden'); rowEl.parentElement.previousElementSibling?.remove(); }
            target = rowEl;
            document.querySelectorAll('.row.target').forEach(r => r.classList.remove('target'));
            rowEl.classList.add('target');
        }
    }
    // A section's first hunk comes with its intro (what calls it, how it's tested): keep that in view.
    const intro = section.querySelector('.group-intro');
    if (target === el && intro && section.querySelector('.hunk') === el) target = intro;
    target.scrollIntoView({ block: target === el || target === intro ? 'start' : 'center', behavior: scrollBehavior() });
    if (flash) { el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash'); }
    const idx = order.indexOf(key);
    if (idx >= 0) { S.currentHunk = idx; updateNavCounter(); }
}

function jumpToEntry(id) {
    const e = E.get(id);
    const hs = entryHunks.get(id);
    if (hs && hs.length) { revealHunk(hs[0].fi, hs[0].hi, e?.line, true, e?.side); closeMobileSidebar(); return; }
    if (e && e.fi >= 0) { jumpToFile(e.fi, e.line); return; }
    toast(e?.path ? `${e.path}${e.line ? ':' + e.line : ''} is not part of this diff.` : 'This change has no location in the diff.');
}

function jumpToFile(fi, line) {
    closeMobileSidebar();
    if (line) {
        const hi = findHunkAt(fi, line, 'new');
        if (hi >= 0) { revealHunk(fi, hi, line); return; }
    }
    if (S.mode !== 'files') setMode('files');
    const section = document.getElementById('s-f' + fi);
    if (!section) return;
    section.classList.remove('collapsed'); S.collapsedSections.delete('f' + fi);
    renderAbove(section);
    renderSectionBody(section);
    section.scrollIntoView({ block: 'start', behavior: scrollBehavior() });
    if (line) toast(`Line ${line} is unchanged. Use “Show unchanged lines” in the file to see it.`);
}

function currentFileIdx() {
    const k = order[S.currentHunk];
    if (k) return +k.split(':')[0];
    // Fall back to the first file section in view.
    for (const s of document.querySelectorAll('.section[data-key^="f"]')) {
        const r = s.getBoundingClientRect();
        if (r.bottom > 120) return +s.dataset.key.slice(1);
    }
    return -1;
}

function jumpSection(dir) {
    const sections = [...document.querySelectorAll('.section')];
    const top = 100;
    let idx = sections.findIndex(s => s.getBoundingClientRect().top > top + 2);
    if (idx < 0) idx = sections.length;
    const target = dir > 0 ? sections[idx] : sections[Math.max(0, idx - 2)];
    target?.scrollIntoView({ block: 'start', behavior: scrollBehavior() });
}

// --- Reviewed state ---
function fileSignature(fi) {
    let h = 0;
    for (const hunk of S.data.results[fi].hunks) for (const c of hunk.changes) {
        const s = (c.kind[0]) + (c.content_new ?? c.content_old ?? '');
        for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) | 0;
    }
    return String(h);
}

function setViewed(fi, on) {
    const f = S.data.files[fi];
    if (on) S.viewed[f.new_path] = fileSignature(fi); else delete S.viewed[f.new_path];
    store.set('viewed:' + sourceKey(), S.viewed);
    if (on && S.changedSinceViewed[f.new_path]) {
        delete S.changedSinceViewed[f.new_path];
        store.set('changed:' + sourceKey(), S.changedSinceViewed);
        const head = document.querySelector(`#s-f${fi} .tag.changed`);
        head?.remove();
    }
    const section = document.getElementById('s-f' + fi);
    if (section) {
        section.classList.toggle('viewed', on);
        section.classList.toggle('collapsed', on);
        on ? S.collapsedSections.add('f' + fi) : S.collapsedSections.delete('f' + fi);
        const cb = section.querySelector('[data-viewed]'); if (cb) cb.checked = on;
        if (on && section.getBoundingClientRect().top < 0) section.scrollIntoView({ block: 'start' });
    }
    renderProgress();
    if (sidebarShows() === 'files') renderSidebar();
}

function setGroupReviewed(gi, on) {
    const g = groups[gi];
    if (on) S.groupsReviewed[g.key] = true; else delete S.groupsReviewed[g.key];
    store.set('groups:' + sourceKey(), S.groupsReviewed);
    const section = document.getElementById('s-g' + gi);
    if (section) {
        section.classList.toggle('collapsed', on); on ? S.collapsedSections.add('g' + gi) : S.collapsedSections.delete('g' + gi);
        const cb = section.querySelector('[data-group-reviewed]'); if (cb) cb.checked = on;
    }
    renderProgress();
    if (sidebarShows() === 'groups') renderSidebar();
}

/** The section being read: the current change's, else the one at the top of the screen. */
function currentGroupIdx() {
    const k = order[S.currentHunk];
    if (k && hunkGroup.has(k)) return hunkGroup.get(k);
    const secs = [...document.querySelectorAll('.section[id^="s-g"]')];
    const above = secs.filter(s => s.getBoundingClientRect().top <= 140);
    const sec = above[above.length - 1] || secs[0];
    return sec ? +sec.id.slice(3) : -1;
}

function jumpToGroup(gi, instant = false) {
    const section = document.getElementById('s-g' + gi);
    if (!section) return;
    if (section.classList.contains('collapsed') && !S.groupsReviewed[groups[gi].key]) {
        section.classList.remove('collapsed'); S.collapsedSections.delete('g' + gi);
    }
    renderAbove(section);
    renderSectionBody(section);
    section.scrollIntoView({ block: 'start', behavior: instant ? 'auto' : scrollBehavior() });
    const first = groupHunks(gi)[0];
    if (first) { S.currentHunk = order.indexOf(first.fi + ':' + first.hi); updateNavCounter(); }
}

// --- LLM analysis ---
/** No LLM yet: how to set one up. Keys stay in the user's environment; the viewer never asks for one. */
function showSetup(note = '') {
    const setup = S.data.capabilities?.llm_setup || {};
    const cmd = c => `<div class="cmd"><code>${esc(c)}</code><button class="copy-btn" data-copy="${esc(c)}" title="Copy" aria-label="Copy">${IC.copy}</button></div>`;
    const claude = setup.claude === 'logged_out'
        ? `<li><b>Claude Code</b> is installed but not logged in. Log in, then check again:${cmd('claude auth login')}</li>`
        : `<li><b><a href="https://claude.com/claude-code" target="_blank" rel="noopener">Claude Code</a></b>: perspica uses its login, no API key needed. Install it and log in, then check again:${cmd('claude auth login')}</li>`;
    showDialog({
        title: 'Set up the analysis',
        body: `<p class="muted">Optional. With an LLM, perspica groups the changes by what they're for, rates how risky each group is and writes a summary. Everything else works without one.</p>
            <ol class="setup">
                ${claude}
                <li><b>An API key</b>: add it to your shell profile (<code>~/.zshrc</code> or <code>~/.bashrc</code>), open a new terminal and run perspica again:${cmd('export ANTHROPIC_API_KEY=sk-ant-…')}<span class="muted">or <code>OPENAI_API_KEY</code></span></li>
                <li><b>A local model</b>: ${setup.ollama_no_model ? 'Ollama is running but has no model yet. Download one' : `install <a href="https://ollama.com" target="_blank" rel="noopener">Ollama</a> and download a model`}, then check again:${cmd(`ollama pull ${setup.ollama_suggested || 'gemma4:12b'}`)}<span class="muted">Needs about 16 GB of RAM; slower than a hosted model.</span></li>
            </ol>
            ${note ? `<p class="setup-note">${esc(note)}</p>` : ''}`,
        actions: [
            { label: 'Close' },
            { label: 'Check again', primary: true, busy: 'Checking…', onClick: checkForLlm },
        ],
    });
}

async function checkForLlm() {
    try {
        const resp = await fetch('/api/llm/check', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{}' });
        const cap = await resp.json();
        if (!resp.ok) throw new Error(cap.error || `HTTP ${resp.status}`);
        S.data.capabilities = cap;
        renderHeader();
        if (!cap.llm) { showSetup(cap.llm_setup?.claude === 'logged_out' ? 'Claude Code still isn\'t logged in.' : 'Still no LLM found. A new API key needs a restart of perspica.'); return; }
        renderOverview(); renderSidebar();
        toast(`Found ${cap.llm}. Ready to analyze.`);
        startAnalysis();
    } catch (e) {
        showSetup(`Couldn't check: ${e.message}`);
    }
}

function startAnalysis() {
    if (S.running) return;
    const cap = S.data.capabilities || {};
    if (!cap.llm) { showSetup(); return; }
    const models = cap.models || [];
    const saved = store.get('analysis', {});
    const model = saved.model && (models.some(m => m.id === saved.model) || !models.length) ? saved.model : cap.model;
    const depth = saved.depth === 'thorough' ? 'thorough' : 'standard';
    const modelField = models.length
        ? `<select id="an-model" class="field">${models.map(m => `<option value="${esc(m.id)}"${m.id === model ? ' selected' : ''}>${esc(m.label)} · ${esc(m.note)}</option>`).join('')}${models.some(m => m.id === cap.model) ? '' : `<option value="${esc(cap.model)}" selected>${esc(cap.model)} (from --model)</option>`}</select>`
        : `<input id="an-model" class="field mono" value="${esc(model || '')}" spellcheck="false" aria-label="Model">`;
    const reqs = S.data.source?.sessions?.requirements?.length || 0;
    // When it's bigger than what's sent by default, say how big and offer to send all of it.
    const over = cap.context_budget && cap.context_chars > cap.context_budget;
    const tokens = n => `about ${Math.round(n / 4000)}K tokens`;
    const sizeNote = over ? `<div class="size-note">
            <p>This change has <b>${cap.context_chars.toLocaleString()}</b> characters of changed code. By default ${esc(cap.llm)} gets the first ${cap.context_budget.toLocaleString()} (${tokens(cap.context_budget)}), and the model is told what it can't see.</p>
            <label class="check"><input type="checkbox" id="an-full"> Send all of it (${tokens(cap.context_chars)})</label>
        </div>` : '';
    const option = (value, title, desc) => `<label class="choice"><input type="radio" name="an-depth" value="${value}"${depth === value ? ' checked' : ''}><span><b>${title}</b><span class="choice-desc">${desc}</span><span class="choice-time" data-time="${value}"></span></span></label>`;
    showDialog({
        title: hasLlmGroups() ? 'Re-analyze this change' : 'Analyze this change',
        body: `<p class="muted">Groups the changes by what they're for, rates how risky each group is, and writes a short summary${reqs ? ', and marks which work you asked for and which the agent decided on its own' : ''}.</p>
            <label class="field-label">Model <span class="muted">via ${esc(cap.llm)}</span></label>${modelField}
            <div class="field-label">Depth</div>
            <div class="choices">
                ${option('standard', 'Standard', 'One request with the change list and the changed code.')}
                ${option('thorough', 'Thorough', 'The model first reads definitions and small files it asks for from the changed files, then answers. Better on unfamiliar code; slower and uses more tokens.')}
            </div>
            ${sizeNote}
            <details class="sent"><summary>What gets sent to ${esc(cap.llm)}</summary><ul>
                <li>the classified changes (names, locations) and the changed code, never whole files${depth === 'thorough' ? '' : ''}</li>
                <li>with Thorough: definitions and files under 200 lines it asks for, only from the changed files</li>
                ${reqs ? `<li>your ${reqs} prompt${pl(reqs)} from the Claude Code session that made the change (turn off with <code>--no-sessions</code>)</li>` : ''}
                ${S.data.source?.pr_title || S.data.source?.label?.includes('..') ? '<li>the PR description or commit messages</li>' : ''}
            </ul></details>`,
        actions: [
            { label: 'Cancel' },
            { label: 'Analyze', primary: true, onClick: () => {
                const m = document.getElementById('an-model')?.value.trim() || cap.model;
                const dp = document.querySelector('input[name="an-depth"]:checked')?.value || 'standard';
                const full = !!document.getElementById('an-full')?.checked;
                store.set('analysis', { model: m, depth: dp });
                runAnalysis({ model: m, depth: dp, full });
            } },
        ],
    });
    const updateTimes = () => {
        const m = document.getElementById('an-model')?.value || cap.model;
        document.querySelectorAll('[data-time]').forEach(el => { el.textContent = timeHint(cap, el.dataset.time === 'thorough', m); });
    };
    document.getElementById('an-model')?.addEventListener('change', updateTimes);
    updateTimes();
}

/**
 * Rough duration. Calibrated on Claude Code runs (~70s for 14 items, ~194s for
 * 83 units on Sonnet; Haiku ~4× faster); other providers are usually faster.
 */
function timeHint(cap, thorough, model) {
    const units = cap.llm_units || 0;
    if (!units) return '';
    const speed = /haiku/i.test(model || '') ? 0.3 : /sonnet/i.test(model || '') ? 0.8 : /fable/i.test(model || '') ? 1.6 : 1;
    const secs = (cap.llm === 'Claude Code' ? 60 + units * 1.6 : 20 + units * 0.8) * speed * (thorough ? 2.2 : 1);
    const mins = secs / 60;
    return mins < 1.2 ? '≈ 1 min' : `≈ ${Math.round(mins)} min`;
}

/** Put a dot on the tab icon while an analysis runs, so it's visible from other tabs. */
function setFaviconBusy(on) {
    const link = document.querySelector('link[rel="icon"]');
    if (!link) return;
    if (!on) { link.href = '/favicon.svg'; return; }
    const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><g fill="none" stroke-linecap="round" transform="translate(1.4 -1)"><path stroke="#57606a" stroke-width="2.8" d="M7.6 13.8V28.4"/><circle stroke="#57606a" stroke-width="2.8" cx="15.2" cy="13.8" r="7.6"/><path stroke="#58a6ff" stroke-width="3" d="M11.3 13.8h7.8"/></g><circle cx="26" cy="26" r="5.5" fill="#d29922"/></svg>`;
    link.href = 'data:image/svg+xml,' + encodeURIComponent(svg);
}

async function runAnalysis({ model, depth, full }) {
    const btn = document.getElementById('analyze-btn');
    setFaviconBusy(true);
    const cap = S.data.capabilities || {};
    S.running = { model, provider: cap.llm, estimate: timeHint(cap, depth === 'thorough', model) };
    btn.disabled = true;
    renderOverview();
    const started = Date.now();
    const tick = () => { btn.textContent = `Analyzing… ${Math.round((Date.now() - started) / 1000)}s`; };
    tick();
    const timer = setInterval(tick, 1000);
    try {
        const resp = await fetch('/api/analyze', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ model, depth, full: !!full }) });
        const result = await resp.json().catch(() => ({ error: `HTTP ${resp.status}` }));
        if (!resp.ok || result.error) throw new Error(result.error || `HTTP ${resp.status}`);
        S.data.intent_groups = result.groups;
        S.data.summary = result.summary || S.data.summary;
        S.data.concerns = result.concerns || [];
        S.data.llm_error = null;
        S.data.llm_model = result.model;
        S.data.llm_saved_at = null;
        S.mode = 'intent'; store.set('mode', 'intent'); store.set('lastGrouped', 'intent');
        derive();
        renderHeader(); renderOverview(); renderSidebar(); renderMain();
        toast(`Grouped by intent with ${modelLabel(result.model)} in ${result.seconds}s${depth === 'thorough' ? ` (${result.tool_calls_made} lookups)` : ''}.`, 'ok');
    } catch (e) {
        toast('Analysis failed: ' + truncate(e.message, 300), 'error', 9000);
    } finally {
        clearInterval(timer);
        setFaviconBusy(false);
        S.running = false;
        btn.disabled = false;
        renderHeader(); renderOverview();
    }
}

/** "3 minutes ago" for a unix timestamp. */
function ago(unix) {
    const secs = Math.max(0, Date.now() / 1000 - unix);
    if (secs < 90) return 'just now';
    const [n, unit] = secs < 5400 ? [Math.round(secs / 60), 'minute'] : secs < 129600 ? [Math.round(secs / 3600), 'hour'] : [Math.round(secs / 86400), 'day'];
    return `${n} ${unit}${n === 1 ? '' : 's'} ago`;
}

function modelLabel(id) {
    return (S.data.capabilities?.models || []).find(m => m.id === id)?.label || id || 'the model';
}

// --- Mode / view switches ---
function setMode(mode) {
    if (mode === 'flow' && !hasFlow()) { toast('No call flow in this diff.'); return; }
    const prev = S.mode;
    S.mode = mode;
    if ((mode === 'flow') !== (prev === 'flow')) buildGroups();
    if (mode === 'intent' && !groups.length) { S.mode = prev; buildGroups(); toast('Nothing to group.'); return; }
    store.set('mode', mode);
    if (mode !== 'files') store.set('lastGrouped', mode);
    S.collapsedSections = new Set([...S.collapsedSections].filter(k => k[0] === 'f'));
    renderSidebar();
    S.currentHunk = -1;
    renderHeader(); renderOverview(); renderMain();
    window.scrollTo({ top: 0 });
    syncUrl();
}
function setView(view) {
    if (view === S.view) return;
    S.view = view; store.set('view', view);
    renderHeader(); renderMain(true);
}
function setHideNoise(on) {
    S.hideNoise = on; store.set('hideNoise', on);
    S.expandedNoise.clear();
    renderHeader(); renderOverview(); renderMain(true);
}
function setWrap(on) {
    S.wrap = on; store.set('wrap', on);
    applyBodyClasses();
    document.getElementById('wrap-toggle').checked = on;
}
function toggleViewMenu() {
    const menu = document.getElementById('view-menu');
    const open = menu.classList.toggle('hidden') === false;
    document.getElementById('view-btn').setAttribute('aria-expanded', String(open));
}
function closeViewMenu() {
    document.getElementById('view-menu').classList.add('hidden');
    document.getElementById('view-btn').setAttribute('aria-expanded', 'false');
}
function applyBodyClasses() {
    document.body.classList.toggle('wrap', S.wrap);
    document.body.classList.toggle('sidebar-collapsed', store.get('sidebarCollapsed', false) && !isMobile());
}
function isMobile() { return window.matchMedia('(max-width: 860px)').matches; }
/** Split view needs room; narrow screens always get unified. */
function viewMode() { return isMobile() ? 'unified' : S.view; }
function toggleSidebar() {
    if (isMobile()) { document.body.classList.toggle('sidebar-open'); return; }
    const c = !document.body.classList.contains('sidebar-collapsed');
    document.body.classList.toggle('sidebar-collapsed', c);
    store.set('sidebarCollapsed', c);
}
function closeMobileSidebar() { document.body.classList.remove('sidebar-open'); }

function toggleTheme() {
    const root = document.documentElement;
    const cur = root.getAttribute('data-theme') || (window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark');
    const next = cur === 'dark' ? 'light' : 'dark';
    root.setAttribute('data-theme', next);
    try { localStorage.setItem('perspica-theme', next); } catch {}
}

// --- Events ---
function setupListeners() {
    document.getElementById('mode-toggle').addEventListener('click', e => { const b = e.target.closest('[data-mode]'); if (b) setMode(b.dataset.mode); });
    document.getElementById('view-toggle').addEventListener('click', e => { const b = e.target.closest('[data-view]'); if (b) setView(b.dataset.view); });
    document.getElementById('noise-toggle').addEventListener('change', e => setHideNoise(e.target.checked));
    document.getElementById('analyze-btn').addEventListener('click', () => { if (!S.static) startAnalysis(); });
    document.getElementById('wrap-toggle').addEventListener('change', e => setWrap(e.target.checked));
    document.getElementById('theme-toggle').addEventListener('click', () => { toggleTheme(); closeViewMenu(); });
    document.getElementById('help-btn').addEventListener('click', () => document.getElementById('shortcut-modal').classList.toggle('hidden'));
    document.getElementById('view-btn').addEventListener('click', e => { e.stopPropagation(); toggleViewMenu(); });
    document.getElementById('view-menu').addEventListener('click', e => e.stopPropagation());
    document.addEventListener('click', closeViewMenu);
    document.getElementById('sidebar-toggle').addEventListener('click', toggleSidebar);
    document.getElementById('sidebar-scrim').addEventListener('click', closeMobileSidebar);
    document.getElementById('prev-change').addEventListener('click', () => navigateChange(-1));
    document.getElementById('next-change').addEventListener('click', () => navigateChange(1));
    document.getElementById('unviewed-toggle').addEventListener('click', e => {
        S.onlyUnviewed = !S.onlyUnviewed;
        e.currentTarget.setAttribute('aria-pressed', String(S.onlyUnviewed));
        applyFilter(document.getElementById('toc-filter').value);
    });
    const filter = document.getElementById('toc-filter');
    filter.addEventListener('input', e => applyFilter(e.target.value));
    // Escape and the clear button empty a search field without an input event.
    filter.addEventListener('search', e => applyFilter(e.target.value));
    document.querySelectorAll('.sidebar-tab').forEach(b => b.addEventListener('click', () => { S.tab = b.dataset.tab; renderSidebar(); }));
    document.querySelectorAll('.modal-backdrop').forEach(b => b.addEventListener('click', () => b.parentElement.classList.add('hidden')));

    // Delegated clicks for dynamic content.
    document.addEventListener('click', e => {
        const t = e.target;
        const a = t.closest('[data-action]');
        if (a) {
            const act = a.dataset.action;
            if (act === 'toggle-overview') { S.overviewCollapsed = !S.overviewCollapsed; store.set('overviewCollapsed', S.overviewCollapsed); document.getElementById('overview').classList.toggle('collapsed', S.overviewCollapsed); }
            else if (act === 'toggle-noise') setHideNoise(!S.hideNoise);
            else if (act === 'analyze' && !S.static) startAnalysis();
            else if (act === 'mode-intent') setMode('intent');
            else if (act === 'mode-flow') setMode('flow');
            else if (act === 'toggle-check') { S.checkOpen = !S.checkOpen; renderOverview(); }
            else if (act === 'toggle-asked') { S.askedOpen = !S.askedOpen; renderOverview(); }
            else if (act === 'tab-changes') { S.tab = 'changes'; renderSidebar(); if (isMobile()) document.body.classList.add('sidebar-open'); }
            return;
        }
        const run = t.closest('[data-open-run]');
        if (run) {
            const box = run.closest('.noise-run');
            const keys = box.dataset.run.split(',');
            keys.forEach(k => S.expandedNoise.add(k));
            box.outerHTML = keys.map(k => { const [fi, hi] = k.split(':').map(Number); return renderHunk(fi, hi, box.dataset.withFile === '1'); }).join('');
            keys.forEach(k => { const el = document.getElementById('h-' + k.replace(':', '-')); if (el) hunkObserver?.observe(el); });
            return;
        }
        const rq = t.closest('[data-req]');
        if (rq) {
            S.overviewCollapsed = false; S.askedOpen = true; renderOverview();
            const li = document.getElementById('req-' + rq.dataset.req);
            if (li) { li.scrollIntoView({ block: 'center', behavior: 'smooth' }); li.classList.remove('flash'); void li.offsetWidth; li.classList.add('flash'); }
            return;
        }
        const copy = t.closest('[data-copy]');
        if (copy) { e.stopPropagation(); copyText(copy.dataset.copy, copy); return; }
        if (t.closest('.viewed-toggle')) return; // handled by change event
        const jf = t.closest('[data-jump-file]');
        if (jf) { jumpToFile(fileByPath.get(jf.dataset.jumpFile), +jf.dataset.jumpLine || 0); return; }
        const sn = t.closest('[data-show-noise]');
        if (sn) {
            const key = sn.dataset.showNoise; S.expandedNoise.add(key);
            const [fi, hi] = key.split(':').map(Number);
            const el = document.getElementById(`h-${fi}-${hi}`);
            el.outerHTML = renderHunk(fi, hi, grouped());
            hunkObserver?.observe(document.getElementById(`h-${fi}-${hi}`));
            return;
        }
        const ex = t.closest('[data-expand]');
        if (ex) { expandGap(ex, t.closest('[data-dir]')?.dataset.dir || 'all'); return; }
        const ie = t.closest('[data-inline-expand]');
        if (ie) { ie.nextElementSibling?.classList.remove('hidden'); ie.remove(); return; }
        const gf = t.closest('[data-goto-file]');
        if (gf) { const fi = +gf.dataset.gotoFile; setMode('files'); setTimeout(() => jumpToFile(fi), 0); return; }
        const st = t.closest('[data-section-toggle]');
        if (st) {
            const section = st.closest('.section');
            const c = section.classList.toggle('collapsed');
            c ? S.collapsedSections.add(section.dataset.key) : S.collapsedSections.delete(section.dataset.key);
            if (!c) renderSectionBody(section);
            return;
        }
        // Sidebar
        const fileRow = t.closest('.tree-file');
        if (fileRow) { jumpToFile(+fileRow.dataset.fi); markCurrentTreeRow(fileRow); return; }
        const dirRow = t.closest('[data-dir]');
        if (dirRow) { dirRow.parentElement.classList.toggle('collapsed'); return; }
        const entry = t.closest('[data-entry]');
        if (entry) { jumpToEntry(+entry.dataset.entry); return; }
        const gt = t.closest('[data-group-toggle]');
        if (gt) { gt.closest('.toc-group').classList.toggle('collapsed'); return; }
        const jg = t.closest('[data-jump-group]');
        if (jg) {
            const gi = +jg.dataset.jumpGroup;
            if (!grouped()) setMode('intent');
            closeMobileSidebar();
            setTimeout(() => jumpToGroup(gi), 0);
        }
    });
    document.addEventListener('change', e => {
        const v = e.target.closest('[data-viewed]');
        if (v) { setViewed(+v.dataset.viewed, v.checked); return; }
        const ck = e.target.closest('[data-check]');
        if (ck) { setChecked(ck.dataset.check, ck.checked, ck); return; }
        const g = e.target.closest('[data-group-reviewed]');
        if (g) setGroupReviewed(+g.dataset.groupReviewed, g.checked);
    });
    document.addEventListener('keydown', e => {
        if (e.key === 'Enter' && e.target.matches('.expand[data-expand]')) { expandGap(e.target); return; }
        const modalOpen = [...document.querySelectorAll('.modal')].find(m => !m.classList.contains('hidden'));
        if (e.key === 'Escape') {
            if (!document.getElementById('view-menu').classList.contains('hidden')) { closeViewMenu(); return; }
            if (modalOpen) { modalOpen.classList.add('hidden'); e.preventDefault(); return; }
            if (document.body.classList.contains('sidebar-open')) { closeMobileSidebar(); return; }
            if (e.target.id === 'toc-filter') { e.target.value = ''; applyFilter(''); e.target.blur(); return; }
            if (e.target.tagName === 'INPUT') { e.target.blur(); return; }
        }
        if (modalOpen || e.metaKey || e.ctrlKey || e.altKey) return;
        if (['INPUT', 'TEXTAREA', 'SELECT'].includes(e.target.tagName)) return;
        const keys = {
            j: () => navigateChange(1), k: () => navigateChange(-1),
            n: () => jumpSection(1), p: () => jumpSection(-1),
            f: () => setMode('files'), i: () => setMode('intent'), r: () => setMode('flow'),
            u: () => setView('unified'), s: () => setView('split'),
            m: () => setHideNoise(!S.hideNoise),
            w: () => setWrap(!S.wrap),
            b: toggleSidebar, t: toggleTheme,
            v: () => {
                // Sections: mark the one being read and go to the next unviewed one.
                if (grouped() && groups.length) {
                    const gi = currentGroupIdx();
                    if (gi < 0) return;
                    const on = !S.groupsReviewed[groups[gi].key];
                    setGroupReviewed(gi, on);
                    if (!on) return;
                    const next = groups.findIndex((g, i) => i > gi && !S.groupsReviewed[g.key]);
                    const any = next >= 0 ? next : groups.findIndex(g => !S.groupsReviewed[g.key]);
                    if (any >= 0) jumpToGroup(any, true); else toast('All sections viewed.');
                    return;
                }
                const fi = currentFileIdx();
                if (fi < 0) return;
                const on = !S.viewed[S.data.files[fi].new_path];
                setViewed(fi, on);
                // Marking viewed moves on to the next unviewed file (Gerrit's "reviewed, next").
                if (on && S.mode === 'files') {
                    const ord = fileOrder();
                    const next = ord.slice(ord.indexOf(fi) + 1).find(i => !S.viewed[S.data.files[i].new_path]) ?? -1;
                    if (next >= 0) jumpToFile(next);
                }
            },
            '/': () => { if (isMobile()) document.body.classList.add('sidebar-open'); document.getElementById('toc-filter').focus(); },
            '?': () => document.getElementById('shortcut-modal').classList.toggle('hidden'),
        };
        const fn = keys[e.key];
        // Keyboard moves jump instantly: smooth scrolls pile up when a key is held.
        if (fn) { instantScroll = true; try { fn(); } finally { instantScroll = false; } e.preventDefault(); }
    });
    window.matchMedia('(max-width: 860px)').addEventListener('change', () => { closeMobileSidebar(); applyBodyClasses(); if (S.view === 'split') renderMain(true); });
}

function markCurrentTreeRow(row) {
    document.querySelectorAll('.tree-row.current').forEach(r => r.classList.remove('current'));
    row.classList.add('current');
}

function setupSidebarResize() {
    const handle = document.getElementById('sidebar-resize');
    const sidebar = document.getElementById('sidebar');
    const saved = store.get('sidebarWidth', 0);
    if (saved) sidebar.style.width = saved + 'px';
    handle.addEventListener('mousedown', (e) => {
        e.preventDefault();
        const startX = e.clientX, startW = sidebar.offsetWidth;
        handle.classList.add('dragging');
        const move = (ev) => { sidebar.style.width = Math.max(200, Math.min(560, startW + ev.clientX - startX)) + 'px'; };
        const up = () => {
            handle.classList.remove('dragging');
            store.set('sidebarWidth', sidebar.offsetWidth);
            document.removeEventListener('mousemove', move); document.removeEventListener('mouseup', up);
        };
        document.addEventListener('mousemove', move); document.addEventListener('mouseup', up);
    });
}

// --- UI helpers: toast, dialog, clipboard ---
function toast(msg, kind = '', ms = 4500) {
    const area = document.getElementById('toast-area');
    const el = document.createElement('div');
    el.className = 'toast ' + kind;
    el.innerHTML = `<span>${esc(msg)}</span><button class="x" aria-label="Dismiss">✕</button>`;
    el.querySelector('.x').onclick = () => el.remove();
    area.appendChild(el);
    while (area.children.length > 3) area.firstChild.remove();
    setTimeout(() => el.remove(), ms);
}

function showDialog({ title, body, actions }) {
    const dlg = document.getElementById('dialog');
    document.getElementById('dialog-title').textContent = title;
    document.getElementById('dialog-body').innerHTML = body;
    const act = document.getElementById('dialog-actions');
    act.innerHTML = '';
    for (const a of actions) {
        const b = document.createElement('button');
        b.className = a.primary ? 'accent-btn primary' : 'plain-btn';
        b.textContent = a.label;
        // `busy`: the action is async and the dialog stays open, showing it, until it settles.
        b.onclick = async () => {
            if (!a.busy) { dlg.classList.add('hidden'); a.onClick?.(); return; }
            b.disabled = true; b.textContent = a.busy;
            try { await a.onClick?.(); } finally { b.disabled = false; b.textContent = a.label; }
        };
        act.appendChild(b);
    }
    dlg.classList.remove('hidden');
    act.lastChild?.focus();
}

async function copyText(text, btn) {
    try {
        await navigator.clipboard.writeText(text);
        btn.classList.add('copied'); btn.innerHTML = IC.check;
        setTimeout(() => { btn.classList.remove('copied'); btn.innerHTML = IC.copy; }, 1200);
    } catch { toast('Clipboard unavailable'); }
}

// --- Text helpers ---
/** Model-written text: `code` spans rendered as code, linked to the change when they name one. */
function richText(s, links = true) {
    return esc(s).replace(/`([^`\n]{1,120})`/g, (_, code) => {
        const id = links ? entryIdByName(code) : undefined;
        return id === undefined ? `<code>${code}</code>` : `<button class="code-link" data-entry="${id}" title="Go to this change"><code>${code}</code></button>`;
    });
}

/** Changed item named by `name` (escaped), matching the full name or its last segment. */
function entryIdByName(name) {
    if (!nameIndex) {
        nameIndex = new Map();
        for (const e of E.values()) {
            const full = (e.short || String(e.text).split(/: | → /)[0]).replace(/^[+−~] /, '').replace(/ \([^)]*\)$/, '');
            for (const k of [full, full.split(/::|\./).pop()]) if (k && !nameIndex.has(esc(k))) nameIndex.set(esc(k), e.id);
        }
    }
    const key = name.replace(/\(\)$/, '');
    return nameIndex.get(key) ?? nameIndex.get(key.split(/::|\./).pop());
}

function esc(s) { return s == null ? '' : String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;'); }
function pl(n) { return n === 1 ? '' : 's'; }
function truncate(s, n) { s = String(s ?? ''); return s.length > n ? s.slice(0, n - 1) + '…' : s; }
function shortPath(p) { if (!p) return ''; const parts = p.split('/'); return parts.length > 3 ? '…/' + parts.slice(-2).join('/') : p; }
function baseName(p) { return (p || '').split('/').pop(); }
function splitLines(s) { if (!s) return []; const l = s.split('\n'); if (l[l.length - 1] === '') l.pop(); return l; }
function fileCounts(fi) {
    let add = 0, del = 0;
    for (const h of S.data.results[fi].hunks) for (const c of h.changes) { if (c.kind === 'added') add++; else if (c.kind === 'removed') del++; }
    return { add, del };
}

const IC = {
    copy: '<svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M0 6.75C0 5.784.784 5 1.75 5h1.5a.75.75 0 0 1 0 1.5h-1.5a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-1.5a.75.75 0 0 1 1.5 0v1.5A1.75 1.75 0 0 1 9.25 16h-7.5A1.75 1.75 0 0 1 0 14.25Z"/><path d="M5 1.75C5 .784 5.784 0 6.75 0h7.5C15.216 0 16 .784 16 1.75v7.5A1.75 1.75 0 0 1 14.25 11h-7.5A1.75 1.75 0 0 1 5 9.25Zm1.75-.25a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-7.5a.25.25 0 0 0-.25-.25Z"/></svg>',
    check: '<svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M13.78 4.22a.75.75 0 0 1 0 1.06l-7.25 7.25a.75.75 0 0 1-1.06 0L2.22 9.28a.75.75 0 0 1 1.06-1.06L6 10.94l6.72-6.72a.75.75 0 0 1 1.06 0Z"/></svg>',
};

// --- Syntax highlighting (lazy, whole-file context) ---
const LANG_MAP = { TypeScript: 'typescript', Tsx: 'typescript', Python: 'python', Rust: 'rust', Go: 'go', Java: 'java', C: 'c', Scala: 'scala', CSharp: 'csharp', Kotlin: 'kotlin', Php: 'php', Ruby: 'ruby' };
const EXT_MAP = { js: 'javascript', mjs: 'javascript', cjs: 'javascript', json: 'json', css: 'css', html: 'xml', xml: 'xml', svg: 'xml', md: 'markdown', yml: 'yaml', yaml: 'yaml', sh: 'bash', toml: 'ini', ini: 'ini', sql: 'sql', rb: 'ruby', kt: 'kotlin', kts: 'kotlin', swift: 'swift', cpp: 'cpp', hpp: 'cpp', cs: 'csharp', php: 'php' };
const MAX_HIGHLIGHT_CHARS = 400_000;

function fileLang(fi) {
    const f = S.data.files[fi];
    if (LANG_MAP[f.language]) return LANG_MAP[f.language];
    return EXT_MAP[(f.new_path.split('.').pop() || '').toLowerCase()] || null;
}

function highlightFile(fi) {
    if (hlCache.has(fi)) return hlCache.get(fi);
    let entry = null;
    const lang = fileLang(fi);
    const f = S.data.files[fi];
    if (typeof hljs !== 'undefined' && lang && hljs.getLanguage(lang) && (f.old_source.length + f.new_source.length) < MAX_HIGHLIGHT_CHARS) {
        try {
            entry = {
                old: splitHighlighted(hljs.highlight(f.old_source || '', { language: lang, ignoreIllegals: true }).value),
                new: splitHighlighted(hljs.highlight(f.new_source || '', { language: lang, ignoreIllegals: true }).value),
            };
        } catch { entry = null; }
    }
    hlCache.set(fi, entry);
    return entry;
}

// Split highlighted HTML into lines, re-opening spans that cross line breaks.
function splitHighlighted(html) {
    const out = [];
    let open = [];
    for (const raw of html.split('\n')) {
        let line = open.join('') + raw;
        const tags = raw.match(/<span[^>]*>|<\/span>/g) || [];
        for (const t of tags) t === '</span>' ? open.pop() : open.push(t);
        out.push(line + '</span>'.repeat(open.length));
    }
    return out;
}

function hl(fi, side, lineNum, text, lang) {
    const cache = highlightFile(fi);
    const lines = cache && (side === 'old' ? cache.old : cache.new);
    if (lines && lineNum > 0 && lineNum <= lines.length) {
        const cached = lines[lineNum - 1];
        // Guard against misaligned lines (e.g. CRLF handling differences).
        if (plainLen(cached) === (text || '').length) return cached;
    }
    return esc(text || '');
}

function plainLen(html) {
    return html.replace(/<[^>]*>/g, '').replace(/&(#\d+|#x[\da-f]+|\w+);/gi, 'x').length;
}

// --- Word diff ---
function wordDiff(a, b) {
    let p = 0; const min = Math.min(a.length, b.length);
    while (p < min && a[p] === b[p]) p++;
    let s = 0;
    while (s < min - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
    return { prefix: p, delA: a.length - p - s, delB: b.length - p - s };
}

function wordHtml(fi, side, lineNum, text, wd, type, lang) {
    const len = type === 'del' ? wd.delA : wd.delB;
    const base = hl(fi, side, lineNum, text, lang);
    // Skip highlighting when the whole line changed (no signal) or nothing did.
    if (len === 0 || len === (text || '').length) return base;
    return wrapRange(base, wd.prefix, wd.prefix + len, type === 'del' ? 'word-del' : 'word-add');
}

// Wrap plain-text range [from, to) of an HTML string in a span, counting
// entities as one character and closing/reopening across tag boundaries.
function wrapRange(html, from, to, cls) {
    let out = '', pos = 0, inside = false;
    const open = `<span class="${cls}">`;
    for (let i = 0; i < html.length;) {
        if (html[i] === '<') {
            const end = html.indexOf('>', i);
            const tag = html.slice(i, end + 1);
            out += inside ? '</span>' + tag + open : tag;
            i = end + 1;
            continue;
        }
        if (pos === from && !inside && from < to) { out += open; inside = true; }
        let ch = html[i];
        if (ch === '&') { const end = html.indexOf(';', i); if (end > i && end - i < 10) ch = html.slice(i, end + 1); }
        out += ch;
        i += ch.length;
        pos++;
        if (pos === to && inside) { out += '</span>'; inside = false; }
    }
    if (inside) out += '</span>';
    return out;
}

init();
