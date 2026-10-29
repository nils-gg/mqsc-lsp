#!/usr/bin/env python3
"""End-to-end test for the PR 7 workspace index (closed *.mqsc files).

Runs the server twice against a throwaway workspace directory:
  A. default settings  — the startup scan indexes the closed files, and
     every read feature (diagnostics, hover, definition, references,
     completion, workspace/symbol) sees their DEFINES; rename and the
     open-buffer override / close-fallback rules behave as documented.
  B. workspaceIndex.enabled=false — the scan is skipped and the same
     references are unresolvable again (the gate really gates).

Like lsp_smoke_test.py: assertive, non-zero exit on FAIL.
"""
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time

BIN = sys.argv[1] if len(sys.argv) > 1 else "server/target/debug/mqsc-lsp"  # run from repo root

fails = []


def check(name, ok, extra=""):
    print(f"  [{'PASS' if ok else 'FAIL'}] {name} {extra}")
    if not ok:
        fails.append(name)


# ------------------------------------------------------------------ fixture
WS = tempfile.mkdtemp(prefix=f"mqsc-ws-smoke-{os.getpid()}-")
# closed file: never opened in the editor, only found by the scan
with open(os.path.join(WS, "defs.mqsc"), "w") as f:
    f.write("DEFINE QLOCAL(WS.SHARED) DESCR('only defined in a closed file')\n"
            "DEFINE QALIAS(WS.ALIAS) TARGET(WS.SHARED)\n")
# recursion proof: a closed file below the root
os.makedirs(os.path.join(WS, "sub"))
with open(os.path.join(WS, "sub", "more.mqsc"), "w") as f:
    f.write("DEFINE QLOCAL(WS.DEEPER)\n")
# exclusion proof: same content, excluded directory
os.makedirs(os.path.join(WS, "target"))
with open(os.path.join(WS, "target", "skip.mqsc"), "w") as f:
    f.write("DEFINE QLOCAL(WS.SKIPPED)\n")

# Hint 12 flags object-REFERENCE parameters (TARGET), which is what the
# closed workspace files can resolve. L6 is the completion probe slot.
OPEN_TEXT = """DEFINE QLOCAL(HERE) DESCR('a local define')
DEFINE QALIAS(A1) TARGET(WS.NOWHERE)
DEFINE QALIAS(A2) TARGET(WS.SHARED)
DEFINE QALIAS(A3) TARGET(WS.SKIPPED)
DEFINE QALIAS(A4) TARGET(WS.DEEPER)
DEFINE QALIAS(A5) TARGET(WS.BUFONLY)
DEFINE QALIAS()
"""
# "DEFINE QALIAS(A2) TARGET(" puts the name of L2 at character 25
NAME_CH = 28
OPEN_URI = "file://" + os.path.join(WS, "open.mqsc")  # not on disk: buffer-only
DEFS_URI = "file://" + os.path.join(WS, "defs.mqsc")  # the closed file, opened later


def file_uri(p):
    return "file://" + p


