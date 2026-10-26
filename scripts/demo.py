#!/usr/bin/env python3
"""Run the real HTTP server, kill it while CI is pending, then resume it.

Only the Python standard library, Cargo, and a Rust toolchain are required.
No GitHub credentials, model API keys, curl, or jq are needed.
"""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
SESSION_ID = "acme:widgets:42"


def main():
    subprocess.run(["cargo", "build", "--locked"], cwd=ROOT, check=True)
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=ROOT
    ))
    binary = Path(metadata["target_directory"]) / "debug" / "ngrc-server"
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    base_url = f"http://127.0.0.1:{port}"
    process = None

    def request(path, body=None):
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Content-Type": "application/json", "X-GitHub-Event": "workflow_run",
                   "X-GitHub-Delivery": str(uuid.uuid4())}
        req = urllib.request.Request(base_url + path, data=data, headers=headers)
        with urllib.request.urlopen(req, timeout=5) as response:
            return json.load(response)

    def poll(path, predicate, description):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("server exited unexpectedly; see server log below")
            try:
                result = request(path)
                if predicate(result):
                    return result
                if isinstance(result, dict) and result.get("status") == "error":
                    raise RuntimeError(result.get("error"))
            except (urllib.error.URLError, TimeoutError):
                pass
            time.sleep(0.1)
        raise RuntimeError(f"timed out waiting for {description}")

    def stop():
        if process is not None and process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()

    def event(attempt, conclusion):
        return {"action": "completed", "repository": {"full_name": "acme/widgets"},
                "workflow_run": {"id": 42, "run_attempt": attempt, "head_sha": "abc123",
                                 "conclusion": conclusion}}

    with tempfile.TemporaryDirectory(prefix="gdk-ci-demo-") as directory:
        log_path = Path(directory) / "server.log"
        with log_path.open("w+") as log:
            env = dict(os.environ, APP_MODE="demo", DATABASE_PATH=str(Path(directory) / "state.db"),
                       BIND_ADDR=f"127.0.0.1:{port}")

            def start():
                return subprocess.Popen([str(binary)], cwd=ROOT, env=env,
                                        stdout=log, stderr=subprocess.STDOUT)

            try:
                process = start()
                poll("/health", lambda body: body.get("ok"), "server startup")
                print("1. Sending a failed CI webhook (attempt 1).", flush=True)
                request("/webhooks/github", event(1, "failure"))
                path = f"/sessions/{SESSION_ID}"
                session = poll(path, lambda body: body["status"] == "waiting_ci", "agent yield")
                assert session["invocations"] == 1
                assert session["pending"]["tool_call_id"] == "demo-rerun"
                print("   Agent yielded. The rerun tool call is persisted, but unanswered.", flush=True)
                print("2. Stopping the server. No agent or waiting task survives.", flush=True)
                stop()
                print("3. Restarting with the same SQLite database.", flush=True)
                process = start()
                poll("/health", lambda body: body.get("ok"), "server restart")
                session = request(path)
                assert session["status"] == "waiting_ci"
                assert session["invocations"] == 1
                print("4. Sending a successful CI webhook (attempt 2).", flush=True)
                request("/webhooks/github", event(2, "success"))
                session = poll(path, lambda body: body["status"] == "complete", "agent resume")
                assert session["invocations"] == 2
                assert session["pending"] is None
                print("\nPersisted execution trace:")
                for line in session["trace"]:
                    print(f"  {line}")
                print("\nFinal report:")
                for content in session["conversation"][-1]["content"]:
                    if content.get("type") == "text":
                        print(content["text"])
                print("\nPASS: two fresh agent invocations, one conversation, one external wait.")
            except BaseException:
                stop()
                log.flush()
                print("\nServer log:\n" + log_path.read_text())
                raise
            finally:
                stop()


if __name__ == "__main__":
    main()
