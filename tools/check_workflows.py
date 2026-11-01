#!/usr/bin/env python3
"""Guard the workflow-file contracts (REVIEW 2026-10-31, C5 + its PR #1 lesson).

1. Every `uses:` is pinned to a full commit SHA with a trailing `# <ref>`
   comment — an upstream tag move cannot change what CI executes.
2. Every dtolnay/rust-toolchain step passes an explicit `toolchain:`
   input. The pinned action's own action.yml declares that input
   required; the old `@stable` shorthand carried the toolchain in the
   REF NAME, which a SHA pin erases — PR #1's matrix jobs died with
   "'toolchain' is a required input" exactly there. CI must fail here,
   locally, in seconds, not in a red matrix.
3. No EMPTY `${{ }}` expression anywhere in the raw file — Actions
   evaluates expressions over the whole file content, comments and
   run-block script comments included, so an empty one makes the entire
   workflow invalid and every trigger produces a 0-second, job-less
   failure (the PR #1 release-blocking incident: a prose comment
   illustrating "no ${{ }} interpolation" broke it by existing).

Stdlib-only (the CI runner's python has no guaranteed PyYAML): this is a
line scanner over files the repo controls, not a YAML parser — it reads
`uses:` lines (with or without the `- ` list dash), skips comment lines
inside blocks, and fails loudly on any shape it cannot classify.

Exits non-zero on any violation; prints each finding.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FILES = [
    os.path.join(ROOT, ".github", "workflows", "ci.yml"),
    os.path.join(ROOT, ".github", "workflows", "release.yml"),
]

# Any step-level `uses:` line, with or without the list dash. The SHA and
# the trailing ref comment are captured separately so each can fail its
# own, specific message.
USES = re.compile(
    r"^(?P<indent>\s*)(?:-\s+)?uses:\s*(?P<repo>[^@\s]+)@(?P<ref>[^\s#]+)"
    r"(?:\s+#\s*(?P<comment>\S+))?\s*$")
SHA = re.compile(r"^[0-9a-f]{40}$")
# An expression with nothing in it — invalid wherever it appears, even
# inside a comment (Actions evaluates over the raw file).
EMPTY_EXPR = re.compile(r"\$\{\{\s*\}\}")


def _next_meaningful(lines, i):
    """Next non-blank, non-comment line after index i (comments inside a
    mapping are ours, not content)."""
    while i < len(lines):
        s = lines[i].strip()
        if s and not s.startswith("#"):
            return i, lines[i]
        i += 1
    return i, ""


def check(path: str) -> int:
    problems = []
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    for i, line in enumerate(lines):
        if EMPTY_EXPR.search(line):
            problems.append(
                f"{path}:{i + 1}: empty " + "${{ }} expression"
                " — Actions evaluates expressions over the RAW file "
                "(comments included), so this invalidates the whole "
                "workflow (PR #1 incident)")
        if re.match(r"^\s*-? ?uses:", line) and not USES.match(line):
            problems.append(f"{path}:{i + 1}: unclassifiable uses: line")
            continue
        m = USES.match(line)
        if not m:
            continue
        ref, comment = m.group("ref"), m.group("comment")
        if not SHA.match(ref):
            problems.append(f"{path}:{i + 1}: {m.group('repo')}@{ref} is not "
                            f"pinned to a full commit SHA")
        elif not comment:
            problems.append(f"{path}:{i + 1}: SHA pin lacks the `# <ref>` "
                            f"comment")
        if m.group("repo") == "dtolnay/rust-toolchain":
            # The step must carry  with:\n  toolchain: <name>  (comment
            # lines between them are fine — they are ours).
            j, nxt = _next_meaningful(lines, i + 1)
            if not re.match(r"^\s+with:\s*$", nxt):
                problems.append(f"{path}:{i + 1}: dtolnay/rust-toolchain pin "
                                f"has no `with:` block")
            else:
                k = j + 1
                found = False
                while k < len(lines):
                    s = lines[k].strip()
                    if s.startswith("#") or not s:
                        k += 1
                        continue
                    # de-indent back to (or past) `with:` ends the block
                    if len(lines[k]) - len(lines[k].lstrip()) <= len(nxt) - len(nxt.lstrip()):
                        break
                    if s.startswith("toolchain:"):
                        found = True
                        break
                    k += 1
                if not found:
                    problems.append(
                        f"{path}:{i + 1}: dtolnay/rust-toolchain pin names "
                        f"no explicit toolchain — the @stable ref-name "
                        f"shorthand does not survive a SHA pin (PR #1)")
    for p in problems:
        print("FAIL:", p)
    return 1 if problems else 0


def main() -> int:
    rc = 0
    for path in FILES:
        rc |= check(path)
    if rc == 0:
        total = sum(len(open(p, encoding="utf-8").readlines()) for p in FILES)
        print(f"PASS: every action is SHA-pinned with a ref comment and "
              f"every toolchain step names its toolchain explicitly "
              f"({total} lines scanned)")
    return rc


if __name__ == "__main__":
    sys.exit(main())
