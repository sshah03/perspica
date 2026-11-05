// Run with: node --test crates/perspica-cli/tests/highlighting.mjs
// Requires agent-browser and Chrome; no CDN access or model calls.
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { promisify } from 'node:util';
import test from 'node:test';

const exec = promisify(execFile);
const web = new URL('../web/', import.meta.url);

function fixture() {
    const oldSource = 'export function answer(): number { return 1; }\n';
    const newSource = 'export function answer(): number { return 2; }\n';
    const location = { file: 'answer.ts', line_start: 1, line_end: 1, side: 'new' };
    const span = { start_line: 1, end_line: 1, start_col: 0, end_col: newSource.length - 1 };
    return {
        version: '0.3.0', source: { label: 'Highlighting regression' },
        files: [{ old_path: 'answer.ts', new_path: 'answer.ts', old_source: oldSource, new_source: newSource, language: 'TypeScript', status: 'M', binary: false }],
        results: [{
            hunks: [{
                old_range: { start: 1, end: 1 }, new_range: { start: 1, end: 1 }, manifest_refs: [1],
                changes: [
                    { kind: 'removed', content_old: oldSource.trimEnd(), old_span: span },
                    { kind: 'added', content_new: newSource.trimEnd(), new_span: span },
                ],
            }],
            manifest: {
                renames: [], signature_changes: [], extracted_functions: [], moved_code: [], dependency_changes: [], dead_code: [], formatting_only: [], test_entries: [],
                logic_changes: [{ id: 1, name: 'answer', description: 'body modified', location }],
            },
            review: { changed_lines: 2, mechanical_lines: 0, parsed: true, generated: false, role: 'source', test_lines: 0 },
        }],
        cross_file: { reading_order: [], moves: [], broken_references: [], signature_impacts: [], test_reach: [] },
        concerns: [],
    };
}

test('a grammar loaded after the first render replaces cached plain text', async t => {
    const assets = new Map(await Promise.all(['index.html', 'app.js', 'style.css', 'favicon.svg'].map(async name => [name, await readFile(new URL(name, web), 'utf8')])));
    assets.set('index.html', assets.get('index.html').replace(/<script src="(https:[^"]+)"[^>]*><\/script>/g, (_, src) => {
        if (src.endsWith('/highlight.min.js')) return '<script src="/highlight.js"></script>';
        if (src.endsWith('/languages/typescript.min.js')) return '<script src="/typescript.js" defer></script>';
        return '';
    }));
    // Minimal Highlight.js interface: core is already available, but TypeScript is not registered yet.
    assets.set('highlight.js', `window.hljs = {
        getLanguage(name) {
            if (name === 'typescript' && !window.grammarLoaded && !window.renderedBeforeGrammar) {
                window.renderedBeforeGrammar = true;
                fetch('/release-grammar');
            }
            return name === 'typescript' && window.grammarLoaded;
        },
        highlight(source) { return { value: source.replaceAll('export', '<span class="hljs-keyword">export</span>') }; },
    };`);

    let grammarResponse;
    let releaseGrammar = false;
    function finishGrammar() {
        if (grammarResponse && releaseGrammar) {
            grammarResponse.end('window.grammarLoaded = true;');
            grammarResponse = null;
        }
    }
    const server = createServer((req, res) => {
        if (req.url === '/api/diff') { res.setHeader('Content-Type', 'application/json'); res.end(JSON.stringify(fixture())); return; }
        if (req.url === '/typescript.js') {
            res.setHeader('Content-Type', 'text/javascript');
            grammarResponse = res;
            finishGrammar();
            return;
        }
        if (req.url === '/release-grammar') {
            releaseGrammar = true;
            res.end();
            finishGrammar();
            return;
        }
        const name = req.url === '/' ? 'index.html' : req.url.slice(1);
        if (!assets.has(name)) { res.writeHead(404); res.end(); return; }
        res.setHeader('Content-Type', name.endsWith('.js') ? 'text/javascript' : name.endsWith('.css') ? 'text/css' : name.endsWith('.svg') ? 'image/svg+xml' : 'text/html');
        res.end(assets.get(name));
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const session = `highlighting-${process.pid}`;
    async function browser(...args) {
        const { stdout } = await exec('agent-browser', ['--session', session, '--json', ...args]);
        return JSON.parse(stdout).data;
    }
    async function evaluate(fn) {
        return (await browser('eval', '-b', Buffer.from(`(${fn})()`).toString('base64'))).result;
    }
    t.after(async () => {
        grammarResponse?.end();
        await browser('close');
        await new Promise(resolve => server.close(resolve));
    });
    await browser('open', `http://127.0.0.1:${server.address().port}/#view=files`);
    await browser('wait', '--fn', 'document.readyState === "complete" && window.grammarLoaded');
    assert.equal(await evaluate(() => window.renderedBeforeGrammar), true, 'exercise a real render before TypeScript registers');
    assert.equal(await evaluate(() => !!hlCache.get(0)), true, 'replace the cached plain-text result without a reload');
    assert.ok(await evaluate(() => document.querySelectorAll('.cd .hljs-keyword').length) > 0, 'render syntax tokens');
    assert.equal(await evaluate(() => {
        const keyword = document.querySelector('.cd .hljs-keyword');
        return getComputedStyle(keyword).color !== getComputedStyle(keyword.closest('.cd')).color;
    }), true, 'syntax tokens have a visible color');
});
