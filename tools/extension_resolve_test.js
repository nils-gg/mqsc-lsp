// Behavior test for the VS Code extension's server resolution (PR 10).
//
// Loads extension.js with stubbed `vscode` / `vscode-languageclient`
// modules — no VS Code install and no npm dependencies — and drives
// activate() through the resolution scenarios: nothing found shows
// guidance and starts no client; an explicit missing path shows guidance
// quoting it; an explicit existing path is used verbatim; PATH hits and
// candidate directories are found in auto mode — EVERY candidate the
// resolver probes (all four unix dirs and the win32 pair), not just the
// one that happens to be writable without root (V1). The host couplings
// are explicit preconditions, not silent assumptions (T8). The
// four-place sync contract — candidatePaths() source, setting
// description, extension README, resolver error text — is guarded by
// parsing all four and diffing the candidate lists. V3's loud-failure
// paths (a rejected client.start(), a refused guidance action) are
// asserted to surface as error messages, never as unhandled rejections.
//
// Run: node tools/extension_resolve_test.js
const Module = require('module');
const fs = require('fs');
const path = require('path');
const assert = require('assert');
const os = require('os');
const { spawnSync } = require('child_process');

const REPO = path.join(__dirname, '..');
const EXT = path.join(REPO, 'editors', 'vscode', 'extension',
    'client', 'extension.js');
const PKG = path.join(REPO, 'editors', 'vscode', 'extension', 'package.json');
const README = path.join(REPO, 'editors', 'vscode', 'extension', 'README.md');

const state = {
    serverPath: undefined, // value of mqsc.server.path, if the scenario sets one
    warnings: [],
    errors: [],
    startedWith: [],
    openExternalRejects: false,
    startRejects: false,
};

// Absolute paths that should behave as existing binaries although the
// test must not write into system directories (V1: /usr/local/bin,
// /opt/homebrew/bin, /usr/bin are candidates too). The fs shim below
// answers statSync for exactly these; everything else hits the real fs.
const virtualFiles = new Set();

const realFs = fs;
const fsForExtension = Object.create(realFs);
fsForExtension.statSync = (p, ...rest) => {
    if (typeof p === 'string' && virtualFiles.has(p)) {
        return { isFile: () => true };
    }
    return realFs.statSync(p, ...rest);
};

class FakeLanguageClient {
    constructor(id, name, serverOptions) {
        this.options = serverOptions;
    }
    start() {
        state.startedWith.push(this.options.run.command);
        if (state.startRejects) {
            return Promise.reject(new Error('spawn EACCES'));
        }
        return Promise.resolve();
    }
    stop() {
        return Promise.resolve();
    }
}

const vscodeStub = {
    workspace: {
        getConfiguration: () => ({
            get: (k) => (k === 'server.path' ? state.serverPath : undefined),
        }),
        onDidChangeConfiguration: () => ({ dispose() {} }),
    },
    window: {
        showWarningMessage: (msg) => {
            state.warnings.push(msg);
            // the guidance's first button, so showServerHelp's action
            // branch runs and its failure path is exercised (V3)
            return Promise.resolve(state.openExternalRejects ? 'Open Release Page' : undefined);
        },
        showErrorMessage: (msg) => {
            state.errors.push(msg);
            return Promise.resolve(undefined);
        },
    },
    env: {
        openExternal: () =>
            state.openExternalRejects
                ? Promise.reject(new Error('external browser refused'))
                : Promise.resolve(true),
    },
    commands: { executeCommand: () => Promise.resolve() },
    Uri: { parse: (s) => s },
};

const origLoad = Module._load;
Module._load = function (request, ...rest) {
    if (request === 'vscode') return vscodeStub;
    if (request === 'vscode-languageclient/node')
        return { LanguageClient: FakeLanguageClient };
    if (request === 'fs') return fsForExtension;
    return origLoad.call(this, request, ...rest);
};

function freshActivate(nextState) {
    Object.assign(state, { warnings: [], errors: [], startedWith: [] }, nextState);
    delete require.cache[require.resolve(EXT)];
    const ext = require(EXT);
    return ext.activate({ subscriptions: { push() {} } });
}

// let showServerHelp's .then/.catch chain settle before asserting
const settle = () => new Promise((r) => setImmediate(r));

/** The candidate directory list as the resolver itself declares it. */
function sourceCandidates() {
    const src = fs.readFileSync(EXT, 'utf8');
    const fn = src.match(/function candidatePaths\(\) \{[\s\S]*?\n\}/);
    assert.ok(fn, 'candidatePaths() still exists in extension.js');
    const literals = [...fn[0].matchAll(/'([^']*)'/g)].map((m) => m[1]);
    const absolute = literals.filter((l) => l.startsWith('/'));
    return {
        unixAbsolute: absolute,
        homeLocal: literals.filter((l) => l === '.local').length, // unix AND win32
        win32: literals.includes('Programs') && literals.includes('mqsc-lsp')
            && fn[0].includes('LOCALAPPDATA'),
    };
}

