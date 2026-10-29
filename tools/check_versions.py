#!/usr/bin/env python3
"""Fail if the release versions disagree (patch.md Phase 5).

The server binary and the VS Code extension ship as one product, so their
versions must match; a git tag additionally must name exactly that version,
otherwise a GitHub Release would advertise artifacts it did not build.

Usage:
    python3 tools/check_versions.py            # Cargo.toml vs package.json
    python3 tools/check_versions.py v0.1.0     # ... and the tag (release job)

Exits non-zero on any divergence and prints what each source says.
"""
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CARGO = os.path.join(ROOT, "server", "Cargo.toml")
PKGJSON = os.path.join(ROOT, "editors", "vscode", "extension", "package.json")


def cargo_version():
    """The [package] version — first bare `version = ` before any other
    table header, so dependency pins can never masquerade as the package."""
    section = ""
    with open(CARGO, encoding="utf-8") as f:
        for line in f:
            header = re.match(r"^\[([^\]]+)\]", line.strip())
            if header:
                section = header.group(1)
                continue
            if section == "package":
                m = re.match(r'^version\s*=\s*"([^"]+)"', line.strip())
                if m:
                    return m.group(1)
    raise SystemExit(f"no [package] version in {CARGO}")


def package_version():
    with open(PKGJSON, encoding="utf-8") as f:
        try:
            return json.load(f)["version"]
        except (ValueError, KeyError) as e:
            raise SystemExit(f"no usable version in {PKGJSON}: {e}")


def main():
    cargo = cargo_version()
    pkg = package_version()
    tag = None
    if len(sys.argv) > 1:
        tag = sys.argv[1]
        if not tag.startswith("v"):
            print(f"FAIL: tag {tag!r} does not start with 'v' (release workflow "
                  f"triggers on vX.Y.Z)")
            return 1
        tag = tag[1:]

    print(f"  server/Cargo.toml:   {cargo}")
    print(f"  extension package.json: {pkg}")
    if tag is not None:
        print(f"  git tag:             {sys.argv[1]} -> {tag}")

    versions = {"Cargo.toml": cargo, "package.json": pkg}
    if tag is not None:
        versions["tag"] = tag
    if len(set(versions.values())) != 1:
        print("FAIL: versions diverge — release artifacts and the extension "
              "would claim different things")
        return 1
    print(f"PASS: all sources agree on {cargo}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
