#!/usr/bin/env python3
"""Run the official MCP SDK interoperability smoke matrix."""

from __future__ import annotations

import contextlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
from collections.abc import Iterator, Sequence


ROOT = Path(__file__).resolve().parents[2]
FIXTURE_ROOT = Path(__file__).resolve().parent
TYPESCRIPT = FIXTURE_ROOT / "typescript"
PYTHON = FIXTURE_ROOT / "python"
PROTOCOLS = ("2025-11-25", "2026-07-28")


def run(command: Sequence[str], *, cwd: Path = ROOT) -> None:
    printable = " ".join(command)
    print(f"[RUN] {printable}", flush=True)
    completed = subprocess.run(
        command,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    if completed.stdout:
        print(completed.stdout, end="" if completed.stdout.endswith("\n") else "\n")
    if completed.returncode != 0:
        raise RuntimeError(f"command failed with exit code {completed.returncode}: {printable}")


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as candidate:
        candidate.bind(("127.0.0.1", 0))
        return int(candidate.getsockname()[1])


def wait_for_port(process: subprocess.Popen[str], port: int, log_path: Path) -> None:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if process.poll() is not None:
            log = log_path.read_text(encoding="utf-8")
            raise RuntimeError(f"server exited before readiness (exit {process.returncode}):\n{log}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    log = log_path.read_text(encoding="utf-8")
    raise RuntimeError(f"server did not listen on port {port} within 15 seconds:\n{log}")


@contextlib.contextmanager
def server_process(
    name: str,
    command: Sequence[str],
    port: int,
    log_dir: Path,
) -> Iterator[None]:
    log_path = log_dir / f"{name}.log"
    print(f"[SERVER] {name}: {' '.join(command)}", flush=True)
    with log_path.open("w", encoding="utf-8") as log_file:
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            text=True,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            env={**os.environ, "PYTHONUNBUFFERED": "1"},
        )
        try:
            wait_for_port(process, port, log_path)
            yield
        except Exception:
            log_file.flush()
            print(f"--- {name} server log ---", file=sys.stderr)
            print(log_path.read_text(encoding="utf-8"), file=sys.stderr)
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


def rust_binary() -> Path:
    metadata = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        text=True,
        stdout=subprocess.PIPE,
        check=True,
    )
    suffix = ".exe" if os.name == "nt" else ""
    return Path(json.loads(metadata.stdout)["target_directory"]) / "debug" / f"sdk-interop{suffix}"


def prepare() -> Path:
    run(["npm", "ci"], cwd=TYPESCRIPT)
    run(["uv", "sync", "--frozen"], cwd=PYTHON)
    run(["cargo", "build", "--locked", "-p", "sdk-interop"])
    binary = rust_binary()
    if not binary.is_file():
        raise RuntimeError(f"cargo did not produce {binary}")
    return binary


def main() -> None:
    binary = prepare()
    python = ["uv", "run", "--project", str(PYTHON), "--frozen", "python"]

    with tempfile.TemporaryDirectory(prefix="tower-mcp-sdk-interop-") as temporary:
        log_dir = Path(temporary)

        typescript_port = free_port()
        typescript_url = f"http://127.0.0.1:{typescript_port}/mcp"
        with server_process(
            "typescript",
            ["node", str(TYPESCRIPT / "server.mjs"), str(typescript_port)],
            typescript_port,
            log_dir,
        ):
            for protocol in PROTOCOLS:
                run([str(binary), "client", typescript_url, protocol])

        python_port = free_port()
        python_url = f"http://127.0.0.1:{python_port}/mcp"
        with server_process(
            "python",
            [*python, str(PYTHON / "server.py"), str(python_port)],
            python_port,
            log_dir,
        ):
            for protocol in PROTOCOLS:
                run([str(binary), "client", python_url, protocol])

        tower_port = free_port()
        tower_url = f"http://127.0.0.1:{tower_port}"
        with server_process(
            "tower-mcp",
            [str(binary), "server", str(tower_port)],
            tower_port,
            log_dir,
        ):
            for protocol in PROTOCOLS:
                run(["node", str(TYPESCRIPT / "client.mjs"), tower_url, protocol])
                run([*python, str(PYTHON / "client.py"), tower_url, protocol])

    print("\nPASS: 8/8 official SDK interoperability legs", flush=True)


if __name__ == "__main__":
    main()
