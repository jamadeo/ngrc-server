#!/usr/bin/env python3
"""Guide a pasted recipe across action/timer waits and two process restarts.

Only Cargo, Rust and Python's standard library are required. No accounts or keys.
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

ROOT = Path(__file__).resolve().parents[1]


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
        req = urllib.request.Request(base_url + path, data=data,
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=5) as response:
            return json.load(response)

    def poll(path, predicate, description):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError("server exited unexpectedly")
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

    def stop(hard=False):
        if process is not None and process.poll() is None:
            if hard:
                process.kill()
            else:
                process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()

    with tempfile.TemporaryDirectory(prefix="gdk-recipe-demo-") as directory:
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
                print("1. Pasting a complete recipe into the agent.", flush=True)
                example = request("/api/config")["example"]
                session = request("/api/sessions", {"recipe": example, "fast_demo": True})
                path = f"/api/sessions/{session['id']}"
                session = poll(path, lambda s: s["status"] == "waiting_action", "first step")
                assert session["invocations"] == 1
                call_id = session["pending"]["tool_call_id"]
                print("2. Killing and restarting while the agent waits for step 1 confirmation.", flush=True)
                stop(hard=True)
                process = start()
                poll("/health", lambda body: body.get("ok"), "first restart")
                assert request(path)["pending"]["tool_call_id"] == call_id
                request(path + "/action", {"tool_call_id": call_id})
                session = poll(path, lambda s: s["status"] == "waiting_action" and s["current_step"] == 1, "second step")
                print("3. Starting step 2's timer via the agent's open action call.", flush=True)
                request(path + "/action", {"tool_call_id": session["pending"]["tool_call_id"]})
                session = poll(path, lambda s: s["status"] == "waiting_timers", "timer wait")
                assert session["invocations"] == 3
                deadline_ms = max(t["deadline_ms"] for t in session["pending"]["timers"])
                print("4. Killing again; letting the timer expire while offline.", flush=True)
                stop(hard=True)
                time.sleep(max(0, deadline_ms / 1000 - time.time()) + 0.2)
                process = start()
                poll("/health", lambda body: body.get("ok"), "second restart")
                session = poll(path, lambda s: s["status"] == "waiting_action", "doneness check")
                assert session["current_step"] == 1  # timer expiry must NOT auto-advance
                print("5. Confirming the doneness check and completing the remaining steps.", flush=True)
                for _ in range(10):
                    if session["status"] == "complete":
                        break
                    assert session["status"] == "waiting_action"
                    call_id = session["pending"]["tool_call_id"]
                    request(path + "/action", {"tool_call_id": call_id})
                    session = poll(path, lambda s: s["status"] == "complete" or (
                        s["status"] == "waiting_action" and s["pending"]["tool_call_id"] != call_id), "next step")
                assert session["status"] == "complete"
                assert session["invocations"] == 7
                assert session["current_step"] == len(session["plan"]["steps"])
                assert session["pending"] is None
                print("\nPersisted execution trace:")
                for line in session["trace"]:
                    print(f"  {line}")
                print("\nCooking companion:")
                for content in session["conversation"][-1]["content"]:
                    if content.get("type") == "text":
                        print(content["text"])
                print("\nPASS: seven fresh invocations, all recipe steps confirmed, durable timers, two process restarts.")
            except BaseException:
                stop()
                log.flush()
                print("\nServer log:\n" + log_path.read_text())
                raise
            finally:
                stop()


if __name__ == "__main__":
    main()
