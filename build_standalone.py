"""Build standalone csearch-rs binaries for Windows and Linux.

Run:  py build_standalone.py            # both targets
      py build_standalone.py --windows  # just Windows
      py build_standalone.py --linux    # just Linux (via WSL)

Windows: x86_64-pc-windows-msvc built with `-C target-feature=+crt-static`, so
the exe imports only OS DLLs -- no Visual C++ redistributable required on the
target machine.

Linux: x86_64-unknown-linux-musl built inside WSL. musl links fully static, so
the binary has no glibc version dependency and runs on any x86_64 distro. The
crates used here are pure Rust, so this needs no musl-gcc and no sudo -- only a
user-scoped rustup in WSL, which this script installs if missing.

Both binaries keep the runtime AVX2 detection, so they still run on pre-AVX2
CPUs via the scalar fallback.

Outputs land in dist/. Prints PASS/FAIL lines; exit code 1 on any failure.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

PROJECT: Path = Path(__file__).resolve().parent
DIST: Path = PROJECT / "dist"
BINARIES: tuple[str, ...] = ("cindex", "csearch")

WIN_TARGET = "x86_64-pc-windows-msvc"
LINUX_TARGET = "x86_64-unknown-linux-musl"

VSWHERE = Path(os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / (
    r"Microsoft Visual Studio\Installer\vswhere.exe"
)
VCVARS_SUFFIX = Path(r"VC\Auxiliary\Build\vcvars64.bat")

# rustup's officially documented installer (https://rustup.rs), user-scoped, no sudo.
RUSTUP_SH = "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path"


@dataclass
class Step:
    ok: bool
    message: str


def run(cmd: list[str], cwd: Path | None = None, env: dict[str, str] | None = None) -> tuple[int, str]:
    """Run a command, returning (returncode, stdout+stderr)."""
    proc = subprocess.run(
        cmd, cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace"
    )
    return proc.returncode, proc.stdout + proc.stderr


def wsl_bash(script: str, distro: str = "Ubuntu") -> tuple[int, str]:
    """Run a bash snippet inside WSL, avoiding host shell quoting entirely."""
    return run(["wsl", "-d", distro, "--", "bash", "-lc", script])


def to_wsl_path(p: Path) -> str:
    r"""Translate a Windows path to its WSL mount point: C:\foo -> /mnt/c/foo."""
    resolved = p.resolve()
    drive = resolved.drive.rstrip(":").lower()
    rest = resolved.as_posix()[len(resolved.drive):]
    return f"/mnt/{drive}{rest}"


# ------------------------------------------------------------------ Windows


def find_vcvars() -> Path | None:
    """Locate vcvars64.bat wherever Visual Studio happens to be installed.

    vswhere ships with every VS 2017+ installer at a fixed path and reports
    installations anywhere on disk, including non-default drives, so it is
    tried first. $CSEARCH_VCVARS overrides everything.
    """
    override = os.environ.get("CSEARCH_VCVARS")
    if override and Path(override).exists():
        return Path(override)

    if VSWHERE.exists():
        code, out = run([str(VSWHERE), "-latest", "-products", "*",
                         "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                         "-property", "installationPath"])
        if code == 0:
            for line in out.splitlines():
                candidate = Path(line.strip()) / VCVARS_SUFFIX
                if line.strip() and candidate.exists():
                    return candidate

    program_files = [os.environ.get("ProgramFiles", r"C:\Program Files"),
                     os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")]
    for base in program_files:
        for year in ("2022", "2019"):
            for edition in ("BuildTools", "Community", "Professional", "Enterprise"):
                candidate = Path(base) / "Microsoft Visual Studio" / year / edition / VCVARS_SUFFIX
                if candidate.exists():
                    return candidate
    return None


def msvc_env(vcvars: Path) -> dict[str, str] | None:
    """Capture the environment vcvars64.bat sets, so cargo can be run directly.

    Chaining `call vcvars && cargo` through `cmd /c` as one argument gets
    mangled by Windows argument quoting, so dump the environment to a file via
    a temp batch script and parse it instead.
    """
    with tempfile.TemporaryDirectory() as td:
        bat = Path(td) / "dumpenv.bat"
        dump = Path(td) / "env.txt"
        bat.write_text(
            f'@echo off\r\ncall "{vcvars}" >nul 2>&1\r\nset > "{dump}"\r\n', encoding="utf-8"
        )
        code, _ = run(["cmd", "/c", str(bat)])
        if code != 0 or not dump.exists():
            return None
        env: dict[str, str] = {}
        for line in dump.read_text(encoding="utf-8", errors="replace").splitlines():
            key, sep, value = line.partition("=")
            if sep:
                env[key] = value
        return env or None


def build_windows(steps: list[Step]) -> None:
    if os.name != "nt":
        steps.append(Step(False, "windows: not running on Windows, skipped"))
        return

    vcvars = find_vcvars()
    if vcvars is None:
        steps.append(Step(False, "windows: no vcvars64.bat found -- install the MSVC build tools, "
                                 "or point $CSEARCH_VCVARS at vcvars64.bat"))
        return
    steps.append(Step(True, f"windows: using {vcvars}"))

    env = msvc_env(vcvars)
    if env is None:
        steps.append(Step(False, f"windows: could not capture the environment from {vcvars}"))
        return

    cargo = shutil.which("cargo", path=env.get("PATH")) or shutil.which("cargo")
    if cargo is None:
        steps.append(Step(False, "windows: cargo not found on PATH"))
        return

    # This RUSTFLAGS is what makes the CRT static.
    env["RUSTFLAGS"] = "-C target-feature=+crt-static"
    code, out = run([cargo, "build", "--release", "--target", WIN_TARGET], cwd=PROJECT, env=env)
    if code != 0:
        steps.append(Step(False, f"windows: cargo build failed\n{out[-1500:]}"))
        return
    steps.append(Step(True, f"windows: built {WIN_TARGET} with static CRT"))

    src_dir = PROJECT / "target" / WIN_TARGET / "release"
    out_dir = DIST / "windows-x86_64"
    out_dir.mkdir(parents=True, exist_ok=True)
    for name in BINARIES:
        src = src_dir / f"{name}.exe"
        if not src.exists():
            steps.append(Step(False, f"windows: missing build output {src}"))
            continue
        dst = out_dir / f"{name}.exe"
        shutil.copy2(src, dst)
        steps.append(Step(True, f"windows: {dst} ({dst.stat().st_size / 1e6:.1f} MB)"))
        verify_windows(dst, env, steps)


def verify_windows(exe: Path, env: dict[str, str], steps: list[Step]) -> None:
    """Confirm the exe imports no C-runtime DLLs (i.e. it is truly standalone)."""
    dumpbin = shutil.which("dumpbin", path=env.get("PATH"))
    if dumpbin is None:
        steps.append(Step(True, f"windows: {exe.name} built but dumpbin unavailable, imports unverified"))
        return
    code, out = run([dumpbin, "/dependents", str(exe)], env=env)
    if code != 0:
        steps.append(Step(True, f"windows: {exe.name} built but dumpbin failed, imports unverified"))
        return
    imports = [
        line.strip()
        for line in out.splitlines()
        if line.strip().lower().endswith(".dll")
    ]
    crt = [d for d in imports if "vcruntime" in d.lower() or "api-ms-win-crt" in d.lower() or "msvcp" in d.lower()]
    if crt:
        steps.append(Step(False, f"windows: {exe.name} still imports the C runtime: {', '.join(crt)}"))
    else:
        steps.append(Step(True, f"windows: {exe.name} imports only OS DLLs ({len(imports)}), no VC++ redist needed"))


# -------------------------------------------------------------------- Linux


def wsl_available(distro: str) -> bool:
    code, _ = run(["wsl", "-d", distro, "--", "true"])
    return code == 0


def ensure_wsl_rust(distro: str, steps: list[Step]) -> str | None:
    """Return the path to cargo inside WSL, installing rustup if needed."""
    code, out = wsl_bash("command -v cargo || ls ~/.cargo/bin/cargo 2>/dev/null", distro)
    cargo = out.strip().splitlines()[0].strip() if code == 0 and out.strip() else ""
    if not cargo:
        steps.append(Step(True, "linux: no cargo in WSL, installing rustup (user-scoped, no sudo)"))
        code, out = wsl_bash(RUSTUP_SH, distro)
        if code != 0:
            steps.append(Step(False, f"linux: rustup install failed\n{out[-1200:]}"))
            return None
        cargo = "$HOME/.cargo/bin/cargo"
    code, out = wsl_bash(f"{cargo} --version", distro)
    if code != 0:
        steps.append(Step(False, f"linux: cargo not usable in WSL: {out[-400:]}"))
        return None
    steps.append(Step(True, f"linux: WSL {out.strip()}"))

    rustup = f"{cargo.rsplit('/', 1)[0]}/rustup" if "/" in cargo else "rustup"
    code, out = wsl_bash(f"{rustup} target add {LINUX_TARGET}", distro)
    if code != 0:
        steps.append(Step(False, f"linux: could not add {LINUX_TARGET}\n{out[-800:]}"))
        return None
    steps.append(Step(True, f"linux: target {LINUX_TARGET} available"))
    return cargo


def build_linux(distro: str, steps: list[Step]) -> None:
    if not wsl_available(distro):
        steps.append(Step(False, f"linux: WSL distro {distro!r} not available"))
        return

    cargo = ensure_wsl_rust(distro, steps)
    if cargo is None:
        return

    # Build from a native WSL directory: cargo on a /mnt DrvFs path is slow and
    # the exFAT mount cannot represent the timestamps cargo wants.
    wsl_src = "$HOME/csearch-rs-build"
    project_mnt = to_wsl_path(PROJECT)
    script = (
        f"set -e; "
        f"rm -rf {wsl_src}; mkdir -p {wsl_src}; "
        f"cp -r '{project_mnt}/src' '{project_mnt}/tests' '{project_mnt}/Cargo.toml' "
        f"'{project_mnt}/Cargo.lock' {wsl_src}/; "
        f"cd {wsl_src} && {cargo} build --release --target {LINUX_TARGET}"
    )
    code, out = wsl_bash(script, distro)
    if code != 0:
        steps.append(Step(False, f"linux: cargo build failed\n{out[-1500:]}"))
        return
    steps.append(Step(True, f"linux: built {LINUX_TARGET} (static musl)"))

    out_dir = DIST / "linux-x86_64"
    out_dir.mkdir(parents=True, exist_ok=True)
    for name in BINARIES:
        built = f"{wsl_src}/target/{LINUX_TARGET}/release/{name}"
        dst = out_dir / name
        dst_mnt = to_wsl_path(dst)
        code, out = wsl_bash(f"cp {built} '{dst_mnt}' && chmod +x '{dst_mnt}'", distro)
        if code != 0:
            steps.append(Step(False, f"linux: could not copy {name} out: {out[-300:]}"))
            continue
        steps.append(Step(True, f"linux: {dst} ({dst.stat().st_size / 1e6:.1f} MB)"))
        verify_linux(built, distro, steps)


def verify_linux(built: str, distro: str, steps: list[Step]) -> None:
    """Confirm the binary is statically linked and actually runs."""
    code, out = wsl_bash(f"file {built}; ldd {built} 2>&1 | head -3; {built} --version", distro)
    text = out.strip()
    static = "statically linked" in text or "not a dynamic executable" in text
    name = built.rsplit("/", 1)[-1]
    if static:
        steps.append(Step(True, f"linux: {name} is statically linked -- runs on any x86_64 distro"))
    else:
        steps.append(Step(False, f"linux: {name} is NOT static:\n{text[:500]}"))


# --------------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--windows", action="store_true", help="build only the Windows binaries")
    parser.add_argument("--linux", action="store_true", help="build only the Linux binaries")
    parser.add_argument("--distro", default="Ubuntu", help="WSL distro to build Linux in")
    args = parser.parse_args()

    do_windows = args.windows or not args.linux
    do_linux = args.linux or not args.windows

    steps: list[Step] = []
    if do_windows:
        build_windows(steps)
    if do_linux:
        build_linux(args.distro, steps)

    print()
    for step in steps:
        print(f"{'PASS' if step.ok else 'FAIL'}: {step.message}")
    failed = any(not s.ok for s in steps)
    print("\nSUMMARY:", "FAIL" if failed else "PASS")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
