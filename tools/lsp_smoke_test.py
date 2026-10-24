#!/usr/bin/env python3
"""Reliable end-to-end test client for the MQSC language server (os.read-based framer)."""
import subprocess, json, select, sys, time, os, tempfile

BIN = sys.argv[1] if len(sys.argv) > 1 else "server/target/debug/mqsc-lsp"  # run from repo root
# T-14: run the server with file logging enabled (MQSC_LSP_LOG path); the
# log is verified at the end. The default (no flag, no env) stays silent —
# this proves the env-var activation and the content in one pass.
LOGPATH = os.path.join(tempfile.gettempdir(), f"mqsc-smoke-{os.getpid()}.log")
if os.path.exists(LOGPATH):
    os.remove(LOGPATH)
proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        env={**os.environ, "MQSC_LSP_LOG": LOGPATH})
buf = b""
fd = proc.stdout.fileno()
os.set_blocking(fd, False)


def send(obj):
    data = json.dumps(obj).encode()
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    proc.stdin.flush()


def pump(timeout):
    global buf
    end = time.time() + timeout
    while True:
        header_end = buf.find(b"\r\n\r\n")
        if header_end != -1:
            head = buf[:header_end].decode(errors="replace")
            n = 0
            for l in head.split("\r\n"):
                if l.lower().startswith("content-length"):
                    n = int(l.split(":")[1])
            if len(buf) >= header_end + 4 + n:
                body = buf[header_end + 4: header_end + 4 + n]
                buf = buf[header_end + 4 + n:]
                return json.loads(body)
        if time.time() >= end:
            return None
        select.select([fd], [], [], 0.05)
        try:
            chunk = os.read(fd, 65536)
        except BlockingIOError:
            continue
        if not chunk:
            return None
        buf += chunk


def request(id, method, params):
    send({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    while True:
        m = pump(8)
        if m is None:
            return None
        if m.get("id") == id:
            return m


DOC = """DEFINE QLOCAL(DEAD.QUEUE) REPLACE
DEFINE CHANNEL(SYSTEM.DEF.SDR) CHLTYPE(SDR) TRPTYPE(TCP) CONNAME('9.20.4.20(1412)') SEQWRAP(9999999)
DISPLAY QLOCAL(*) WHERE(CURRENT_QS)
ALTER QLOCAL(NEW1) MAXMSGS
DEF QL(X)
DEFINE QLOCAL(Q1) DESCR('It works')
DEFINE QALIAS(A2) TARGET(Q1) REPLACE
DEFINE QALIAS(A3) TARGET(NO.SUCH.Q) REPLACE
"""
uri = "file:///tmp/sample.mqsc"

fails = []


def check(name, ok, extra=""):
    print(f"  [{'PASS' if ok else 'FAIL'}] {name} {extra}")
    if not ok:
        fails.append(name)

r = request(1, "initialize", {"processId": None, "capabilities": {}, "rootUri": None})
caps = r["result"]["capabilities"]
print("== initialize: hover", bool(caps.get("hoverProvider")),
      "| completion", bool(caps.get("completionProvider")),
      "| symbols", caps.get("documentSymbolProvider"),
      "| folding", bool(caps.get("foldingRangeProvider")),
      "| semtokens", bool(caps.get("semanticTokensProvider")),
      "| definition", bool(caps.get("definitionProvider")),
      "| references", bool(caps.get("referencesProvider")),
      "| rename", bool(caps.get("renameProvider")),
      "| workspace/symbol", bool(caps.get("workspaceSymbolProvider")))
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
    "textDocument": {"uri": uri, "languageId": "mqsc", "version": 1, "text": DOC}}})

diags = []
while True:
    m = pump(2)
    if m is None:
        break
    if m.get("method") == "textDocument/publishDiagnostics":
        diags = m["params"]["diagnostics"]
print("== diagnostics ==")
for d in diags:
    print(f"  L{d['range']['start']['line']}:{d['range']['start']['character']}-{d['range']['end']['character']}"
          f" sev{d['severity']} [{d.get('code')}] {d['message']}")

