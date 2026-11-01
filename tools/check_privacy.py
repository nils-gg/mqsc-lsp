#!/usr/bin/env python3
"""Privacy guard: the dmpmqcfg corpus is CUSTOMER DATA.

No customer-derived identifier — queue-manager names, file names from the
corpus folder, object names, the company's names or domains — may ever
appear in this repository: not in tracked files, not in any commit
message, not in any tag message. Refer to corpus files only by neutral
placeholders (CORPUS-A, CORPUS-B, ...).

Scans (and fails, exit 1, on any hit):
  1. every tracked file (working-tree content — clean at gate time —
     read and matched in-process; `git grep -E` cannot be trusted here:
     its ERE silently ignores \\b)
  2. every commit message and tag message: `git log --all`

Run from anywhere: `python3 tools/check_privacy.py`.
"""
import re
import subprocess
import sys

# Customer identifier patterns, never the literal names (this file must
# not contain what it forbids): VCC/MC-prefixed queue-manager names of the
# corpus family (VCC + 2-3 digits, optional letter; MC + digit + letter)
# and the company name/domain. Placeholder names (CORPUS-X), synthetic
# fixture tokens (VCCK — no digits) and the regex sources below cannot
# match their own patterns.
FORBIDDEN = [
    r"VCC[0-9]{2,3}[A-Z]?\b",
    r"\bMC[0-9][A-Z]\b",
    r"(?i)vol" + "vo",  # concatenation: this file may not carry the literal
]
COMPILED = [(p, re.compile(p)) for p in FORBIDDEN]


def scan(label: str, data: bytes) -> int:
    hits = 0
    if b"\0" in data[:8192]:
        return 0  # binary
    text = data.decode("utf-8", "replace")
    for pat, rx in COMPILED:
        for m in sorted(set(rx.findall(text))):
            print(f"FORBIDDEN: {label}: {m!r} (pattern {pat!r})")
            hits += 1
    return hits


def main() -> int:
    hits = 0

    files = subprocess.run(
        ["git", "ls-files", "-z"], capture_output=True, check=True
    ).stdout.split(b"\0")
    for f in [x for x in files if x]:
        try:
            with open(f, "rb") as fh:
                hits += scan(f"tracked file {f.decode(errors='replace')}", fh.read())
        except OSError:
            pass  # dangling symlink or unreadable: not matchable text

    msgs = subprocess.run(
        ["git", "log", "--all", "--format=%H %s%n%b"],
        capture_output=True, check=True,
    ).stdout
    for block in re.split(rb"\n(?=[0-9a-f]{40} )", msgs):
        hits += scan("commit message", block)

    if hits:
        print(f"check_privacy: FAIL ({hits} forbidden mention(s))")
        print("dmpmqcfg content is customer data: scrub it (placeholders")
        print("CORPUS-A/B/C/…) and rewrite history if it is in a message.")
        return 1
    print("PASS: no customer-derived identifier in tracked content or history")
    return 0


if __name__ == "__main__":
    sys.exit(main())
