#!/usr/bin/env python3
"""Smoke-test the native CLI bridge and shell executor over Unix sockets."""

from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "debug"
CLI_VERSION = 2


def wait_for_socket(path: Path, process: subprocess.Popen[str]) -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if path.exists():
            return
        if process.poll() is not None:
            _, stderr = process.communicate()
            raise RuntimeError(f"{process.args[0]} exited {process.returncode}: {stderr}")
        time.sleep(0.02)
    raise TimeoutError(f"socket not created: {path}")


def exchange(
    path: Path, request: dict[str, object], max_response_bytes: int | None = None
) -> dict[str, object]:
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(3)
        connection.connect(str(path))
        connection.sendall(json.dumps(request).encode() + b"\n")
        response = bytearray()
        while not response.endswith(b"\n"):
            chunk = connection.recv(4096)
            if not chunk:
                break
            response.extend(chunk)
            if max_response_bytes is not None:
                assert len(response) <= max_response_bytes, "oversized response frame"
    return json.loads(response)


def stop(process: subprocess.Popen[str]) -> None:
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def cli_bridge_smoke(directory: Path) -> None:
    path = directory / "cli.sock"

    def start() -> subprocess.Popen[str]:
        process = subprocess.Popen(
            [str(BIN / "pluribus-cli-bridge"), "--socket", str(path), "--runtime-uid", str(os.getuid())],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        wait_for_socket(path, process)
        return process

    def poll(after: int, session_id: str | None = None) -> dict[str, object]:
        request = {"kind": "poll", "version": CLI_VERSION, "after": after, "timeout_ms": 1000}
        if session_id is not None:
            request["session_id"] = session_id
        return exchange(path, request, 256 * 1024)

    process = start()
    try:
        assert process.stdin is not None
        process.stdin.write("smoke input\n")
        process.stdin.flush()
        response = poll(0)
        assert response["status"] == "messages", response
        assert response["messages"][0]["text"] == "smoke input", response
        cursor = response["messages"][-1]["sequence"]
        session_id = response.get("session_id")
        stop(process)
        path.unlink(missing_ok=True)
        process = start()
        assert process.stdin is not None
        process.stdin.write("input after restart\n")
        process.stdin.flush()
        response = poll(cursor, session_id)
        assert [m["text"] for m in response["messages"]] == ["input after restart"], response
        assert response["session_id"] != session_id
        session_id = response["session_id"]
        cursor = response["messages"][-1]["sequence"]

        process.stdin.write("x" * (16 * 1024 + 1) + "\ninput after oversized line\n")
        process.stdin.flush()
        response = poll(cursor, session_id)
        assert [m["text"] for m in response["messages"]] == ["input after oversized line"], response
        cursor = response["messages"][-1]["sequence"]

        escaped = '\\"' * (8 * 1024)
        process.stdin.write((escaped + "\n") * 32)
        process.stdin.flush()
        received = []
        deadline = time.monotonic() + 10
        while len(received) < 32:
            assert time.monotonic() < deadline, "batch did not drain"
            response = poll(cursor, session_id)
            received.extend(message["text"] for message in response["messages"])
            if response["messages"]:
                cursor = response["messages"][-1]["sequence"]
        assert received == [escaped] * 32
    finally:
        stop(process)


def shell_executor_smoke(directory: Path) -> None:
    path = directory / "shell.sock"
    workspace = directory / "workspace"
    workspace.mkdir()
    process = subprocess.Popen(
        [
            str(BIN / "pluribus-shell-executor"),
            "--socket",
            str(path),
            "--workspace",
            str(workspace),
            "--runtime-uid",
            str(os.getuid()),
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    request = {
        "version": 3,
        "command": "printf smoke; pwd",
        "env": {},
        "timeout_ms": 2000,
        "invocation_id": "invocation",
        "authority_id": "authority",
        "activity_id": "activity",
        "origin_event_id": "origin",
    }
    try:
        wait_for_socket(path, process)
        response = exchange(path, request)["response"]
        assert response["status"] == "completed", response
        assert response["stdout"] == f"smoke{workspace}\n", response
        assert response["exit_code"] == 0, response
    finally:
        stop(process)


def main() -> None:
    with tempfile.TemporaryDirectory(prefix="pluribus-native-smoke-") as temporary:
        directory = Path(temporary)
        cli_bridge_smoke(directory)
        print("CLI bridge socket smoke: PASS")
        shell_executor_smoke(directory)
        print("Shell executor socket smoke: PASS")


if __name__ == "__main__":
    main()