print("\n== hover DEFINE ==")
m = request(2, "textDocument/hover", {"textDocument": {"uri": uri}, "position": {"line": 0, "character": 2}})
print(((m.get("result") or {}).get("contents") or {}).get("value", "")[:160] if m else "TIMEOUT")

print("\n== hover QLOCAL ==")
m = request(3, "textDocument/hover", {"textDocument": {"uri": uri}, "position": {"line": 0, "character": 10}})
print(((m.get("result") or {}).get("contents") or {}).get("value", "")[:280] if m else "TIMEOUT")

print("\n== hover REPLACE (param) ==")
m = request(4, "textDocument/hover", {"textDocument": {"uri": uri}, "position": {"line": 0, "character": 28}})
print(((m.get("result") or {}).get("contents") or {}).get("value", "")[:400] if m else "TIMEOUT")

print("\n== completion verb position (line 4 'DEF QL(X)' col 0) ==")
m = request(5, "textDocument/completion", {"textDocument": {"uri": uri}, "position": {"line": 4, "character": 0},
                                          "context": {"triggerKind": 1}})
if m and m.get("result"):
    items = (m["result"]["items"] if isinstance(m["result"], dict) else m["result"])
    print(f"  {len(items)} items:", ", ".join(i["label"] for i in items[:12]))

print("\n== completion after DEFINE QLOCAL( ==")
m = request(6, "textDocument/completion", {"textDocument": {"uri": uri}, "position": {"line": 5, "character": 18},
                                          "context": {"triggerKind": 2}})
if m and m.get("result"):
    items = (m["result"]["items"] if isinstance(m["result"], dict) else m["result"])
    print(f"  {len(items)} items:", ", ".join(i["label"] for i in items[:12]))

print("\n== completion params for ALTER QLOCAL ==")
m = request(7, "textDocument/completion", {"textDocument": {"uri": uri}, "position": {"line": 3, "character": 19},
                                          "context": {"triggerKind": 1}})
if m and m.get("result"):
    items = (m["result"]["items"] if isinstance(m["result"], dict) else m["result"])
    print(f"  {len(items)} items:", ", ".join(i["label"] for i in items[:15]))
    for i in items[:5]:
        print(f"   {i['label']:18} kind={i.get('kind')} detail={(i.get('detail') or '')[:70]!r}")

print("\n== documentSymbol ==")
m = request(8, "textDocument/documentSymbol", {"textDocument": {"uri": uri}})
if m:
    for sy in m["result"]:
        print(f"   {sy['name']!r} kind={sy['kind']} L{sy['range']['start']['line']}")
    names = [sy["name"] for sy in m["result"]]
    check("documentSymbol labels carry object type + name",
          any(n == "DEFINE QLOCAL (DEAD.QUEUE)" for n in names), str(names[:2]))

print("\n== foldingRange ==")
m = request(9, "textDocument/foldingRange", {"textDocument": {"uri": uri}})
if m:
    print("  ", [(f['startLine'], f['endLine'], f.get('kind')) for f in m["result"]])

print("\n== semanticTokens/full ==")
m = request(10, "textDocument/semanticTokens/full", {"textDocument": {"uri": uri}})
if m:
    d = m["result"]["data"]
    print("   total:", len(d), "first quintets:", [d[i:i+5] for i in range(0, min(20, len(d)), 5)])

print("\n== completion inside TARGET( on line 6: script-defined names ==")
m = request(11, "textDocument/completion", {"textDocument": {"uri": uri}, "position": {"line": 6, "character": 25},
                                           "context": {"triggerKind": 2}})
labels = []
if m and m.get("result"):
    items = (m["result"]["items"] if isinstance(m["result"], dict) else m["result"])
    labels = [i["label"] for i in items]
    print("  ", labels)
check("completion offers Q1 DEFINEd earlier in script", "Q1" in labels)

print("\n== codeAction quickfix on the MAXMSGS diagnostic ==")
d4 = next((d for d in diags if d.get("code") == 4 and "MAXMSGS" in d["message"]), None)
if d4 is None:
    check("code-4 MAXMSGS diagnostic published", False)
