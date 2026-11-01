#!/usr/bin/env python3
"""Publish a compact failure digest of a captured log as ONE ::error::
annotation, so anonymous visitors can read *which* tests failed.

GitHub hides job logs and step summaries from anyone without repo rights;
annotations (the "Annotations" box on the check-run page) are public even
on public repos. Platform jobs therefore tee their output to a file and,
on failure, run this tool over it (the full tail still goes to the step
summary for logged-in readers).

Usage: ci_publish_failure.py <logfile> <label>
Never fails: a broken digest must not mask the job's real failure. A
missing logfile is its own case (C3, REVIEW 2026-10-31): the job died
before the tee step produced anything, and the annotation must still
name the situation instead of an open() traceback.
"""
import re
import sys
from pathlib import Path


ANSI = re.compile(r"\x1b\[[0-9;]*m")


def digest(text: str) -> str:
    # T11 (REVIEW 2026-10-31): scrub ANSI and CRLF BEFORE anchoring —
    # cargo colorizes the `failures:` heading itself, so the old raw-text
    # find("\nfailures:") missed exactly the anchor it was written for
    # (and silently degraded to a whole-log tail).
    text = ANSI.sub("", text).replace("\r\n", "\n").replace("\r", "\n")
    lines = text.splitlines()
    # the Python smoke harnesses report through "[FAIL]" markers plus a
    # trailing summary — live for the smoke logs published by release.yml,
    # inert for cargo logs. cargo test's story starts at its
    # "failures:" heading. Anything else: whole log.
    fails = [l for l in lines if "[FAIL]" in l]
    if fails:
        return "\n".join(fails[-20:] + ["--- log tail ---"] + lines[-30:])
    m = re.search(r"(?m)^failures:", text)
    blob = text[m.start():] if m else text  # keep the heading itself
    return "\n".join(blob.splitlines()[:120])


def main() -> None:
    try:
        label = sys.argv[2] if len(sys.argv) > 2 else "failed step"
        path = Path(sys.argv[1])
        if not path.is_file():
            print(f"::error::{label}: no captured log ({path}) — the job "
                  "failed before this output was recorded")
            return
        text = path.read_text(encoding="utf-8", errors="replace")
        blob = digest(text) or text[-3000:]
        blob = blob.replace("%", "%25").replace("\n", "%0A")
        # annotation messages cap around 4096 chars; stay well inside
        print(f"::error::{label}: {blob[:3600]}")
    except Exception as e:  # noqa: BLE001 - publishing must never itself fail
        print(f"::error::{sys.argv[0]} broke: {e}")
    sys.exit(0)


if __name__ == "__main__":
    main()
