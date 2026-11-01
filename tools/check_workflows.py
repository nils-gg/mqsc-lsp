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
4. The release job's asset verification reads the live list with
   `--json assets --jq '.assets[].name'` — guarded by `if !` (no pipe,
   which would swallow gh's exit code: bash steps run `bash -e`
   WITHOUT pipefail) and followed by a zero-asset refusal. `assetNames`
   is not a gh JSON field, and the run that used it published all 8
   assets correctly, then watched the verifier read an empty list and
   flag every one of them as "missing from the release" (v0.2.0
   incident: green release, red self-check).

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
    if path.endswith("release.yml"):
        # Contract 4 (v0.2.0 incident): the asset-list read must be the
        # known-good gh invocation, guarded without a pipe, and refuse an
        # empty list. Text-pinned because CI only exercises this code
        # once per release — far too slow to catch a phantom field.
        # Comments are stripped first: the step's own prose EXPLAINS the
        # bad field name, and only the command lines are load-bearing.
        raw = "\n".join(l for l in lines if not l.strip().startswith("#"))
        if "assetNames" in raw:
            problems.append(f"{path}: gh has no `assetNames` JSON field — "
                            f"read the asset list with `--json assets "
                            f"--jq '.assets[].name'` (v0.2.0 incident: the "
                            f"dead call's exit code was swallowed by a "
                            f"`| sort` pipe and every asset looked missing)")
        if "--json assets --jq '.assets[].name'" not in raw:
            problems.append(f"{path}: the release job must read the live "
                            f"asset list with the exact invocation "
                            f"`--json assets --jq '.assets[].name'`")
        if "if ! gh release view" not in raw:
            problems.append(f"{path}: the gh asset-list read must check gh's "
                            f"exit status with `if !` — a pipeline would "
                            f"mask it (bash steps have no pipefail)")
        if "! -s /tmp/actual" not in raw:
            problems.append(f"{path}: the verifier must refuse a zero-asset "
                            f"response — an empty live list must fail loudly, "
                            f"never as a wrong diff")
    for p in problems:
        print("FAIL:", p)
    return 1 if problems else 0


def main() -> int:
    rc = 0
    for path in FILES:
        rc |= check(path)
    if rc == 0:
        total = sum(len(open(p, encoding="utf-8").readlines()) for p in FILES)
        print(f"PASS: every action is SHA-pinned with a ref comment, every "
              f"toolchain step names its toolchain, no empty expression, and "
              f"the release verifier reads gh's asset list the known-good "
              f"way ({total} lines scanned)")
    return rc


if __name__ == "__main__":
    sys.exit(main())