else:
    m = request(12, "textDocument/codeAction", {"textDocument": {"uri": uri}, "range": d4["range"],
                                                "context": {"diagnostics": [d4]}})
    acts = (m or {}).get("result") or []
    titles = [a.get("title") for a in acts]
    print("  ", titles)
    check("quickfix 'Change to `MAXMSGL`' offered", any("MAXMSGL" in (t or "") for t in titles))

print("\n== definition: TARGET(Q1) on line 6 -> DEFINE QLOCAL(Q1) on line 5 ==")
m = request(13, "textDocument/definition", {"textDocument": {"uri": uri},
                                           "position": {"line": 6, "character": 25}})
res = (m or {}).get("result")
locs = res if isinstance(res, list) else ([res] if res else [])
starts = [(l["range"]["start"]["line"], l["range"]["start"]["character"]) for l in locs]
check("definition resolves Q1 to its DEFINE at (5,14)", starts == [(5, 14)], str(starts))

print("\n== references on Q1 (line 5) ==")
m = request(14, "textDocument/references", {"textDocument": {"uri": uri},
                                           "position": {"line": 5, "character": 14},
                                           "context": {"includeDeclaration": True}})
locs = (m or {}).get("result") or []
rs = sorted((l["range"]["start"]["line"], l["range"]["start"]["character"]) for l in locs)
print("  ", rs)
check("references finds Q1 at (5,14) definition and (6,25) use", rs == [(5, 14), (6, 25)], str(rs))

print("\n== rename Q1 -> QNEW from the TARGET use ==")
m = request(15, "textDocument/rename", {"textDocument": {"uri": uri},
                                        "position": {"line": 6, "character": 25},
                                        "newName": "QNEW"})
ch = (((m or {}).get("result") or {}).get("changes") or {}).get(uri, [])
ed = sorted((e["range"]["start"]["line"], e["range"]["start"]["character"], e["newText"]) for e in ch)
print("  ", ed)
check("rename rewrites both Q1 sites", ed == [(5, 14, "QNEW"), (6, 25, "QNEW")], str(ed))

print("\n== hover on object names (T-18) ==")
m = request(16, "textDocument/hover", {"textDocument": {"uri": uri},
                                       "position": {"line": 5, "character": 14}})
v = (((m or {}).get("result") or {}).get("contents") or {}).get("value", "")
print("   def hover:", v.replace("\n", " ")[:110])
check("hover on the DEFINE name marks the definition", "this is the definition" in v, v[:60])
m = request(17, "textDocument/hover", {"textDocument": {"uri": uri},
                                       "position": {"line": 6, "character": 25}})
v = (((m or {}).get("result") or {}).get("contents") or {}).get("value", "")
check("hover on the use names the defining line (1-based 6)", "DEFINEd at line 6" in v, v[:80])
m = request(18, "textDocument/hover", {"textDocument": {"uri": uri},
                                       "position": {"line": 7, "character": 25}})
v = (((m or {}).get("result") or {}).get("contents") or {}).get("value", "")
check("hover on NO.SUCH.Q says it is not DEFINEd", "not DEFINEd" in v, v[:80])

print("\n== workspace/symbol ==")
m = request(19, "workspace/symbol", {"query": "Q1"})
syms = (m or {}).get("result") or []
print("  ", [(s["name"], s.get("containerName")) for s in syms])
check("workspace/symbol finds Q1 grouped under QLOCAL at line 5",
      any(s["name"] == "Q1" and s.get("containerName") == "QLOCAL"
          and s["location"]["range"]["start"]["line"] == 5 for s in syms))
m = request(20, "workspace/symbol", {"query": ""})
syms = (m or {}).get("result") or []
got = sorted(s["name"] for s in syms)
print("   all symbols:", got)
check("workspace/symbol lists all DEFINEd objects (X counts via DEF/QL synonyms)",
      got == sorted(["DEAD.QUEUE", "SYSTEM.DEF.SDR", "Q1", "A2", "A3", "X"]), str(got))

print("\n== codeAction is_preferred is per diagnostic (T-20) ==")
d_a = {"range": {"start": {"line": 5, "character": 18}, "end": {"line": 5, "character": 23}},
       "severity": 2, "code": 5, "source": "mqsc", "message": "value must be one of YES, NO",
       "data": {"quickfixes": ["YES", "NO"]}}
