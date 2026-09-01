"""Compare Google's Go codesearch (cindex/csearch) against csearch-rs.

Run:  py compare_csearch.py --corpus E:\\proj

What it does, fully automated:
  1. Finds (or builds) the Rust binaries in target/release next to this script.
  2. Finds the Go binaries (PATH, %USERPROFILE%\\go\\bin, or --go-bin); if
     missing and `go` is available, runs `go install ...@latest` (falls back to
     v1.2.0); if `go` itself is missing on Windows, installs it with winget.
  3. Indexes the corpus with each implementation into separate temp index
     files (warm-up + timed runs), records index size and wall time.
  4. Runs every pattern with `csearch -c` on both, times them, and checks that
     the per-file match counts are IDENTICAL.
  5. Prints a table, writes compare_results.md next to this script, prints
     PASS/FAIL lines and exits 1 if any parity check failed.

Patterns: built-in set, or --patterns FILE (one per line; prefix a line with
"-i " for case-insensitive). Stdlib only, Python 3.12+.
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

EXE = ".exe" if os.name == "nt" else ""
SCRIPT_DIR = Path(__file__).resolve().parent
GO_MODULE = "github.com/google/codesearch"

DEFAULT_PATTERNS: list[tuple[str, bool]] = [
    ("fn main", False),
    ("unsafe impl Send", False),
    ("TODO|FIXME", False),
    ("foo.*bar", False),
    ("hello", False),
    ("Hello.*[Ww]orld", False),
    ("^use std::", False),
    ("impl<[A-Z]> .* for ", False),
    ("[0-9]{4}-[0-9]{2}-[0-9]{2}", False),
    ("license", True),
    ("deprecated", True),
]


@dataclass
class Impl:
    name: str
    cindex: Path
    csearch: Path
    index_path: Path
    index_secs: list[float] = field(default_factory=list)
    index_bytes: int = 0


@dataclass
class SearchResult:
    secs: list[float]
    counts: dict[str, int]
    error: str | None = None


def which_any(name: str, extra_dirs: list[Path]) -> Path | None:
    for d in extra_dirs:
        candidate = d / f"{name}{EXE}"
        if candidate.exists():
            return candidate
    found = shutil.which(name)
    return Path(found) if found else None


def run(cmd: list[str], env: dict[str, str] | None = None, cwd: Path | None = None) -> tuple[int, str, str]:
    proc = subprocess.run(
        cmd, env=env, cwd=cwd, capture_output=True, text=True, encoding="utf-8", errors="replace"
    )
    return proc.returncode, proc.stdout, proc.stderr


def timed(cmd: list[str], env: dict[str, str]) -> tuple[float, int, str, str]:
    t0 = time.perf_counter()
    code, out, err = run(cmd, env=env)
    return time.perf_counter() - t0, code, out, err


# ---------------------------------------------------------------- locating


def find_rust(rust_dir: Path, results: list[str], no_build: bool) -> tuple[Path, Path] | None:
    rel = rust_dir / "target" / "release"
    cindex, csearch = rel / f"cindex{EXE}", rel / f"csearch{EXE}"
    if cindex.exists() and csearch.exists():
        results.append(f"PASS: rust binaries at {rel}")
        return cindex, csearch
    if no_build:
        results.append(f"FAIL: rust binaries missing in {rel} and --no-build given")
        return None
    cargo = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / f"cargo{EXE}")
    if not Path(cargo).exists():
        results.append("FAIL: cargo not found; run setup_csearch.py first")
        return None
    code, _, err = run([cargo, "build", "--release"], cwd=rust_dir)
    if code != 0 or not (cindex.exists() and csearch.exists()):
        results.append(f"FAIL: cargo build --release failed: {err[-800:]}")
        return None
    results.append(f"FIXED: built rust binaries in {rel}")
    return cindex, csearch


def go_bin_dir() -> Path | None:
    go = shutil.which("go")
    if not go:
        return None
    code, out, _ = run([go, "env", "GOBIN"])
    if code == 0 and out.strip():
        return Path(out.strip())
    code, out, _ = run([go, "env", "GOPATH"])
    if code == 0 and out.strip():
        return Path(out.strip().split(os.pathsep)[0]) / "bin"
    return None


def find_go(go_bin: Path | None, results: list[str], no_build: bool) -> tuple[Path, Path] | None:
    dirs = [d for d in (go_bin, go_bin_dir(), Path.home() / "go" / "bin") if d is not None]
    cindex, csearch = which_any("cindex", dirs), which_any("csearch", dirs)
    # Guard against picking up the Rust binaries as "Go" ones.
    if cindex and csearch and "csearch-rs" not in str(cindex):
        results.append(f"PASS: go binaries at {cindex.parent}")
        return cindex, csearch
    if no_build:
        results.append("FAIL: go codesearch binaries not found and --no-build given")
        return None
    if not shutil.which("go"):
        if os.name == "nt" and shutil.which("winget"):
            print("go not found; installing via winget...")
            run(["winget", "install", "-e", "--id", "GoLang.Go", "--accept-package-agreements", "--accept-source-agreements"])
            for candidate in (Path(os.environ.get("ProgramFiles", r"C:\Program Files")) / "Go" / "bin",):
                if (candidate / f"go{EXE}").exists():
                    os.environ["PATH"] = f"{candidate}{os.pathsep}{os.environ['PATH']}"
        if not shutil.which("go"):
            results.append("FAIL: go toolchain not found; install from https://go.dev/dl and re-run")
            return None
    go = shutil.which("go") or "go"
    for version in ("latest", "v1.2.0"):
        ok = True
        for tool in ("cindex", "csearch"):
            code, _, err = run([go, "install", f"{GO_MODULE}/cmd/{tool}@{version}"])
            if code != 0:
                ok = False
                print(f"  go install {tool}@{version} failed: {err.strip()[-300:]}")
                break
        if ok:
            break
    dirs = [d for d in (go_bin_dir(), Path.home() / "go" / "bin") if d is not None]
    cindex, csearch = which_any("cindex", dirs), which_any("csearch", dirs)
    if cindex and csearch:
        results.append(f"FIXED: installed go codesearch to {cindex.parent}")
        return cindex, csearch
    results.append("FAIL: go install of codesearch did not produce binaries")
    return None


# ---------------------------------------------------------------- measuring


def index_corpus(impl: Impl, corpus: Path, runs: int, results: list[str]) -> bool:
    env = dict(os.environ, CSEARCHINDEX=str(impl.index_path))
    for i in range(runs + 1):  # first run is warm-up
        if impl.index_path.exists():
            impl.index_path.unlink()
        secs, code, _, err = timed([str(impl.cindex), str(corpus)], env)
        if code != 0 or not impl.index_path.exists():
            results.append(f"FAIL: {impl.name} cindex failed (rc={code}): {err.strip()[-400:]}")
            return False
        if i > 0:
            impl.index_secs.append(secs)
    impl.index_bytes = impl.index_path.stat().st_size
    results.append(f"PASS: {impl.name} indexed {corpus} in {min(impl.index_secs):.3f}s (best of {runs})")
    return True


def parse_counts(out: str, corpus: Path) -> dict[str, int]:
    counts: dict[str, int] = {}
    corpus_key = os.path.normcase(str(corpus))
    for line in out.splitlines():
        head, sep, tail = line.rpartition(":")
        if not sep or not tail.strip().isdigit():
            continue
        path = os.path.normcase(os.path.normpath(head.strip()))
        if path.startswith(corpus_key):
            path = path[len(corpus_key):].lstrip("\\/")
        counts[path] = int(tail)
    return counts


def search(impl: Impl, pattern: str, icase: bool, corpus: Path, runs: int) -> SearchResult:
    env = dict(os.environ, CSEARCHINDEX=str(impl.index_path))
    cmd = [str(impl.csearch), "-c"] + (["-i"] if icase else []) + [pattern]
    secs: list[float] = []
    counts: dict[str, int] = {}
    for i in range(runs + 1):
        t, code, out, err = timed(cmd, env)
        if code not in (0, 1):  # 1 = no matches, like grep
            return SearchResult(secs=[], counts={}, error=err.strip()[-300:] or f"rc={code}")
        if i == 0:
            counts = parse_counts(out, corpus)
        else:
            secs.append(t)
    return SearchResult(secs=secs, counts=counts)


def fmt_ms(secs: list[float]) -> str:
    if not secs:
        return "-"
    return f"{min(secs) * 1000:7.1f} / {statistics.median(secs) * 1000:7.1f}"


def diff_summary(go: dict[str, int], rs: dict[str, int]) -> str:
    only_go = sorted(set(go) - set(rs))
    only_rs = sorted(set(rs) - set(go))
    mismatched = sorted(k for k in set(go) & set(rs) if go[k] != rs[k])
    parts: list[str] = []
    if only_go:
        parts.append(f"only in go ({len(only_go)}): " + ", ".join(only_go[:3]))
    if only_rs:
        parts.append(f"only in rust ({len(only_rs)}): " + ", ".join(only_rs[:3]))
    if mismatched:
        parts.append(f"count differs ({len(mismatched)}): " + ", ".join(f"{k} go={go[k]} rs={rs[k]}" for k in mismatched[:3]))
    return "; ".join(parts)


# ---------------------------------------------------------------- main


def load_patterns(path: Path | None) -> list[tuple[str, bool]]:
    if path is None:
        return DEFAULT_PATTERNS
    out: list[tuple[str, bool]] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("-i "):
            out.append((line[3:].strip(), True))
        else:
            out.append((line, False))
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--corpus", type=Path, default=None, help="directory to index (default: E:\\proj if present, else the Rust project dir)")
    ap.add_argument("--rust-dir", type=Path, default=SCRIPT_DIR, help="csearch-rs project directory")
    ap.add_argument("--go-bin", type=Path, default=None, help="directory holding the Go cindex/csearch")
    ap.add_argument("--runs", type=int, default=5, help="timed runs per measurement (after one warm-up)")
    ap.add_argument("--index-runs", type=int, default=2, help="timed index builds per implementation")
    ap.add_argument("--patterns", type=Path, default=None, help="file of patterns, one per line")
    ap.add_argument("--no-build", action="store_true", help="never build/install anything")
    args = ap.parse_args()

    results: list[str] = []
    corpus: Path = args.corpus or (Path("E:/proj") if Path("E:/proj").is_dir() else args.rust_dir)
    corpus = corpus.resolve()
    if not corpus.is_dir():
        print(f"FAIL: corpus {corpus} is not a directory")
        return 1

    rust = find_rust(args.rust_dir.resolve(), results, args.no_build)
    go = find_go(args.go_bin, results, args.no_build)
    if rust is None or go is None:
        print("\n".join(results))
        return 1

    tmp = Path(tempfile.mkdtemp(prefix="csearch-compare-"))
    impls = [
        Impl("go", go[0], go[1], tmp / "go.index"),
        Impl("rust", rust[0], rust[1], tmp / "rust.index"),
    ]
    print(f"corpus: {corpus}")
    for impl in impls:
        print(f"indexing with {impl.name} ({impl.cindex}) ...")
        if not index_corpus(impl, corpus, args.index_runs, results):
            print("\n".join(results))
            return 1

    patterns = load_patterns(args.patterns)
    rows: list[str] = []
    header = f"{'pattern':34} {'go ms (min/med)':>17} {'rust ms (min/med)':>17} {'speedup':>8}  parity"
    print(header)
    print("-" * len(header))
    parity_fail = 0
    for pattern, icase in patterns:
        label = ("-i " if icase else "") + pattern
        g = search(impls[0], pattern, icase, corpus, args.runs)
        r = search(impls[1], pattern, icase, corpus, args.runs)
        if g.error or r.error:
            status = f"ERROR go={g.error or 'ok'} rust={r.error or 'ok'}"
            parity_fail += 1
        elif g.counts == r.counts:
            status = f"OK ({len(r.counts)} files)"
        else:
            status = "MISMATCH " + diff_summary(g.counts, r.counts)
            parity_fail += 1
        speed = f"{min(g.secs) / min(r.secs):7.2f}x" if g.secs and r.secs else "-"
        row = f"{label[:34]:34} {fmt_ms(g.secs):>17} {fmt_ms(r.secs):>17} {speed:>8}  {status}"
        print(row)
        rows.append(row)

    print()
    idx_line = "index build (best): " + "   ".join(
        f"{i.name} {min(i.index_secs):.3f}s / {i.index_bytes / 1e6:.1f} MB" for i in impls
    )
    print(idx_line)
    if all(i.index_secs for i in impls):
        print(f"index speedup: {min(impls[0].index_secs) / min(impls[1].index_secs):.2f}x")

    report = SCRIPT_DIR / "compare_results.md"
    with report.open("w", encoding="utf-8") as fh:
        fh.write(f"# csearch comparison\n\n- date: {time.strftime('%Y-%m-%d %H:%M')}\n- corpus: `{corpus}`\n")
        fh.write(f"- machine: {platform.platform()}, {os.cpu_count()} cpus\n- go: `{impls[0].csearch}`\n- rust: `{impls[1].csearch}`\n\n")
        fh.write(f"{idx_line}\n\n```\n{header}\n{'-' * len(header)}\n" + "\n".join(rows) + "\n```\n")
    results.append(f"{'PASS' if parity_fail == 0 else 'FAIL'}: parity on {len(patterns) - parity_fail}/{len(patterns)} patterns")
    results.append(f"PASS: wrote {report}")

    shutil.rmtree(tmp, ignore_errors=True)
    print()
    print("\n".join(results))
    failed = any(r.startswith("FAIL") for r in results)
    print("\nSUMMARY:", "FAIL" if failed else "PASS")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
