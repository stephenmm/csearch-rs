#!/usr/bin/env python3
"""Fail unless Cargo.toml's [package] version increased from the PR's base.

Compares two Cargo.toml files as semver (MAJOR, MINOR, PATCH) tuples and
requires the second to be strictly greater than the first. A PR title
containing "[skip version]" (case-insensitive) bypasses the check, for
changes that genuinely touch nothing release-worthy (CI config, docs, this
script itself).

Usage: check_version_bump.py <base-cargo-toml> <head-cargo-toml>
Reads PR_TITLE from the environment for the skip marker.
"""

from __future__ import annotations

import os
import re
import sys
from pathlib import Path

Version = tuple[int, int, int]


def version_of(path: Path) -> Version:
    text = path.read_text(encoding="utf-8")
    m = re.search(r'(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"', text)
    if not m:
        sys.exit(f"FAIL: no [package] version = \"X.Y.Z\" line found in {path}")
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)))


def fmt(v: Version) -> str:
    return ".".join(map(str, v))


def main() -> int:
    if len(sys.argv) != 3:
        sys.exit("usage: check_version_bump.py <base-cargo-toml> <head-cargo-toml>")
    base = version_of(Path(sys.argv[1]))
    head = version_of(Path(sys.argv[2]))
    print(f"base version: {fmt(base)}")
    print(f"head version: {fmt(head)}")

    if head > base:
        print(f"PASS: version bumped ({fmt(base)} -> {fmt(head)})")
        return 0

    title = os.environ.get("PR_TITLE", "")
    if re.search(r"\[skip version\]", title, re.IGNORECASE):
        print(f"PASS: version unchanged, but the PR title requests a skip: {title!r}")
        return 0

    print(
        f"FAIL: version did not increase ({fmt(base)} -> {fmt(head)}). "
        f"Bump [package].version in Cargo.toml, or add \"[skip version]\" "
        f"to the PR title if this change genuinely needs no release."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