d_b = {"range": {"start": {"line": 3, "character": 18}, "end": {"line": 3, "character": 25}},
       "severity": 1, "code": 4, "source": "mqsc", "message": "'MAXMSGS' is not a parameter",
       "data": {"quickfixes": ["MAXMSGL"]}}
m = request(21, "textDocument/codeAction", {"textDocument": {"uri": uri}, "range": d_a["range"],
                                           "context": {"diagnostics": [d_a, d_b]}})
acts = ((m or {}).get("result") or [])
pairs = [(a.get("title"), a.get("isPreferred")) for a in acts]
print("  ", pairs)
prefs = {t for t, p in pairs if p}
check("each diagnostic's first suggestion is preferred",
      prefs == {"Change to `YES`", "Change to `MAXMSGL`"}, str(pairs))

print("\n== cross-file (T-17): open sibling that DEFINEs NO.SUCH.Q ==")
DOC2 = "DEFINE QLOCAL(NO.SUCH.Q) DESCR('defined in sibling')\nDEFINE QALIAS(A9) TARGET(Q1)\n"
uri2 = "file:///tmp/other.mqsc"
send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {
    "textDocument": {"uri": uri2, "languageId": "mqsc", "version": 1, "text": DOC2}}})
d_other = d_sample = None
while True:
    m = pump(2)
    if m is None:
        break
    if m.get("method") == "textDocument/publishDiagnostics":
        if m["params"]["uri"] == uri2:
            d_other = m["params"]["diagnostics"]
        elif m["params"]["uri"] == uri:
            d_sample = m["params"]["diagnostics"]
check("sibling's defines silence the code-12 hint in the first doc (republished)",
      d_sample is not None and not any(d.get("code") == 12 for d in d_sample),
      str([(d.get("code"), d["message"][:40]) for d in (d_sample or [])]))
check("the definition itself gets no undefined-object noise",
      d_other is not None and not any(d.get("code") == 12 for d in d_other))

m = request(22, "textDocument/definition", {"textDocument": {"uri": uri},
                                           "position": {"line": 7, "character": 25}})
res = (m or {}).get("result")
locs = res if isinstance(res, list) else ([res] if res else [])
got = [(l["uri"], l["range"]["start"]["line"], l["range"]["start"]["character"]) for l in locs]
check("definition jumps from DOC's TARGET(NO.SUCH.Q) into the sibling",
      got == [(uri2, 0, 14)], str(got))

m = request(23, "textDocument/references", {"textDocument": {"uri": uri},
                                           "position": {"line": 7, "character": 25},
                                           "context": {"includeDeclaration": True}})
locs = (m or {}).get("result") or []
rs = sorted((l["uri"], l["range"]["start"]["line"], l["range"]["start"]["character"]) for l in locs)
check("references span both documents (sorted by uri)",
      rs == [(uri2, 0, 14), (uri, 7, 25)], str(rs))

m = request(24, "textDocument/hover", {"textDocument": {"uri": uri},
                                       "position": {"line": 7, "character": 25}})
v = (((m or {}).get("result") or {}).get("contents") or {}).get("value", "")
print("   cross hover:", v.replace("\n", " ")[:120])
check("hover names the sibling file and line",
      "another open document" in v and "other.mqsc" in v and "line 1" in v, v[:80])

m = request(25, "textDocument/completion", {"textDocument": {"uri": uri},
                                           "position": {"line": 7, "character": 25},
                                           "context": {"triggerKind": 2}})
labels = []
if m and m.get("result"):
    items = (m["result"]["items"] if isinstance(m["result"], dict) else m["result"])
    labels = [i["label"] for i in items]
check("completion offers the sibling's QLOCAL name", "NO.SUCH.Q" in labels, str(labels[:12]))

m = request(26, "textDocument/rename", {"textDocument": {"uri": uri},
                                        "position": {"line": 7, "character": 25},
                                        "newName": "RENAMED.Q"})
ch = (((m or {}).get("result") or {}).get("changes") or {})
edits = {u: sorted((e["range"]["start"]["line"], e["range"]["start"]["character"], e["newText"])
                   for e in el) for u, el in ch.items()}
