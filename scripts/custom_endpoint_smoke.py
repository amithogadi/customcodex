#!/usr/bin/env python3
"""Exercise a built macOS CLI against a local provider with an outbound-network audit.

Usage: python3 scripts/custom_endpoint_smoke.py codex-rs/target/release/codex
The test uses an isolated configuration and never changes the user's credentials.
The macOS interposer audits the CLI's network calls; protected system executables
may strip DYLD variables, so this is not a process-tree network sandbox.
"""

import argparse
import base64
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import platform
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import threading
import time


def output_event(item):
    return {"type": "response.output_item.done", "item": item}


def function(call_id, namespace, name, arguments):
    return output_event(
        {
            "type": "function_call",
            "call_id": call_id,
            "namespace": namespace,
            "name": name,
            "arguments": json.dumps(arguments),
        }
    )


def message(text):
    return output_event(
        {
            "type": "message",
            "role": "assistant",
            "id": "message",
            "content": [{"type": "output_text", "text": text}],
        }
    )


def tool_result_json(value):
    if isinstance(value, list):
        value = "".join(
            part.get("text", "") for part in value if isinstance(part, dict)
        )
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except json.JSONDecodeError:
            return {}
    return value if isinstance(value, dict) else {}


def started_thread_id(output):
    events = [json.loads(line) for line in output.splitlines() if line.strip()]
    ids = [
        event["thread_id"] for event in events if event.get("type") == "thread.started"
    ]
    assert len(ids) == 1, events
    return ids[0]


