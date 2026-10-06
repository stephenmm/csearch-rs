"""Build, test, and install csearch-rs on Windows (or any OS).

Run:  py setup_csearch.py
Steps: locate cargo (offers winget install of rustup if missing) -> cargo build
--release -> cargo test --release -> copy cindex-rs/csearch-rs into
%USERPROFILE%\\bin -> verify the binaries run. Prints PASS/FIXED/FAIL lines;
exit code 1 on failure.

The binaries are deliberately not called cindex and csearch: those names
belong to the original codesearch, which this installs alongside. A build from
before the rename that is still sitting in the bin directory under the old
names is reported (NOTE), never deleted.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

PROJECT = Path(__file__).resolve().parent
EXE = ".exe" if os.name == "nt" else ""
BINARIES = ("cindex-rs", "csearch-rs")
# What csearch-rs 0.2 and earlier installed -- and what the original is called.
OLD_NAMES = ("cindex", "csearch")


def run(cmd: list[str], cwd: Path | None = None) -> tuple[int, str]:
    proc = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, encoding="utf-8", errors="replace")
    return proc.returncode, proc.stdout + proc.stderr


def find_cargo() -> str | None:
    found = shutil.which("cargo")
    if found:
        return found
    candidate = Path.home() / ".cargo" / "bin" / f"cargo{EXE}"
    return str(candidate) if candidate.exists() else None


def ensure_cargo(results: list[str]) -> str | None:
    cargo = find_cargo()
    if cargo:
        results.append(f"PASS: cargo found at {cargo}")
        return cargo
    if os.name == "nt" and shutil.which("winget"):
        print("cargo not found; installing rustup via winget (may prompt)...")
        code, out = run(["winget", "install", "-e", "--id", "Rustlang.Rustup", "--accept-package-agreements", "--accept-source-agreements"])
        cargo = find_cargo()
        if cargo:
            results.append(f"FIXED: installed rustup, cargo at {cargo}")
            return cargo
        results.append(f"FAIL: winget rustup install did not yield cargo (rc={code}): {out[-400:]}")
        return None
    results.append("FAIL: cargo not found; install Rust from https://rustup.rs and re-run")
    return None


def is_pre_rename_build(exe: Path, old_name: str) -> bool:
    """True if `exe` is a csearch-rs binary from before the rename.

    Such a build answers `--version` with "<old name> <semver>". The original
    codesearch has no --version flag at all, so it can never match.
    """
    try:
        code, out = run([str(exe), "--version"])
    except OSError:
        return False
    return code == 0 and re.fullmatch(rf"{re.escape(old_name)} \d+\.\d+\.\d+\S*", out.strip()) is not None


def main() -> int:
    results: list[str] = []
    cargo = ensure_cargo(results)
    if cargo is None:
        print("\n".join(results))
        return 1

    code, out = run([cargo, "build", "--release"], cwd=PROJECT)
    if code != 0:
        results.append(f"FAIL: cargo build --release\n{out[-2000:]}")
        print("\n".join(results))
        return 1
    results.append("PASS: cargo build --release")

    code, out = run([cargo, "test", "--release"], cwd=PROJECT)
    if code != 0:
        results.append(f"FAIL: cargo test --release\n{out[-2000:]}")
    else:
        passed = sum(int(line.split()[3]) for line in out.splitlines() if line.startswith("test result:"))
        results.append(f"PASS: cargo test --release ({passed} tests)")

    bin_dir = Path.home() / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    for name in BINARIES:
        src = PROJECT / "target" / "release" / f"{name}{EXE}"
        dst = bin_dir / f"{name}{EXE}"
        if not src.exists():
            results.append(f"FAIL: missing build output {src}")
            continue
        if dst.exists():
            shutil.copy2(dst, dst.with_suffix(dst.suffix + ".bak"))
        shutil.copy2(src, dst)
        code, out = run([str(dst), "--version"])
        if code == 0:
            results.append(f"PASS: installed {dst} ({out.strip()})")
        else:
            results.append(f"FAIL: {dst} --version rc={code}: {out[-300:]}")

    for old_name in OLD_NAMES:
        old = bin_dir / f"{old_name}{EXE}"
        if old.exists() and is_pre_rename_build(old, old_name):
            results.append(
                f"NOTE: {old} is a csearch-rs build from before the rename. It is not updated "
                f"any more, and it shadows the original codesearch's `{old_name}` -- delete it."
            )

    on_path = any(Path(p).resolve() == bin_dir.resolve() for p in os.environ.get("PATH", "").split(os.pathsep) if p)
    if on_path:
        results.append(f"PASS: {bin_dir} is on PATH")
    else:
        results.append(f"FAIL: {bin_dir} is not on PATH - add it (System Properties > Environment Variables) or call the binaries by full path")

    print("\n".join(results))
    failed = any(r.startswith("FAIL") for r in results)
    print("\nSUMMARY:", "FAIL" if failed else "PASS")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