print("  ", edits)
check("rename edits both documents",
      edits.get(uri) == [(7, 25, "RENAMED.Q")] and edits.get(uri2) == [(0, 14, "RENAMED.Q")],
      str(edits))

print("\n== cross-file teardown: close the sibling ==")
send({"jsonrpc": "2.0", "method": "textDocument/didClose", "params": {
    "textDocument": {"uri": uri2}}})
d_sample = None
while True:
    m = pump(2)
    if m is None:
        break
    if m.get("method") == "textDocument/publishDiagnostics" and m["params"]["uri"] == uri:
        d_sample = m["params"]["diagnostics"]
check("closing the sibling republishes the hint in the first doc",
      d_sample is not None and any(d.get("code") == 12 and "NO.SUCH.Q" in d["message"] for d in d_sample),
      str([(d.get("code"), d["message"][:40]) for d in (d_sample or []) if d.get("code") == 12]))
m = request(27, "textDocument/references", {"textDocument": {"uri": uri},
                                           "position": {"line": 7, "character": 25},
                                           "context": {"includeDeclaration": True}})
locs = (m or {}).get("result") or []
check("references shrink back to one document",
      [(l["uri"], l["range"]["start"]["line"]) for l in locs] == [(uri, 7)], str(locs))

print("\n== settings: UNDEFINED_OBJECT off ==")
before = sum(1 for d in diags if d.get("code") == 12)
print("   code-12 hints before:", before)
send({"jsonrpc": "2.0", "method": "workspace/didChangeConfiguration",
      "params": {"settings": {"mqsc": {"diagnostics": {"UNDEFINED_OBJECT": "off"}}}}})
send({"jsonrpc": "2.0", "method": "textDocument/didChange", "params": {
    "textDocument": {"uri": uri, "version": 2}, "contentChanges": [{"text": DOC}]}})
diags2 = []
while True:
    m = pump(3)
    if m is None:
        break
    if m.get("method") == "textDocument/publishDiagnostics":
        diags2 = m["params"]["diagnostics"]
after = sum(1 for d in diags2 if d.get("code") == 12)
print("   code-12 hints after:", after, "| total diags:", len(diags2))
check("hint 12 present before config", before > 0)
check("hint 12 gone after UNDEFINED_OBJECT=off", after == 0)

print("\n== log file (T-14, via MQSC_LSP_LOG) ==")
try:
    with open(LOGPATH) as f:
        logtext = f.read()
except OSError as e:
    logtext = ""
    check("log file created", False, str(e))
for needle, label in [
    ("logging to", "startup line names the log file"),
    ("client initialized", "initialized line"),
    ("didOpen file:///tmp/sample.mqsc", "didOpen with uri"),
    ("parse=", "parse duration"),
    ("analyze file:///tmp/sample.mqsc", "analyze line with uri"),
    ("diags=", "diagnostic count"),
    ("req textDocument/hover -> ok", "per-request timing line"),
]:
    check(f"log contains {label}", needle in logtext, "" if needle in logtext else logtext[:120])
if logtext:
    print("   sample lines:")
    for l in logtext.splitlines()[:4]:
        print("   ", l[:110])

print("\n== summary ==")
if fails:
    print("  FAILED:", ", ".join(fails))

request(99, "shutdown", {})
send({"jsonrpc": "2.0", "method": "exit", "params": {}})
proc.stdin.close()  # tower-lsp terminates its stdio loop on EOF after exit
try:
    proc.wait(timeout=5)
except subprocess.TimeoutExpired:
    proc.kill()
err = proc.stderr.read().decode(errors="replace")
if err.strip():
    print("stderr:", err[-500:])
    # panics are echoed to stderr by design (telemetry.rs); anything with
    # that marker during a smoke session is a failure rc can miss.
    check("no panic echoed to stderr", "panicked at" not in err, err[-200:])
# A server that exits non-zero (e.g. 101 despite the exit handshake) must
# fail this gate, not just print the number (REVIEW.md minor).
check("server exits cleanly", proc.returncode == 0, f"rc={proc.returncode}")
try:
    os.remove(LOGPATH)  # verified above; don't litter the temp dir
except OSError:
    pass
sys.exit(1 if fails else 0)
