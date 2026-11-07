#!/usr/bin/env python3
"""Real-model native TUI wakeup demonstration; no continuation input injection."""

import argparse
from datetime import datetime, timezone
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import uuid

SCENARIO = "real-model-autonomous-same-conversation"


def utc():
    return datetime.now(timezone.utc).isoformat()


def binding(path):
    # Load the authorized operator binding in a child shell. Return only paths,
    # never the shell environment or the contents of any credential.
    code = ("import os,json; print(json.dumps({k:os.environ.get(k) for k in "
            "['CODEX_WAKEUP_SMOKE_HOME','CODEX_WAKEUP_AUTH_SOURCE']}))")
    data = json.loads(subprocess.check_output(
        ["/bin/bash", "-c", 'set -a; source "$1"; python3 -c "$2"',
         "wakeup-binding", str(path), code], text=True))
    for name, value in data.items():
        if not value or not Path(value).exists():
            raise RuntimeError(f"Missing authorized runtime resource: {name}")
    return data


def stop(process):
    if process is not None and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


class Observation:
    def __init__(self, home, master, server, cli, evidence):
        self.home, self.master, self.server, self.cli = home, master, server, cli
        self.evidence = evidence
        self.inputs = []
        self.terminal = bytearray()
        self.started = time.monotonic()

    def pump(self):
        ready, _, _ = select.select([self.master], [], [], 0.1)
        if ready:
            try:
                chunk = os.read(self.master, 65536)
                self.terminal.extend(chunk)
                # Emulate terminal protocol responses, never user submissions.
                for query, reply in ((b"\x1b[6n", b"\x1b[1;1R"),
                                     (b"\x1b[c", b"\x1b[?1;2c")):
                    if query in chunk:
                        self.send(reply.decode(), "terminal protocol reply")
            except OSError:
                pass

    def records(self):
        paths = list((self.home / "sessions").rglob("*.jsonl"))
        records = []
        for path in paths:
            for line in path.read_text().splitlines():
                try:
                    records.append(json.loads(line))
                except json.JSONDecodeError:
                    pass  # A concurrent append is not a complete observation yet.
        return records

    def wait(self, predicate, label, timeout=100):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump()
            if self.server.poll() is not None:
                raise RuntimeError("Owned app-server exited during " + label)
            value = predicate(self.records())
            if value:
                return value
            if self.cli.poll() is not None:
                raise RuntimeError("Candidate CLI exited during " + label)
        raise RuntimeError("Timed out observing " + label)

    def send(self, text, purpose):
        self.inputs.append({"utc": utc(), "elapsed": time.monotonic() - self.started,
                            "purpose": purpose, "text": text})
        os.write(self.master, text.encode())

    def retain(self):
        (self.evidence / "input-timeline.json").write_text(json.dumps(self.inputs, indent=2))
        (self.evidence / "terminal.ansi").write_bytes(self.terminal)
        # A semantic excerpt of this isolated synthetic conversation, without
        # account metadata, encrypted provider content or a second full archive.
        timeline = []
        for record in self.records():
            payload = dict(record.get("payload", {}))
            kind = record.get("type")
            if kind == "session_meta":
                payload = {k: payload[k] for k in (
                    "id", "session_id", "source", "thread_source", "timestamp",
                    "cwd", "cli_version", "originator") if k in payload}
            elif kind == "turn_context":
                payload = {k: payload[k] for k in (
                    "turn_id", "root_turn_id", "model", "cwd", "approval_policy",
                    "permission_profile") if k in payload}
            elif kind == "response_item":
                if payload.get("type") not in (
                    "function_call", "function_call_output", "custom_tool_call",
                    "custom_tool_call_output", "message"):
                    continue
                payload.pop("encrypted_content", None)
            elif kind != "event_msg" or payload.get("type") not in (
                "task_started", "task_complete", "user_message", "error"):
                continue
            timeline.append(dict(record, payload=payload))
        (self.evidence / "native-timeline.json").write_text(json.dumps(timeline, indent=2))


def events(records, kind):
    return [r for r in records if r.get("type") == "event_msg"
            and r.get("payload", {}).get("type") == kind]


def calls(records, name):
    return [r for r in records if r.get("type") == "response_item"
            and r.get("payload", {}).get("type") == "function_call"
            and r["payload"].get("name", "").split(".")[-1] == name]


