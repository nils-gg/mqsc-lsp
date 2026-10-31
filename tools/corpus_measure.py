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
for later triage (tabs/newlines in messages are flattened to keep the TSV
column contract). Exit code is 0 after a successful measurement; missing
corpus dirs, empty corpora, missing producer binaries AND producer
failures (a probe that died on some file, a --check that came back with
a code outside its 0/1 contract) exit non-zero — measuring the survivors
and exiting 0 would hide exactly the crashes this tool exists to find
(REVIEW T2). This is a measurement tool, not a gate (see
tools/check_model.py for the model gate).
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
    failures = 0
    for f in files:
        with open(f, "rb") as fh:
            p = subprocess.run([binary], stdin=fh,
                               stdout=subprocess.DEVNULL,
                               stderr=subprocess.PIPE, text=True,
                               errors="replace")
        if p.returncode != 0:
            failures += 1
            print(f"probe failed on {f}: rc={p.returncode}", file=sys.stderr)
        for ln in p.stderr.splitlines():
            m = re.match(r"  L(\d+) sev\S+ \[(\d+)\] (.*)", ln)
            if m:
                diags.append((os.path.basename(f), int(m.group(1)),
                              int(m.group(2)), m.group(3)))
    return diags, failures


RE_CHECK = re.compile(r"^(.*?):(\d+):(\d+): (\w+)\[(\d+)\]: (.*)$")


def run_check(binary, files):
    diags = []
    p = subprocess.run([binary, "--check"] + files,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       text=True, errors="replace")
    # 0 = clean, 1 = error findings: normal. 2 = usage/IO problem, 101 = a
    # panic in the analyzer: say so loudly AND count it as a failure (see
    # main) — measuring the truncated stdout as if the run had succeeded
    # is how a crashing analyzer goes unnoticed (REVIEW T2).
    failures = 0
    if p.returncode not in (0, 1):
        failures += 1
        print(f"check failed: rc={p.returncode}\n{p.stderr[:2000]}",
              file=sys.stderr)
    for ln in p.stdout.splitlines():
        m = RE_CHECK.match(ln)
        if m:
            diags.append((os.path.basename(m.group(1)), int(m.group(2)),
                          int(m.group(5)), m.group(6)))
    return diags, failures


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
        diags, failures = run_check(args.check, files)
    else:
        diags, failures = run_probe(args.probe, files)

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
                msg = msg.replace("\t", " ").replace("\n", " ")
                fh.write(f"{f}\t{ln}\t{c}\t{msg}\n")
        print(f"\ndumped {len(diags)} records to {args.dump}")

    # the measurement above stays printable even when some producer died —
    # but the exit code must tell that apart from a clean run (REVIEW T2)
    if failures:
        print(f"\n{failures} producer failure(s): the counts above are TRUNCATED",
              file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
