#!/usr/bin/env python3
"""Build the native wakeup runtime using Codex's pinned V8 package artifacts."""

import os
from pathlib import Path
import shutil
import subprocess
import sys


def main():
    # The canonical package helpers use Python 3.11's tomllib. Resolve an
    # already installed interpreter; never install tools or change shell setup.
    if sys.version_info < (3, 11):
        candidates = [shutil.which(f"python3.{minor}") for minor in (14, 13, 12, 11)]
        uv = shutil.which("uv")
        if uv:
            found = subprocess.run(
                [uv, "python", "find", "--offline", "--no-python-downloads", ">=3.11"],
                capture_output=True, text=True,
            )
            if found.returncode == 0:
                candidates.append(found.stdout.strip())
        for python in filter(None, candidates):
            supported = subprocess.run(
                [python, "-c", "import sys; sys.exit(sys.version_info < (3, 11))"],
                capture_output=True,
            )
            if supported.returncode == 0:
                os.execv(python, [python, str(Path(__file__).resolve()), *sys.argv[1:]])
        raise RuntimeError("Codex V8 package setup needs an installed Python 3.11 or newer")

    repo = Path(__file__).resolve().parents[1]
    os.environ["CODEX_REPO_ROOT"] = str(repo)
    sys.path.insert(0, str(repo / "scripts"))
    from codex_package.targets import TARGET_SPECS, default_target
    from codex_package.v8 import resolve_codex_v8_cargo_env

    command = sys.argv[1:]
    if command[:1] == ["--"]:
        command = command[1:]
    if not command or command[:2] != ["cargo", "build"]:
        raise RuntimeError("Pass one native cargo build command after --")
    target = Path(os.environ.get("CARGO_TARGET_DIR", repo / "codex-rs/target")).resolve()
    env = dict(os.environ)
    env.update(resolve_codex_v8_cargo_env(
        TARGET_SPECS[default_target()], cache_root=target / "wakeup-v8",
    ))
    print("Using checksum-verified native Codex V8 package artifacts", flush=True)
    return subprocess.call(command, cwd=repo / "codex-rs", env=env)


if __name__ == "__main__":
    sys.exit(main())