def receipt(records, call):
    call_id = call["payload"]["call_id"]
    for r in records:
        p = r.get("payload", {})
        if (r.get("type") == "response_item" and p.get("type") == "function_call_output"
                and p.get("call_id") == call_id):
            output = p.get("output")
            if isinstance(output, str):
                return r, json.loads(output)
            if isinstance(output, list):
                return r, json.loads("".join(c.get("text", "") for c in output))
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True)
    parser.add_argument("--runtime-env", type=Path, required=True)
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    workspace = repo.parent
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
    target = Path(os.environ.get("CARGO_TARGET_DIR", repo / "codex-rs/target"))
    binary = (args.binary or target / "debug/codex").resolve()
    if not binary.is_file():
        raise RuntimeError("Build the candidate first: cargo build --locked -p codex-cli --bin codex")
    runtime = binding(args.runtime_env)
    owned = workspace / ".loop-manager/runtime"
    owned.mkdir(parents=True, exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="wakeup-", dir=owned))
    home = run / "home"
    home.mkdir()
    # An owned fixture outside repository ancestry avoids inheriting project instructions.
    socket_dir = Path(tempfile.mkdtemp(prefix="cw-", dir="/tmp")).resolve()
    fixture = socket_dir / "fixture"
    fixture.mkdir()
    source = Path(runtime["CODEX_WAKEUP_AUTH_SOURCE"])
    source_digest = hashlib.sha256(source.read_bytes()).hexdigest()
    shutil.copy2(source, home / "auth.json")
    os.chmod(home / "auth.json", 0o600)
    # Credentials stay in the owned runtime directory for operator cleanup after
    # all live reviewers finish; evidence never includes this file.
    config = (f'model = {json.dumps(args.model)}\napproval_policy = "never"\n'
              'sandbox_mode = "workspace-write"\ncheck_for_update_on_startup = false\n'
              f'[projects.{json.dumps(str(fixture))}]\ntrust_level = "trusted"\n')
    (home / "config.toml").write_text(config)
    evidence = workspace / ".loop-manager/evidence/wakeup" / revision / run.name
    evidence.mkdir(parents=True)
    # Short UDS path avoids macOS sockaddr_un path limits; directory is owned.
    endpoint = "unix://" + str(socket_dir / "server.sock")
    env = dict(os.environ, CODEX_HOME=str(home), TERM="xterm-256color", NO_COLOR="1")
    # The approved copied login is the sole auth source for this demonstration.
    for name in ("OPENAI_API_KEY", "CODEX_API_KEY", "AZURE_OPENAI_API_KEY"):
        env.pop(name, None)
    server = cli = observer = None
    master = slave = None
    server_log = (run / "server.log").open("wb")
    result = None
    try:
        server_argv = [str(binary), "app-server", "--listen", endpoint]
        server = subprocess.Popen(server_argv, cwd=fixture, env=env,
                                  stdout=server_log, stderr=server_log, start_new_session=True)
        deadline = time.monotonic() + 15
        while not (socket_dir / "server.sock").exists():
            if server.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("Owned candidate app-server failed to open its endpoint")
            time.sleep(0.05)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        cli_argv = [str(binary), "--remote", endpoint, "--no-alt-screen", "--model", args.model]
        cli = subprocess.Popen(cli_argv, cwd=fixture, env=env, stdin=slave,
                               stdout=slave, stderr=slave, start_new_session=True)
        observer = Observation(home, master, server, cli, evidence)
        token = "context-" + uuid.uuid4().hex
        output = fixture / "continued.txt"
        sentinel = fixture / "must-not-run.txt"
        prompt = (f"Remember this context token: {token}. Please use your native wakeup tool to return "
                  f"in 30000 milliseconds and write that context token as the only line of {output}. "
                  "This file write is authorized. Register now and finish this reply with REGISTERED. "
                  "Do not wait in this turn, use sleep, or launch a background process. "
                  "When the framework wakes you, write the file then finish with COMPLETED.")
        # Initial submit only. No further user input until the first causal journey ends.
        observer.wait(lambda _: "›".encode() in observer.terminal,
                      "initial composer", timeout=20)
        observer.send(prompt, "initial authorized request")
        observer.send("\r", "submit initial request")
        call = observer.wait(lambda rs: calls(rs, "schedule_wakeup"), "native registration")[0]
        registered, first = observer.wait(lambda rs: receipt(rs, call), "registration receipt")
        assert first["status"] == "scheduled" and first["task_id"]
        due = datetime.fromisoformat(first["due_at"].replace("Z", "+00:00"))
        # Native rollout timestamps have three fractional digits (recorder.rs).
        # Compare at that observable precision; native timer tests own the
        # monotonic deadline oracle without this serialization limitation.
        due_millis = due.replace(microsecond=due.microsecond // 1000 * 1000)
        assert datetime.fromisoformat(registered["timestamp"].replace("Z", "+00:00")) < due
        first_complete = observer.wait(lambda rs: events(rs, "task_complete"), "first turn completion")[0]
        assert datetime.fromisoformat(first_complete["timestamp"].replace("Z", "+00:00")) < due
        assert not output.exists(), "Follow-up output was written in the original turn"
        completed = observer.wait(
            lambda rs: rs if output.exists() and len(events(rs, "task_complete")) >= 2 else None,
            "automatic follow-up result", timeout=110)
        assert output.read_text().splitlines() == [token]
        started = events(completed, "task_started")
        assert len(started) == 2 and len(calls(completed, "schedule_wakeup")) == 1
        assert started[0]["payload"]["turn_id"] != started[1]["payload"]["turn_id"]
        assert datetime.fromisoformat(started[1]["timestamp"].replace("Z", "+00:00")) >= due_millis
        resumed_calls = [r for r in completed if r.get("type") == "response_item"
                         and r.get("payload", {}).get("type") in ("function_call", "custom_tool_call")
                         and r["timestamp"] >= started[1]["timestamp"]]
        assert resumed_calls, "No actual native tool call performed the follow-up work"
        assert len(events(completed, "user_message")) == 1
        meta = [r["payload"] for r in completed if r.get("type") == "session_meta"]
        assert len(meta) == 1 and meta[0]["source"] == "vscode" and meta[0]["thread_source"] == "user"
        contexts = [r["payload"] for r in completed if r.get("type") == "turn_context"]
        for turn in started:
            assert any(c.get("turn_id") == turn["payload"]["turn_id"]
                       and c.get("model") == args.model for c in contexts), "Actual model identity unavailable/mismatched"
        assert "Wakeup started" in observer.terminal.decode(errors="replace")
        first_journey_inputs = len(observer.inputs)
        # A distinct post-success interaction proves closing the actual TUI,
        # rather than killing an embedded server, cancels a pending reminder.
        observer.send(f"Now register a separate wakeup in 30000 milliseconds to write EXIT-FAIL to {sentinel}. "
                      "Finish this reply immediately. Do not write the file now.", "exit-tail request")
        observer.send("\r", "submit exit-tail request")
        second_call = observer.wait(lambda rs: calls(rs, "schedule_wakeup")[1:2], "exit-tail registration")[0]
        _, second = observer.wait(lambda rs: receipt(rs, second_call), "exit-tail receipt")
        observer.wait(lambda rs: len(events(rs, "task_complete")) >= 3, "exit-tail initial completion")
        second_due = datetime.fromisoformat(second["due_at"].replace("Z", "+00:00"))
        assert datetime.now(timezone.utc) < second_due
        observer.send("/quit\r", "graceful CLI exit before second deadline")
        cli.wait(timeout=15)
        while datetime.now(timezone.utc) <= second_due:
            if server.poll() is not None:
                raise RuntimeError("Server exited early; cleanup proof unavailable")
            observer.pump()
        # Allow event persistence after the deadline without injecting work.
        end = time.monotonic() + 1
        while time.monotonic() < end:
            observer.pump()
        final = observer.records()
        assert server.poll() is None and not sentinel.exists()
        assert len(events(final, "task_started")) == 3
        assert hashlib.sha256(source.read_bytes()).hexdigest() == source_digest
        manifest = {"revision": revision, "binary": str(binary),
                    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                    "requested_model": args.model,
                    "observed_models": {c["turn_id"]: c["model"] for c in contexts if c.get("turn_id")},
                    "role": "isolated-authorized-user", "fixture": str(fixture),
                    "entry": cli_argv, "server": server_argv, "viewport": [120, 40], "theme": "terminal",
                    "source": meta[0]["source"], "thread_id": meta[0]["id"],
                    "native_timestamp_precision": "milliseconds",
                    "first_task": first, "second_task": second,
                    "first_journey_input_count": first_journey_inputs, "read_back": output.read_text(),
                    "exit_tail_server_alive": True, "completed_utc": utc(),
                    "states": ["registered", "first-final", "autonomous-followup", "completed", "exit-tail"]}
        (evidence / "manifest.json").write_text(json.dumps(manifest, indent=2))
        result = {"schema": "loops.verification.result.v1", "executed": 1, "passed": 1,
                  "failed": 0, "undefined": 0, "pending": 0, "ambiguous": 0, "skipped": 0,
                  "relevantEvidence": [{"scenario": SCENARIO, "artifact": str(evidence),
                                        "taskId": first["task_id"], "threadId": meta[0]["id"]}]}
        print("Native model journey complete; inspect retained terminal and timeline:", evidence)
    finally:
        if observer:
            observer.retain()
        stop(cli); stop(server)
        if master is not None:
            os.close(master)
        if slave is not None:
            os.close(slave)
        server_log.close()
        shutil.rmtree(socket_dir)
        if result is not None:
            target_path = os.environ.get("LOOPS_VERIFICATION_RESULT_PATH")
            if target_path:
                Path(target_path).write_text(json.dumps(result, indent=2) + "\n")
        print("Owned processes cleaned up; isolated copied credentials await operator cleanup in", home)


if __name__ == "__main__":
    main()
