#!/usr/bin/env python3
"""Reliable end-to-end test client for the MQSC language server (os.read-based framer)."""
import subprocess, json, select, sys, time, os

BIN = sys.argv[1] if len(sys.argv) > 1 else "server/target/debug/mqsc-lsp"  # run from repo root
proc = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
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
"""
uri = "file:///tmp/sample.mqsc"

r = request(1, "initialize", {"processId": None, "capabilities": {}, "rootUri": None})
caps = r["result"]["capabilities"]
print("== initialize: hover", bool(caps.get("hoverProvider")),
      "| completion", bool(caps.get("completionProvider")),
      "| symbols", caps.get("documentSymbolProvider"),
      "| folding", bool(caps.get("foldingRangeProvider")),
      "| semtokens", bool(caps.get("semanticTokensProvider")))
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

print("\n== foldingRange ==")
m = request(9, "textDocument/foldingRange", {"textDocument": {"uri": uri}})
if m:
    print("  ", [(f['startLine'], f['endLine'], f.get('kind')) for f in m["result"]])

print("\n== semanticTokens/full ==")
m = request(10, "textDocument/semanticTokens/full", {"textDocument": {"uri": uri}})
if m:
    d = m["result"]["data"]
    print("   total:", len(d), "first quintets:", [d[i:i+5] for i in range(0, min(20, len(d)), 5)])

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
print("exit code:", proc.returncode)