def check_tui_startup(binary, work, environment):
    """Open a real terminal, observe the composer, and quit without sending a turn."""
    master, slave = pty.openpty()
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    command = [
        str(binary),
        "--strict-config",
        "--no-alt-screen",
        "-a",
        "never",
        "-c",
        f'projects.{json.dumps(str(work))}.trust_level="trusted"',
    ]
    process = subprocess.Popen(
        command,
        cwd=work,
        env={**environment, "TERM": "xterm-256color"},
        stdin=slave,
        stdout=slave,
        stderr=slave,
        start_new_session=True,
    )
    os.close(slave)
    transcript = bytearray()
    pending = bytearray()
    ready = False
    deadline = time.monotonic() + 30
    try:
        while time.monotonic() < deadline:
            readable, _, _ = select.select([master], [], [], 0.1)
            if readable:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break
                if not chunk:
                    break
                transcript.extend(chunk)
                pending.extend(chunk)
                # Reply only to the cursor probe; other optional probes time out.
                while b"\x1b[6n" in pending:
                    position = pending.index(b"\x1b[6n")
                    del pending[: position + 4]
                    os.write(master, b"\x1b[1;1R")
                pending = pending[-32:]
                plain = re.sub(
                    r"\x1b\[[0-?]*[ -/]*[@-~]",
                    "",
                    transcript.decode(errors="replace"),
                )
                if (
                    not ready
                    and "mock-agent" in plain
                    and "Ask Codex to do anything" in plain
                ):
                    ready = True
                    os.write(master, b"/quit\r")
                    deadline = time.monotonic() + 10
            if process.poll() is not None:
                break
        assert ready, (
            f"TUI did not reach its composer:\n{transcript.decode(errors='replace')}"
        )
        assert process.wait(timeout=2) == 0, transcript.decode(errors="replace")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if platform.system() != "Darwin":
        parser.error(
            "the network interposer in this smoke test currently requires macOS"
        )

    with tempfile.TemporaryDirectory(prefix="custom-endpoint-smoke-") as directory:
        root = Path(directory)
        home, work = root / "config", root / "work"
        home.mkdir()
        work.mkdir()
        audit_log = root / "network.log"
        tui_audit_log = root / "tui-network.log"
        library = root / "network-audit.dylib"
        subprocess.run(
            [
                "cc",
                "-dynamiclib",
                "-Wall",
                "-Werror",
                "-o",
                str(library),
                str(Path(__file__).with_name("custom_endpoint_network_audit.c")),
            ],
            check=True,
        )
        state = {
            "requests": [],
            "authorization": [],
            "observed": [],
            "child": False,
            "child_completed": False,
            "shell": False,
            "spawn": False,
            "wait": False,
        }
        lock = threading.Lock()

        class Provider(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                with lock:
                    state["requests"].append(("GET", self.path))
                self.send_error(404)

            def do_POST(self):
                data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                items = data.get("input", [])
                outputs = {
                    item.get("call_id"): item.get("output", "")
                    for item in items
                    if item.get("type") == "function_call_output"
                }
                is_child = any(
                    item.get("type") in ("message", "agent_message")
                    and "Message Type: NEW_TASK" in json.dumps(item)
                    and "CUSTOM_ENDPOINT_CHILD" in json.dumps(item)
                    for item in items
                )
                with lock:
                    state["requests"].append(("POST", self.path))
                    state["authorization"].append(self.headers.get("Authorization"))
                    state["observed"].append(
                        {
                            "outputs": outputs,
                            "agent_messages": [
                                item
                                for item in items
                                if item.get("type") == "agent_message"
                            ],
                        }
                    )
                    if any(
                        item.get("role") == "user"
                        and "CUSTOM_ENDPOINT_RESUME" in json.dumps(item)
                        for item in items
                    ):
                        event = message("resume-ok")
                    elif is_child:
                        state["child"] = True
                        event = message("child-ok")
                    elif "wait-call" in outputs:
                        state["wait"] = (
                            tool_result_json(outputs["wait-call"]).get("timed_out")
                            is False
                        )
                        state["child_completed"] = any(
                            item.get("type") in ("message", "agent_message")
                            and "Message Type: FINAL_ANSWER" in json.dumps(item)
                            and "child-ok" in json.dumps(item)
                            for item in items
                        )
                        event = message("custom-endpoint-ok")
                    elif "spawn-call" in outputs:
                        task_name = tool_result_json(outputs["spawn-call"]).get(
                            "task_name", ""
                        )
                        state["spawn"] = task_name.endswith("/smoke_child")
                        event = function(
                            "wait-call",
                            "collaboration",
                            "wait_agent",
                            {"timeout_ms": 10000},
                        )
                    elif "shell-call" in outputs:
                        proof = work / "terminal-proof.txt"
                        state["shell"] = (
                            "terminal-ok" in str(outputs["shell-call"])
                            and proof.is_file()
                            and proof.read_text() == "terminal-ok"
                        )
                        event = function(
                            "spawn-call",
                            "collaboration",
                            "spawn_agent",
                            {
                                "task_name": "smoke_child",
                                "message": "CUSTOM_ENDPOINT_CHILD",
                                "fork_turns": "none",
                            },
                        )
                    else:
                        event = function(
                            "shell-call",
                            "functions",
                            "exec_command",
                            {
                                "cmd": "printf terminal-ok | tee terminal-proof.txt",
                                "shell": "/bin/sh",
                                "login": False,
                            },
                        )
                    response_id = f"response-{len(state['requests'])}"
                events = [
                    {"type": "response.created", "response": {"id": response_id}},
                    event,
                    {
                        "type": "response.completed",
                        "response": {
                            "id": response_id,
                            "usage": {
                                "input_tokens": 1,
                                "output_tokens": 1,
                                "total_tokens": 2,
                            },
                        },
                    },
                ]
                body = "".join(
                    f"data: {json.dumps(event)}\n\n" for event in events
                ).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        port = server.server_port
        config = f"""model = "mock-agent"
model_provider = "custom_smoke"
cli_auth_credentials_store = "file"
forced_login_method = "chatgpt"
forced_chatgpt_workspace_id = "retired-workspace"
[model_providers.custom_smoke]
name = "Local smoke provider"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"
requires_openai_auth = false
env_key = "CUSTOM_SMOKE_PROVIDER_KEY"
[features]
multi_agent_v2 = true
[analytics]
enabled = true
[feedback]
enabled = true
[otel]
exporter = {{ otlp-http = {{ endpoint = "http://127.0.0.1:{port}/retired-otel", protocol = "binary" }} }}
"""
        (home / "config.toml").write_text(config)
        payload = (
            base64.urlsafe_b64encode(
                json.dumps(
                    {
                        "email": "old@example.invalid",
                        "exp": 946684800,
                        "https://api.openai.com/auth": {
                            "chatgpt_account_id": "old-test-account",
                            "chatgpt_user_id": "old-test-user",
                            "chatgpt_plan_type": "plus",
                        },
                    }
                ).encode()
            )
            .decode()
            .rstrip("=")
        )
        old_credentials = json.dumps(
            {
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": f"eyJhbGciOiJub25lIn0.{payload}.c2ln",
                    "access_token": "expired-test-token",
                    "refresh_token": "expired-test-refresh",
                    "account_id": "old-test-account",
                },
                "last_refresh": "2000-01-01T00:00:00Z",
            }
        )
        (home / "auth.json").write_text(old_credentials)
        environment = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("CODEX_", "OPENAI_", "AWS_", "DYLD_"))
            and key not in ("ENV", "BASH_ENV", "ZDOTDIR")
            and key.lower()
            not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")
        }
        environment.update(
            {
                "CODEX_HOME": str(home),
                "DYLD_INSERT_LIBRARIES": str(library),
                "CUSTOM_ENDPOINT_NETWORK_AUDIT_LOG": str(audit_log),
                "NO_PROXY": "*",
                "CUSTOM_SMOKE_PROVIDER_KEY": "local-provider-test-key",
            }
        )
        command = [
            str(binary),
            "-a",
            "never",
            "exec",
            "--strict-config",
            "--skip-git-repo-check",
            "--json",
            "--sandbox",
            "workspace-write",
            "CUSTOM_ENDPOINT_ROOT",
        ]
        try:
            check_tui_startup(
                binary,
                work,
                {
                    **environment,
                    "CUSTOM_ENDPOINT_NETWORK_AUDIT_LOG": str(tui_audit_log),
                },
            )
            assert not state["requests"], (
                "interactive startup unexpectedly contacted the provider",
                state["requests"],
            )
            result = subprocess.run(
                command,
                cwd=work,
                env=environment,
                text=True,
                capture_output=True,
                timeout=120,
            )
            if result.returncode:
                raise RuntimeError(
                    f"CLI failed ({result.returncode}):\n{result.stderr}\n{result.stdout}"
                )
            parent_thread_id = started_thread_id(result.stdout)
            resume = subprocess.run(
                command[:-1] + ["resume", parent_thread_id, "CUSTOM_ENDPOINT_RESUME"],
                cwd=work,
                env=environment,
                text=True,
                capture_output=True,
                timeout=60,
            )
            if resume.returncode:
                raise RuntimeError(
                    f"Resume failed ({resume.returncode}):\n{resume.stderr}\n{resume.stdout}"
                )
        finally:
            server.shutdown()
        assert "custom-endpoint-ok" in result.stdout, result.stdout
        assert "resume-ok" in resume.stdout, resume.stdout
        assert started_thread_id(resume.stdout) == parent_thread_id, resume.stdout
        assert all(
            state[key] for key in ("shell", "spawn", "child", "child_completed", "wait")
        ), (json.dumps(state, indent=2), result.stdout, result.stderr)
        assert (home / "auth.json").read_text() == old_credentials, (
            "old credentials were modified"
        )
        assert state["requests"] and all(
            method == "POST" and path == "/v1/responses"
            for method, path in state["requests"]
        ), state
        assert all(
            header == "Bearer local-provider-test-key"
            for header in state["authorization"]
        ), state
        assert audit_log.exists(), "network interposer did not load"
        audit = audit_log.read_text().splitlines()
        assert any(line.startswith(("connect ", "connectx ")) for line in audit), audit
        assert not any(line.endswith(" blocked") for line in audit), audit
        assert all(
            line.startswith("dns ") or line.split()[2] == str(port) for line in audit
        ), audit
        # The TUI also connects to its own loopback task-tools MCP listener.
        # Keep those core IPC connections separate from the model request audit.
        assert tui_audit_log.exists(), "TUI network interposer did not load"
        tui_audit = tui_audit_log.read_text().splitlines()
        assert not any(line.endswith(" blocked") for line in tui_audit), tui_audit
        print(
            f"PASS: shell, subagent, resume, and completion used only the configured endpoint ({len(state['requests'])} requests)"
        )
        print(
            "PASS: expired ChatGPT credentials unchanged; retired login and telemetry settings inert"
        )
        print(
            "PASS: interactive TUI reached the configured model and composer, then exited cleanly; only loopback IPC occurred"
        )


if __name__ == "__main__":
    main()