class Session:
    """One server process over stdio, with a buffered LSP framer."""

    def __init__(self, tag, scan_enabled):
        self.tag = tag
        self.log = os.path.join(tempfile.gettempdir(), f"mqsc-wssmoke-{tag}-{os.getpid()}.log")
        if os.path.exists(self.log):
            os.remove(self.log)
        self.proc = subprocess.Popen(
            [BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env={**os.environ, "MQSC_LSP_LOG": self.log})
        self.buf = b""
        self.fd = self.proc.stdout.fileno()
        os.set_blocking(self.fd, False)
        self.n = 0
        init = {"processId": None, "capabilities": {}, "rootUri": None,
                "workspaceFolders": [{"uri": file_uri(WS), "name": os.path.basename(WS)}]}
        if scan_enabled is not None:
            init["initializationOptions"] = {"mqsc": {"workspaceIndex": {"enabled": scan_enabled}}}
        r = self.request("initialize", init)
        assert r and "result" in r, "initialize failed"
        self.notify("initialized", {})

    # --- plumbing (same framing as lsp_smoke_test.py)
    def _send(self, obj):
        data = json.dumps(obj).encode()
        self.proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
        self.proc.stdin.flush()

    def _pump(self, timeout):
        end = time.time() + timeout
        while True:
            he = self.buf.find(b"\r\n\r\n")
            if he != -1:
                head = self.buf[:he].decode(errors="replace")
                n = 0
                for l in head.split("\r\n"):
                    if l.lower().startswith("content-length"):
                        n = int(l.split(":")[1])
                if len(self.buf) >= he + 4 + n:
                    body = self.buf[he + 4: he + 4 + n]
                    self.buf = self.buf[he + 4 + n:]
                    return json.loads(body)
            if time.time() >= end:
                return None
            select.select([self.fd], [], [], 0.05)
            try:
                chunk = os.read(self.fd, 65536)
            except BlockingIOError:
                continue
            if not chunk:
                return None
            self.buf += chunk

    def request(self, method, params):
        self.n += 1
        rid = self.n
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            m = self._pump(10)
            if m is None:
                return None
            if m.get("id") == rid:
                return m

    def notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def did_open(self, uri, text, version=1):
        self.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "mqsc", "version": version, "text": text}})

    def did_change(self, uri, text, version):
        self.notify("textDocument/didChange", {
            "textDocument": {"uri": uri, "version": version},
            "contentChanges": [{"text": text}]})

    def did_close(self, uri):
        self.notify("textDocument/didClose", {"textDocument": {"uri": uri}})

    def wait_log(self, needle, timeout=25.0):
        """Block until the scan (which runs on a blocking thread) reports."""
        end = time.time() + timeout
        while time.time() < end:
            try:
                with open(self.log) as f:
                    if needle in f.read():
                        return True
            except OSError:
                pass
            time.sleep(0.05)
        return False

    def settled_diags(self, uri, quiet=0.6, timeout=12.0):
        """The LAST publish for `uri` in a quiet window — the scan merges
        asynchronously and republishes, so the first answer is not final."""
        last = None
        end = time.time() + timeout
        while time.time() < end:
            m = self._pump(quiet)
            if m is None:
                return last
            if m.get("method") == "textDocument/publishDiagnostics" \
                    and m["params"]["uri"] == uri:
                last = m["params"]["diagnostics"]

    def codes(self, diags, code):
        return [d for d in (diags or []) if d.get("code") == code]

    def shutdown(self):
        self.request("shutdown", {})
        self.notify("exit", {})
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        err = self.proc.stderr.read().decode(errors="replace")
        check(f"session {self.tag}: no panic on stderr",
              "panicked at" not in err, err[-160:] if err.strip() else "")
        check("server exits cleanly rc=0", self.proc.returncode == 0,
              f"rc={self.proc.returncode}")
        try:
            os.remove(self.log)
        except OSError:
            pass


print("== session A: workspace scan enabled (defaults) ==")
A = Session("A", None)
# opened immediately, i.e. while the scan is still running on the blocking
# pool — exactly the startup race the post-scan republish exists for
A.did_open(OPEN_URI, OPEN_TEXT)
check("scan ran and reported", A.wait_log("workspace scan:"))
diags = A.settled_diags(OPEN_URI)
c12 = A.codes(diags, 12)
flagged = {d["message"] for d in c12}
print("   code-12 messages:", sorted(m[:46] for m in flagged))
check("closed file's DEFINE silences hint 12 (WS.SHARED)",
      not any("WS.SHARED" in m for m in flagged))
check("unknown name still flagged (WS.NOWHERE)",
      any("WS.NOWHERE" in m for m in flagged))
check("excluded dir stays invisible (WS.SKIPPED in target/)",
      any("WS.SKIPPED" in m for m in flagged))
check("subdir file is indexed (WS.DEEPER)",
      not any("WS.DEEPER" in m for m in flagged))

print("\n== workspace/symbol across closed files ==")
m = A.request("workspace/symbol", {"query": "WS."})
syms = (m or {}).get("result") or []
by_name = {s["name"]: s for s in syms}
print("   found:", sorted(by_name))
check("closed DEFINE listed (WS.SHARED)", "WS.SHARED" in by_name)
check("closed DEFINE carries its type",
      by_name.get("WS.SHARED", {}).get("containerName") == "QLOCAL",
      str(by_name.get("WS.SHARED", {}).get("containerName")))
check("deeper dir listed (WS.DEEPER)", "WS.DEEPER" in by_name)
check("excluded file not listed", "WS.SKIPPED" not in by_name)
if "WS.SHARED" in by_name:
    check("symbol points into the closed file",
          by_name["WS.SHARED"]["location"]["uri"] == DEFS_URI,
          by_name["WS.SHARED"]["location"]["uri"])

print("\n== navigation into a closed file ==")
# L0 COL20: the name inside ALTER QLOCAL(WS.SHARED)
m = A.request("textDocument/definition", {"textDocument": {"uri": OPEN_URI},
                                          "position": {"line": 2, "character": NAME_CH}})
res = (m or {}).get("result")
if isinstance(res, list):
    res = res[0] if res else {}
one = res or {}
check("definition jumps to the closed file's DEFINE",
      bool(one) and one.get("uri") == DEFS_URI
      and one["range"]["start"]["line"] == 0, str(one)[:120])

m = A.request("textDocument/references", {"textDocument": {"uri": OPEN_URI},
                                          "position": {"line": 2, "character": NAME_CH},
                                          "context": {"includeDeclaration": True}})
