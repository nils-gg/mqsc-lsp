#!/usr/bin/env python3
"""End-to-end test for the workspace index (PR 7 closed files + PR 8 watching).

Runs the server twice against a throwaway workspace directory:
  A. default settings — the startup scan indexes the closed files, every
     read feature (diagnostics, hover, definition, references, completion,
     workspace/symbol) sees their DEFINES, the server registers a
     **/*.mqsc watcher with the client, and simulated didChangeWatchedFiles
     events reindex single files (create/change/delete), while open buffers
     keep overriding the disk. The open-buffer override / close-fallback
     rules and rename's full-workspace coverage are asserted too.
  B. workspaceIndex.enabled=false — the scan is skipped, no watcher is
     registered even though the client accepts dynamic registration, and
     the same references are unresolvable again (the gate really gates).

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
        # registrations the server asked us for (client/registerCapability)
        self.registrations = []
        init = {"processId": None,
                "capabilities": {"workspace": {
                    "didChangeWatchedFiles": {"dynamicRegistration": True}}},
                "rootUri": None,
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
                    msg = json.loads(body)
                    # a server->client REQUEST (has both id and method) must be
                    # answered here or the server waits on us forever
                    if "method" in msg and "id" in msg:
                        self._on_server_request(msg)
                        continue
                    return msg
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

    def _on_server_request(self, m):
        if m["method"] == "client/registerCapability":
            self.registrations.extend(m.get("params", {}).get("registrations", []))
            self._respond(m["id"], None)  # void result per LSP
        else:
            self._respond(m["id"],
                          error={"code": -32601,
                                 "message": "smoke client: unhandled " + m["method"]})

    def _respond(self, rid, result=None, error=None):
        msg = {"jsonrpc": "2.0", "id": rid}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result
        self._send(msg)

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

    def did_change_watched_files(self, events):
        """What a real client sends after its own watcher fires: 1=created,
        2=changed, 3=deleted. The server never sees the filesystem itself."""
        self.notify("workspace/didChangeWatchedFiles", {"changes": [
            {"uri": file_uri(p), "type": t} for p, t in events]})

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

    def log_ordered(self, first, second, timeout=25.0):
        """Both lines appeared, `first` before `second`."""
        end = time.time() + timeout
        while time.time() < end:
            try:
                with open(self.log) as f:
                    data = f.read()
            except OSError:
                data = ""
            if first in data and second in data and data.index(first) < data.index(second):
                return True
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

print("\n== PR 8: the server registers its own file watcher ==")
# settled_diags above answered the server->client registerCapability request
regs = [r for r in A.registrations
        if r.get("method") == "workspace/didChangeWatchedFiles"]
check("client received client/registerCapability", bool(regs), str(A.registrations)[:120])
globs = [w.get("globPattern")
         for r in regs for w in r.get("registerOptions", {}).get("watchers", [])]
check("watch glob is **/*.mqsc", globs == ["**/*.mqsc"], str(globs))
check("registration happened after the scan (a refused watcher never costs the index)",
      A.log_ordered("workspace scan:", "watched files: registered"))

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

print("\n== rename covers the whole workspace now that files are watched ==")
m = A.request("textDocument/rename", {"textDocument": {"uri": OPEN_URI},
                                      "position": {"line": 2, "character": NAME_CH},
                                      "newName": "WS.RENAMED"})
ch = (((m or {}).get("result") or {}).get("changes") or {})
check("rename rewrites the use in the open buffer", OPEN_URI in ch, str(sorted(ch)))
check("rename also rewrites the closed file (PR 8 graduation)", DEFS_URI in ch,
      "watched entries are as fresh as the disk, so the mass edit may rest on them")
check("the open-buffer edit is the TARGET use",
      [e["range"]["start"]["line"] for e in ch.get(OPEN_URI, [])] == [2],
      str(ch.get(OPEN_URI, []))[:100])
check("the closed-file edits hit its DEFINE and its TARGET use",
      sorted(e["range"]["start"]["line"] for e in ch.get(DEFS_URI, [])) == [0, 1],
      str(ch.get(DEFS_URI, []))[:120])

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

print("\n== PR 8: watched events reindex single files ==")
# state now: only open.mqsc is open; WS.NOWHERE, WS.SKIPPED, WS.BUFONLY dangle
late = os.path.join(WS, "late.mqsc")
with open(late, "w") as f:
    f.write("DEFINE QLOCAL(WS.NOWHERE)\n")
A.did_change_watched_files([(late, 1)])  # created
d = A.settled_diags(OPEN_URI)
check("created file resolves hint 12 without reopening anything",
      d is not None and not any("WS.NOWHERE" in d2["message"] for d2 in A.codes(d, 12)),
      str(d)[:100])

with open(late, "w") as f:
    f.write("DEFINE QLOCAL(WS.LATER)\n")
A.did_change_watched_files([(late, 2)])  # changed
d = A.settled_diags(OPEN_URI)
check("changed file: the old name dangles again",
      d is not None and any("WS.NOWHERE" in d2["message"] for d2 in A.codes(d, 12)))
names = {s["name"] for s in ((A.request("workspace/symbol", {"query": "WS."}) or {})
                            .get("result") or [])}
check("changed file: the new name replaced it in the index",
      "WS.LATER" in names and "WS.NOWHERE" not in names,
      str(sorted(n for n in names if n.startswith("WS."))))

os.remove(late)
A.did_change_watched_files([(late, 3)])  # deleted
check("deleted file: settled (republish observed)", A.settled_diags(OPEN_URI) is not None)
names = {s["name"] for s in ((A.request("workspace/symbol", {"query": "WS.LATER"}) or {})
                            .get("result") or [])}
check("deleted file: its DEFINE is gone from the index", not names, str(sorted(names)))

# an event below an excluded directory must not smuggle the file in
os.makedirs(os.path.join(WS, "target"), exist_ok=True)
sneak = os.path.join(WS, "target", "late2.mqsc")
with open(sneak, "w") as f:
    f.write("DEFINE QLOCAL(WS.FROMTARGET)\n")
A.did_change_watched_files([(sneak, 1)])
A.settled_diags(OPEN_URI)  # nothing changes, so this is only a quiet-window barrier
names = {s["name"] for s in ((A.request("workspace/symbol", {"query": "WS.FROMTARGET"}) or {})
                            .get("result") or [])}
check("watched events honour the exclude list", not names, str(sorted(names)))

print("\n== PR 8: a save event for an OPEN buffer does not evict the buffer ==")
A.did_open(DEFS_URI, BUF_TEXT)
d = A.settled_diags(OPEN_URI)
check("the buffer's DEFINE resolves while open",
      d is not None and not any("WS.BUFONLY" in d2["message"] for d2 in A.codes(d, 12)))
# the client now reports the file ON DISK changed (saving the very buffer);
# the entry is Open, so the watcher must not touch it
A.did_change_watched_files([(os.path.join(WS, "defs.mqsc"), 2)])
d2 = A.settled_diags(OPEN_URI)
check("no reindex republish for an open document's own save event", d2 is None,
      str(d2)[:100])
A.did_close(DEFS_URI)
d3 = A.settled_diags(OPEN_URI)
check("after close the disk version is the truth again",
      d3 is not None and any("WS.BUFONLY" in d2["message"] for d2 in A.codes(d3, 12)))
A.shutdown()

print("\n== session B: workspaceIndex.enabled=false ==")
B = Session("B", False)
B.tag = "B"
B.did_open(OPEN_URI, OPEN_TEXT)
diags_b = B.settled_diags(OPEN_URI)
flagged_b = {d["message"] for d in B.codes(diags_b, 12)}
print("   code-12 messages:", sorted(m2[:46] for m2 in flagged_b))
check("scan skipped and logged", B.wait_log("workspace scan skipped", timeout=5))
check("no watcher registration when the index is off (even with the cap on)",
      not B.registrations, str(B.registrations)[:120])
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