/** The five directories resolveServer probes, in the prose spelling each
 *  of the three documentation sites must carry (V1's sync contract). */
const CANONICAL = [
    '~/.local/bin',
    '/usr/local/bin',
    '/opt/homebrew/bin',
    '/usr/bin',
    '%LOCALAPPDATA%\\Programs\\mqsc-lsp',
];
const norm = (s) => s.replace(/\\\\/g, '\\');

function assertCarriesAll(siteName, text) {
    const t = norm(text);
    for (const c of CANONICAL) {
        assert.ok(t.includes(c), `${siteName} does not document candidate ${c}`);
    }
}

async function main() {
    const emptyHome = fs.mkdtempSync(path.join(os.tmpdir(), 'mqsc-ext-home-'));
    const realPath = process.env.PATH;
    const realHome = process.env.HOME;
    // the fake PATH keeps system utilities (which) reachable but must hold
    // no mqsc-lsp; no-bin is where scenario 4 "installs" one. T8: this
    // isolation is an ASSERTED precondition, not a hope about the host.
    const noBin = path.join(emptyHome, 'no-bin');
    fs.mkdirSync(noBin);
    process.env.PATH = noBin + ':/usr/bin:/bin';
    process.env.HOME = emptyHome; // candidate dirs live under HOME
    const pathProbe = spawnSync('which', ['mqsc-lsp'], { encoding: 'utf8' });
    assert.notStrictEqual(
        pathProbe.status,
        0,
        `host is not mqsc-free: PATH still finds ${pathProbe.stdout.trim()}`
    );
    // ...and no candidate binary pre-exists on the host either — scenario 6
    // virtualizes exactly these paths, a real one would resolve for the
    // wrong reason (T8's isolation contract).
    for (const dir of ['/usr/local/bin', '/opt/homebrew/bin', '/usr/bin']) {
        assert.ok(
            !fs.existsSync(path.join(dir, 'mqsc-lsp')),
            `host already ships ${path.join(dir, 'mqsc-lsp')}; scenarios cannot own it`
        );
    }
    let scenarios = 0;
    try {

    // 1. nothing anywhere -> guidance, no client
    await freshActivate({ serverPath: undefined });
    assert.strictEqual(state.startedWith.length, 0, 'no client without a server');
    assert.strictEqual(state.warnings.length, 1, 'exactly one guidance message');
    assert.ok(/not found/.test(state.warnings[0]), state.warnings[0]);
    scenarios += 1;

    // 2. explicit path that does not exist -> guidance quoting the path
    await freshActivate({ serverPath: '/nowhere/mqsc-lsp' });
    assert.strictEqual(state.startedWith.length, 0);
    assert.ok(/\/nowhere\/mqsc-lsp/.test(state.warnings[0]), state.warnings[0]);
    scenarios += 1;

    // 3. explicit path that exists -> started with exactly it
    const real = path.join(emptyHome, 'mqsc-lsp');
    fs.writeFileSync(real, '#!/bin/sh\n', { mode: 0o755 });
    await freshActivate({ serverPath: real });
    assert.deepStrictEqual(state.startedWith, [real]);
    assert.strictEqual(state.warnings.length, 0);
    scenarios += 1;

    // 4. auto mode finds a PATH hit and resolves it absolutely
    const onPath = path.join(noBin, 'mqsc-lsp');
    fs.writeFileSync(onPath, '#!/bin/sh\n', { mode: 0o755 });
    await freshActivate({ serverPath: 'mqsc-lsp' }); // the default value
    assert.deepStrictEqual(state.startedWith, [onPath]);
    scenarios += 1;

    // 5. auto mode finds a ~/.local/bin candidate when PATH has nothing
    fs.rmSync(onPath);
    fs.mkdirSync(path.join(emptyHome, '.local', 'bin'), { recursive: true });
    const local = path.join(emptyHome, '.local', 'bin', 'mqsc-lsp');
    fs.copyFileSync(real, local);
    await freshActivate({ serverPath: undefined });
    assert.deepStrictEqual(state.startedWith, [local]);
    scenarios += 1;

    // 6. EVERY remaining unix candidate is pinned behaviorally (V1) —
    //    through the statSync shim, so no scenario writes into system
    //    directories and none depends on which one the host permits.
    fs.rmSync(local);
    for (const dir of ['/usr/local/bin', '/opt/homebrew/bin', '/usr/bin']) {
        const cand = path.join(dir, 'mqsc-lsp');
        virtualFiles.clear();
        virtualFiles.add(cand);
        await freshActivate({ serverPath: undefined });
        assert.deepStrictEqual(
            state.startedWith,
            [cand],
            `auto mode must fall through to ${dir}`
        );
        scenarios += 1;
    }

    // 7. candidate ORDER: the earliest probe wins
    virtualFiles.clear();
    virtualFiles.add('/usr/bin/mqsc-lsp');
    virtualFiles.add('/usr/local/bin/mqsc-lsp');
    await freshActivate({ serverPath: undefined });
    assert.deepStrictEqual(state.startedWith, ['/usr/local/bin/mqsc-lsp']);
    virtualFiles.clear();
    scenarios += 1;

    // 8. V3: a client that fails to start surfaces an error message —
    //    activate() itself resolves, never rejects.
    await freshActivate({ serverPath: real, startRejects: true });
    assert.strictEqual(state.errors.length, 1, 'start failure shows exactly one error');
    assert.ok(/failed to start/.test(state.errors[0]), state.errors[0]);
    scenarios += 1;

    // 9. V3: a refused guidance action (openExternal rejects) surfaces too
    await freshActivate({ serverPath: '/nowhere/mqsc-lsp', openExternalRejects: true });
    await settle();
    assert.strictEqual(state.errors.length, 1, 'refused guidance shows exactly one error');
    assert.ok(/refused/.test(state.errors[0]), state.errors[0]);
    scenarios += 1;

    // 10. win32 behavior (T8): the resolver's Windows branch IS reachable
    //     from this host — EXE and candidatePaths() read process.platform
    //     at (re)load/consult time, so flipping it before the fresh
    //     require() drives the win32 candidate list for real.
    const realPlatform = process.platform;
    Object.defineProperty(process, 'platform', { value: 'win32', configurable: true });
    try {
        process.env.LOCALAPPDATA = '/fake-win/Local';
        // nothing installed anywhere -> guidance (also proves the win32
        // PATH probe ('where') misses, which it must on a unix host)
        await freshActivate({ serverPath: undefined });
        assert.strictEqual(state.startedWith.length, 0);
        assert.strictEqual(state.warnings.length, 1);
        scenarios += 1;
        // %LOCALAPPDATA%\Programs\mqsc-lsp\mqsc-lsp.exe is probed (the
        // expected string is the resolver's own path.join expression,
        // posix-joined on this host — the pin is the LOCATION and the
        // .exe name, not the separator).
        const la = path.join(process.env.LOCALAPPDATA, 'Programs', 'mqsc-lsp', 'mqsc-lsp.exe');
        virtualFiles.add(la);
        await freshActivate({ serverPath: undefined });
        assert.deepStrictEqual(state.startedWith, [la],
            'win32 auto mode must probe %LOCALAPPDATA%\\Programs\\mqsc-lsp');
        scenarios += 1;
        // and ~/.local/bin with the Windows exe name as second candidate
        virtualFiles.clear();
        const hb = path.join(emptyHome, '.local', 'bin', 'mqsc-lsp.exe');
        virtualFiles.add(hb);
        await freshActivate({ serverPath: undefined });
        assert.deepStrictEqual(state.startedWith, [hb],
            'win32 auto mode must also probe ~/.local/bin (mqsc-lsp.exe)');
        virtualFiles.clear();
        delete process.env.LOCALAPPDATA;
        scenarios += 1;
    } finally {
        Object.defineProperty(process, 'platform', { value: realPlatform, configurable: true });
    }

    // --- the four-place sync contract (V1) -------------------------------
    const cand = sourceCandidates();
    assert.deepStrictEqual(
        cand.unixAbsolute,
        ['/usr/local/bin', '/opt/homebrew/bin', '/usr/bin'],
        'candidatePaths() unix list changed shape; update this guard AND ' +
            'the three prose sites (AGENTS.md: change it in all four)'
    );
    assert.strictEqual(cand.homeLocal, 2, '~/.local/bin must be probed on unix AND win32');
    assert.ok(cand.win32, 'the win32 %LOCALAPPDATA%\\Programs probe vanished');
    // site 1+2: the setting description and the README
    assertCarriesAll('package.json markdownDescription', fs.readFileSync(PKG, 'utf8'));
    assertCarriesAll('extension README', fs.readFileSync(README, 'utf8'));
    // site 3: the resolver's own error text (inside the parentheses)
    const errList = fs.readFileSync(EXT, 'utf8').match(/install directories \(([\s\S]*?)\)\.'/);
    assert.ok(errList, 'resolver error text still lists its directories');
    assertCarriesAll('resolver error message', errList[1]);

    console.log(
        `PASS: all ${scenarios} resolution scenarios and the four-place ` +
            'candidate sync behave as documented'
    );
    } finally {
        // T9 (REVIEW 2026-10-31): env restore and the sandbox removal must
        // happen on FAIL too — the old tail-only restore leaked a temp dir
        // (and a hijacked env for the exit path) on every failure.
        process.env.PATH = realPath;
        process.env.HOME = realHome;
        virtualFiles.clear();
        fs.rmSync(emptyHome, { recursive: true, force: true });
    }
}

main().catch((e) => {
    console.error('FAIL:', e.message);
    process.exit(1);
});