refs = (m or {}).get("result") or []
ref_uris = {r["uri"] for r in refs}
print("   reference uris:", sorted(os.path.basename(u) for u in ref_uris))
check("references include the closed file (defs.mqsc)", DEFS_URI in ref_uris)
check("references include the open buffer", OPEN_URI in ref_uris)

m = A.request("textDocument/hover", {"textDocument": {"uri": OPEN_URI},
                                     "position": {"line": 2, "character": NAME_CH}})
hv = (((m or {}).get("result") or {}).get("contents") or {})
htext = hv.get("value") if isinstance(hv, dict) else str(hv)
check("hover shows the closed file as the defining file",
      "defs.mqsc" in (htext or ""), (htext or "")[:120].replace("\n", " | "))

print("\n== completion offers closed-file names ==")
# L6: "DEFINE QALIAS()" — cursor inside the parens
m = A.request("textDocument/completion", {"textDocument": {"uri": OPEN_URI},
                                          "position": {"line": 6, "character": 14},
                                          "context": {"triggerKind": 2}})
items = (m or {}).get("result") or []
items = items.get("items") if isinstance(items, dict) else items
labels = {i["label"] for i in (items or [])}
print("   names:", sorted(l for l in labels if l.startswith("WS.")))
check("closed-file queue offered in completion", "WS.SHARED" in labels)
check("excluded file's queue not offered", "WS.SKIPPED" not in labels)

print("\n== rename still edits open documents only (until PR 8) ==")
m = A.request("textDocument/rename", {"textDocument": {"uri": OPEN_URI},
                                      "position": {"line": 2, "character": NAME_CH},
                                      "newName": "WS.RENAMED"})
ch = (((m or {}).get("result") or {}).get("changes") or {})
check("rename rewrites the use in the open buffer", OPEN_URI in ch, str(sorted(ch)))
check("rename does NOT edit the closed file's DEFINE", DEFS_URI not in ch,
      "a closed entry is only as fresh as the last scan")
check("the open-buffer edit is the TARGET use",
      [e["range"]["start"]["line"] for e in ch.get(OPEN_URI, [])] == [2],
      str(ch.get(OPEN_URI, []))[:100])

print("\n== open buffer overrides disk; close falls back to disk ==")
# open defs.mqsc with UNSAVED extra content: the buffer is the truth
BUF_TEXT = open(os.path.join(WS, "defs.mqsc")).read() + "DEFINE QLOCAL(WS.BUFONLY)\n"
check("WS.BUFONLY unknown while only in the buffer",
      any("WS.BUFONLY" in m2 for m2 in flagged))
A.did_open(DEFS_URI, BUF_TEXT)
d_open = A.settled_diags(OPEN_URI)
check("the buffer's new DEFINE resolves the sibling immediately",
      not any("WS.BUFONLY" in d["message"] for d in A.codes(d_open, 12)))
# while open, the on-disk version must not double-count as a closed entry
m = A.request("workspace/symbol", {"query": "WS.BUFONLY"})
n_buf = len([s for s in ((m or {}).get("result") or []) if s["name"] == "WS.BUFONLY"])
check("no duplicate entry while the file is open", n_buf == 1, f"{n_buf} symbols")
A.did_close(DEFS_URI)
d_closed = A.settled_diags(OPEN_URI)
c12_closed = A.codes(d_closed, 12)
check("after didClose the unsaved DEFINE stops resolving (disk wins)",
      any("WS.BUFONLY" in d["message"] for d in c12_closed))
check("after didClose the on-disk DEFINE still resolves (fallback entry)",
      not any("WS.SHARED" in d["message"] for d in c12_closed))
A.shutdown()

print("\n== session B: workspaceIndex.enabled=false ==")
B = Session("B", False)
B.tag = "B"
B.did_open(OPEN_URI, OPEN_TEXT)
diags_b = B.settled_diags(OPEN_URI)
flagged_b = {d["message"] for d in B.codes(diags_b, 12)}
print("   code-12 messages:", sorted(m2[:46] for m2 in flagged_b))
check("scan skipped and logged", B.wait_log("workspace scan skipped", timeout=5))
check("without the scan the closed DEFINE is invisible again",
      any("WS.SHARED" in m2 for m2 in flagged_b))
m = B.request("workspace/symbol", {"query": "WS."})
names_b = {s["name"] for s in ((m or {}).get("result") or [])}
check("workspace/symbol sees only the open buffer", not names_b, str(sorted(names_b)))
B.shutdown()

shutil.rmtree(WS, ignore_errors=True)

print("\n== summary ==")
if fails:
    print("  FAILED:", ", ".join(fails))
sys.exit(1 if fails else 0)
