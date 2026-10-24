#!/usr/bin/env python3
"""Regeneration guard for model/mqsc-model.json (T-12).

Runs tools/extract_model.py against a temporary file and compares the result
with the committed model. The committed file is NEVER written: the extractor's
OUT_PATH module global is redirected to a temp file before its main() runs.

Exit codes:
  0  regenerated model is byte-identical to the committed one, or the IBM
     reference markdown is absent (regeneration is impossible on a checkout
     without it — this is a skip, not a failure, so CI can call this
     unconditionally).
  1  drift: the committed model is not what the current extractor produces
     from the reference. A summary of changed parameters is printed; use
     --full for the complete list.

Run it after touching tools/extract_model.py (fix expected: rerun
extract_model.py and commit the new model), and before trusting a model edit.
"""

import argparse
import difflib
import importlib.util
import json
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
COMMITTED = REPO / "model" / "mqsc-model.json"


def load_extractor():
    spec = importlib.util.spec_from_file_location(
        "extract_model", REPO / "tools" / "extract_model.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def param_index(model):
    """(verb, object, param) -> param dict, for a structural diff.

    Names are stringified: a nameless param (None) must stay sortable
    against str keys instead of crashing the diff (py3 compares None
    against str).
    """
    out = {}
    for verb, objs in model.get("commands", {}).items():
        for obj, entry in objs.items():
            for p in entry.get("params", []):
                out[(verb, obj, str(p.get("name") or "<unnamed>"))] = p
    return out


def diff_models(old, new):
    po, pn = param_index(old), param_index(new)
    lines = []
    for key in sorted(set(po) | set(pn)):
        a, b = po.get(key), pn.get(key)
        if a is None:
            lines.append(f"  + added    {' '.join(key)}")
        elif b is None:
            lines.append(f"  - removed  {' '.join(key)}")
        elif a != b:
            ka = f"{a.get('kind')}{'=' + ','.join(map(str, a.get('values') or [])) if a.get('values') else ''}"
            kb = f"{b.get('kind')}{'=' + ','.join(map(str, b.get('values') or [])) if b.get('values') else ''}"
            lines.append(f"  ~ changed  {' '.join(key)}: {ka} -> {kb}")
    # docs section drift (text equality of the whole section is enough)
    if old.get("param_docs") != new.get("param_docs"):
        lines.append("  ~ param_docs section differs")
    return lines


def stats(model):
    cmds = model.get("commands", {})
    n = sum(len(e.get("params", [])) for o in cmds.values() for e in o.values())
    objs = sum(len(o) for o in cmds.values())
    return f"{len(cmds)} verbs / {objs} commands, {n} params, {len(model.get('param_docs', {}))} docs"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--full", action="store_true", help="print every changed parameter, not the first 20")
    args = ap.parse_args()

    if not COMMITTED.exists():
        print(f"FAIL: no committed model at {COMMITTED}", file=sys.stderr)
        return 1

    ext = load_extractor()
    if not ext.DOC_PATH.exists():
        print(
            f"SKIP: {ext.DOC_PATH.name} not found at the repo root — the IBM\n"
            "  'Script (MQSC) Command Reference' markdown is copyrighted and not\n"
            "  redistributed (see ATTRIBUTION.md). Regeneration cannot run here;\n"
            "  the committed model is taken as-is. This is expected on clean\n"
            "  checkouts and in CI."
        )
        return 0

    committed_bytes = COMMITTED.read_bytes()
    with tempfile.TemporaryDirectory() as td:
        ext.OUT_PATH = Path(td) / "model.json"  # committed file is never written
        ext.main()
        produced = ext.OUT_PATH.read_bytes()

    if produced == committed_bytes:
        print("PASS: extractor reproduces the committed model byte-for-byte")
        return 0

    print("FAIL: committed model does not match current extractor output", file=sys.stderr)
    old, new = json.loads(committed_bytes), json.loads(produced)
    print(f"  committed: {stats(old)}", file=sys.stderr)
    print(f"  regenerated: {stats(new)}", file=sys.stderr)
    lines = diff_models(old, new)
    shown = lines if args.full else lines[:20]
    print(f"  {len(lines)} difference(s):", file=sys.stderr)
    for ln in shown:
        print(ln, file=sys.stderr)
    if not args.full and len(lines) > len(shown):
        print(f"  ... {len(lines) - len(shown)} more (use --full)", file=sys.stderr)
    # a short unified diff of the raw JSON for anyone who wants to grep it
    ud = list(
        difflib.unified_diff(
            committed_bytes.decode().splitlines(),
            produced.decode().splitlines(),
            "committed",
            "regenerated",
            lineterm="",
            n=0,
        )
    )
    print(f"  ({len(ud)} unified-diff lines; rerun extract_model.py to adopt)", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
