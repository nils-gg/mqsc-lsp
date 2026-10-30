#!/usr/bin/env python3
"""Publish a compact failure digest of a captured log as ONE ::error::
annotation, so anonymous visitors can read *which* tests failed.

GitHub hides job logs and step summaries from anyone without repo rights;
annotations (the "Annotations" box on the check-run page) are public even
on public repos. Platform jobs therefore tee their output to a file and,
on failure, run this tool over it (the full tail still goes to the step
summary for logged-in readers).

Usage: ci_publish_failure.py <logfile> <label>
Never fails: a broken digest must not mask the job's real failure.
"""
import re
import sys


def digest(text: str) -> str:
    # the Python smoke harnesses report through "[FAIL]" markers plus a
    # trailing summary; cargo test prints its whole story after the first
    # blank-line-anchored "failures:" block. Anything else: tail of the log.
    lines = text.splitlines()
    fails = [l for l in lines if "[FAIL]" in l]
    if fails:
        return "\n".join(fails[-20:] + ["--- log tail ---"] + lines[-30:])
    i = text.find("\nfailures:")
    blob = text[i + 1:] if i != -1 else text
    return "\n".join(blob.splitlines()[:120])


def main() -> None:
    try:
        text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
        label = sys.argv[2] if len(sys.argv) > 2 else "failed step"
        blob = digest(text) or text[-3000:]
        blob = re.sub(r"\x1b\[[0-9;]*m", "", blob)  # CARGO_TERM_COLOR=always
        blob = blob.replace("%", "%25").replace("\r", "").replace("\n", "%0A")
        # annotation messages cap around 4096 chars; stay well inside
        print(f"::error::{label}: {blob[:3600]}")
    except Exception as e:  # noqa: BLE001 - publishing must never itself fail
        print(f"::error::{sys.argv[0]} broke: {e}")
    sys.exit(0)


if __name__ == "__main__":
    main()
