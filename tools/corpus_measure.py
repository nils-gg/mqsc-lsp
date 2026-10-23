#!/usr/bin/env python3
"""Aggregate diagnostics over a corpus of .mqsc files.

Runs a diagnostic producer over every *.mqsc file in the given directory and
prints per-code counts plus the most frequent messages per code. Producers:

  probe   server/target/debug/examples/probe (reads stdin, prints to stderr)
          the default; used before --check exists / for raw probing
  check   mqsc-lsp --check (prints "file:line:col: severity[code]: message"
          to stdout)

Usage:
  python3 tools/corpus_measure.py [dir] [--probe PATH] [--check PATH]
                                  [--dump FILE]

--dump writes one "<file>\t<line>\t<code>\t<message>" record per diagnostic
for later triage. Exit code is always 0; this is a measurement tool, not a
gate (see tools/check_model.py for the model gate).
"""
import argparse
import collections
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run_probe(binary, files):
    diags = []  # (file, line, code, message)
    for f in files:
        with open(f, "rb") as fh:
            p = subprocess.run([binary], stdin=fh,
                               stdout=subprocess.DEVNULL,
                               stderr=subprocess.PIPE, text=True,
                               errors="replace")
        if p.returncode != 0:
            print(f"probe failed on {f}: rc={p.returncode}", file=sys.stderr)
        for ln in p.stderr.splitlines():
            m = re.match(r"  L(\d+) sev\S+ \[(\d+)\] (.*)", ln)
            if m:
                diags.append((os.path.basename(f), int(m.group(1)),
                              int(m.group(2)), m.group(3)))
    return diags


RE_CHECK = re.compile(r"^(.*?):(\d+):(\d+): (\w+)\[(\d+)\]: (.*)$")


def run_check(binary, files):
    diags = []
    p = subprocess.run([binary, "--check"] + files,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       text=True, errors="replace")
    for ln in p.stdout.splitlines():
        m = RE_CHECK.match(ln)
        if m:
            diags.append((os.path.basename(m.group(1)), int(m.group(2)),
                          int(m.group(5)), m.group(6)))
    return diags


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dir", nargs="?",
                    default=os.path.expanduser("~/Code/esql-lsp/dmpmqcfg"))
    ap.add_argument("--probe",
                    default=os.path.join(REPO, "server/target/debug/examples/probe"))
    ap.add_argument("--check", help="use mqsc-lsp --check instead of probe")
    ap.add_argument("--dump", help="write raw records here (TSV)")
    args = ap.parse_args()

    files = sorted(
        os.path.join(args.dir, n) for n in os.listdir(args.dir)
        if n.endswith(".mqsc"))
    if not files:
        sys.exit(f"no .mqsc files under {args.dir}")

    if args.check:
        diags = run_check(args.check, files)
    else:
        diags = run_probe(args.probe, files)

    by_code = collections.Counter(c for _, _, c, _ in diags)
    msg_by_code = collections.defaultdict(collections.Counter)
    for _, _, c, msg in diags:
        msg_by_code[c][re.sub(r"\(near .*\)$", "", msg).strip()] += 1

    print(f"files: {len(files)}  diagnostics: {len(diags)}")
    for code, n in sorted(by_code.items()):
        print(f"  code {code:>2}: {n}")
    for code, _ in sorted(by_code.items()):
        print(f"\n=== code {code} top messages ===")
        for msg, n in msg_by_code[code].most_common(12):
            print(f"  {n:>7}  {msg[:110]}")

    if args.dump:
        os.makedirs(os.path.dirname(os.path.abspath(args.dump)), exist_ok=True)
        with open(args.dump, "w") as fh:
            for f, ln, c, msg in diags:
                fh.write(f"{f}\t{ln}\t{c}\t{msg}\n")
        print(f"\ndumped {len(diags)} records to {args.dump}")


if __name__ == "__main__":
    main()
