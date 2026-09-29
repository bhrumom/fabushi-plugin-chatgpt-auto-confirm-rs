#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlparse
from urllib.request import urlopen

SCENARIOS = {
    "target_crash_recovery",
    "browser_crash_recovery",
    "message_confirmation_timeout",
    "continuation",
    "disconnection",
    "rate_limit",
    "conversation_too_long",
}
NATURAL_SCENARIOS = {"rate_limit", "conversation_too_long"}
SENSITIVE_KEYS = {
    "assistant_text",
    "prompt",
    "original_prompt",
    "acceptance_prompt",
    "composer_text",
    "visible_progress_messages",
    "progress_messages",
    "interrupted_turn_visible_content",
}


class NotConfigured(RuntimeError):
    def __init__(
        self,
        reason,
        *,
        observations=None,
        artifact_files=None,
        conversation_url=None,
        real_chatgpt=True,
    ):
        super().__init__(reason)
        self.reason = reason
        self.observations = observations or []
        self.artifact_files = artifact_files or []
        self.conversation_url = conversation_url
        self.real_chatgpt = real_chatgpt


class ScenarioFailed(RuntimeError):
    def __init__(self, reason, *, observations=None, artifact_files=None):
        super().__init__(reason)
        self.reason = reason
        self.observations = observations or []
        self.artifact_files = artifact_files or []


def now_iso():
    return datetime.now(timezone.utc).isoformat()


def sha256_text(value):
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path, payload):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(payload, ensure_ascii=False, sort_keys=True, indent=2) + "\n",
        encoding="utf-8",
    )


def canonical_chatgpt_url(value):
    if not isinstance(value, str) or not value:
        return False
    parsed = urlparse(value)
    if parsed.scheme != "https" or (parsed.hostname or "") not in {
        "chatgpt.com",
        "chat.openai.com",
    }:
        return False
    parts = [part for part in parsed.path.split("/") if part]
    if len(parts) < 2 or parts[0] != "c":
        return False
    return bool(parts[1]) and all(ch.isalnum() or ch in "-_" for ch in parts[1])


def safe_argv(argv):
    redact_next = False
    result = []
    for value in argv:
        if redact_next:
            result.append(f"<sha256:{sha256_text(value)}>")
            redact_next = False
            continue
        result.append(str(value))
        if value in {"--prompt", "--acceptance-prompt"}:
            redact_next = True
    return result


def sanitize(value, key=""):
    if key in SENSITIVE_KEYS:
        if isinstance(value, str):
            return {"sha256": sha256_text(value), "length": len(value)}
        if isinstance(value, list):
            return {"count": len(value), "sha256": sha256_text(json.dumps(value, ensure_ascii=False))}
        return "<redacted>"
    if isinstance(value, dict):
        return {str(k): sanitize(v, str(k)) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitize(item) for item in value]
    return value


def command_json(evidence_dir, label, argv, timeout=300):
    evidence_dir = Path(evidence_dir)
    started = now_iso()
    try:
        result = subprocess.run(
            argv,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as error:
        meta = {
            "started_at": started,
            "argv": safe_argv(argv),
            "timeout_seconds": timeout,
            "timed_out": True,
        }
        write_json(evidence_dir / f"{label}.command.json", meta)
        raise ScenarioFailed(f"{label} timed out after {timeout}s", artifact_files=[f"{label}.command.json"]) from error

    meta = {
        "started_at": started,
        "finished_at": now_iso(),
        "argv": safe_argv(argv),
        "returncode": result.returncode,
        "stdout_sha256": sha256_text(result.stdout),
        "stderr_tail": result.stderr[-4000:],
    }
    write_json(evidence_dir / f"{label}.command.json", meta)
    artifacts = [f"{label}.command.json"]
    if result.returncode != 0:
        raise ScenarioFailed(
            f"{label} failed with exit {result.returncode}: {result.stderr[-1200:]}",
            artifact_files=artifacts,
        )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ScenarioFailed(
            f"{label} did not return exactly one JSON document",
            artifact_files=artifacts,
        ) from error
    write_json(evidence_dir / f"{label}.json", sanitize(payload))
    artifacts.append(f"{label}.json")
    return payload, artifacts


def raw_json(argv, timeout=30):
    result = subprocess.run(
        argv,
        text=True,
        capture_output=True,
        check=False,
        timeout=timeout,
    )
    if result.returncode != 0:
        raise ScenarioFailed(f"command failed: {safe_argv(argv)}")
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ScenarioFailed("shipping command did not return JSON") from error


def finish_process(evidence_dir, label, process, timeout=300):
    started = getattr(process, "_fabushi_started_at", now_iso())
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate()
        meta = {
            "started_at": started,
            "finished_at": now_iso(),
            "argv": safe_argv(process.args),
            "timed_out": True,
            "stdout_sha256": sha256_text(stdout),
            "stderr_tail": stderr[-4000:],
        }
        write_json(Path(evidence_dir) / f"{label}.command.json", meta)
        raise ScenarioFailed(
            f"{label} timed out after {timeout}s",
            artifact_files=[f"{label}.command.json"],
        )

    meta = {
        "started_at": started,
        "finished_at": now_iso(),
        "argv": safe_argv(process.args),
        "returncode": process.returncode,
        "stdout_sha256": sha256_text(stdout),
        "stderr_tail": stderr[-4000:],
    }
    write_json(Path(evidence_dir) / f"{label}.command.json", meta)
    artifacts = [f"{label}.command.json"]
    if process.returncode != 0:
        raise ScenarioFailed(
            f"{label} failed with exit {process.returncode}: {stderr[-1200:]}",
            artifact_files=artifacts,
        )
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise ScenarioFailed(
            f"{label} did not return exactly one JSON document",
            artifact_files=artifacts,
        ) from error
    write_json(Path(evidence_dir) / f"{label}.json", sanitize(payload))
    artifacts.append(f"{label}.json")
    return payload, artifacts


def shipping_base(args):
    return [args.binary, "--cdp", args.cdp]


def preflight(args, evidence_dir, label="preflight"):
    payload, artifacts = command_json(
        evidence_dir, label, shipping_base(args) + ["status"], timeout=60
    )
    if payload.get("authentication_required"):
        raise NotConfigured(
            "authenticated ChatGPT session is not available",
            observations=["shipping status reports authentication_required=true"],
            artifact_files=artifacts,
            real_chatgpt=False,
        )
    if not payload.get("conversation_loaded") or not payload.get("composer_ready"):
        raise NotConfigured(
            "authenticated ChatGPT UI is not ready",
            observations=[
                f"conversation_loaded={payload.get('conversation_loaded')}",
                f"composer_ready={payload.get('composer_ready')}",
            ],
            artifact_files=artifacts,
            conversation_url=payload.get("url"),
        )
    return payload, artifacts


def cdp_base_url(value):
    parsed = urlparse(value)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise ScenarioFailed("CDP endpoint must be an http(s) URL")
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    return f"{parsed.scheme}://{parsed.hostname}:{port}", port


def cdp_targets(cdp):
    base, _ = cdp_base_url(cdp)
    with urlopen(f"{base}/json/list", timeout=5) as response:
        return json.loads(response.read().decode("utf-8"))


def close_target(cdp, target_id):
    base, _ = cdp_base_url(cdp)
    with urlopen(f"{base}/json/close/{target_id}", timeout=5) as response:
        response.read()


def wait_new_canonical_target(cdp, baseline_ids, process, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ScenarioFailed("shipping process exited before a faultable canonical target appeared")
        try:
            for target in cdp_targets(cdp):
                target_id = str(target.get("id") or "")
                url = target.get("url") or ""
                if (
                    target_id
                    and target_id not in baseline_ids
                    and target.get("type") == "page"
                    and canonical_chatgpt_url(url)
                ):
                    return target
        except Exception:
            pass
        time.sleep(0.1)
    raise ScenarioFailed("timed out waiting for the shipping RunWorker canonical target")


def queue_report_prompt(task_id, marker):
    report = {
        "protocol": "mahayana.task-report.v1",
        "task_id": task_id,
        "applied_task_revision": 1,
        "applied_spec_digest": "",
        "status": "complete",
        "all_tasks_complete": True,
        "summary": marker,
        "completed": [marker],
        "remaining": [],
        "blockers": [],
        "verification": ["real authenticated Gate D scenario"],
        "next_task": "",
    }
    block = (
        "MAHAYANA_TASK_REPORT_V1_BEGIN\n"
        + json.dumps(report, ensure_ascii=False, separators=(",", ":"))
        + "\nMAHAYANA_TASK_REPORT_V1_END"
    )
    return (
        "这是真实 ChatGPT Gate D 故障恢复场景。请先输出 12 个简短编号段落，"
        f"最后原样包含标记 {marker}，随后严格输出下面报告块并结束：\n{block}"
    )


def enqueue_fault_task(args, evidence_dir, db_path, task_id, marker):
    prompt = queue_report_prompt(task_id, marker)
    return command_json(
        evidence_dir,
        "queue-enqueue",
        [
            args.binary,
            "--cdp",
            args.cdp,
            "--db",
            str(db_path),
            "queue-enqueue",
            "--task-id",
            task_id,
            "--account-id",
            task_id,
            "--prompt",
            prompt,
            "--model",
            args.model,
            "--thinking",
            args.thinking,
            "--conversation-kind",
            "work",
            "--known-exact-head",
            args.commit,
            "--ci-evidence",
            f"github-actions:{args.workflow_run_id}",
            "--current-stage",
            f"gate-d-{args.scenario}",
            "--pending-work",
            f"prove {args.scenario} through the shipping runtime",
        ],
        timeout=60,
    )


def spawn_queue_once(args, db_path, task_id, *, managed=None):
    argv = [
        args.binary,
        "--cdp",
        args.cdp,
        "--db",
        str(db_path),
        "queue-run-once",
        "--account-id",
        task_id,
    ]
    if managed:
        argv += [
            "--manage-browser",
            "true",
            "--browser-binary",
            managed["binary"],
            "--profile",
            managed["profile"],
            "--port",
            str(managed["port"]),
            "--headed",
            "false",
        ]
    process = subprocess.Popen(argv, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    process._fabushi_started_at = now_iso()
    return process


def queue_snapshot(args, evidence_dir, db_path, label="queue-status"):
    return command_json(
        evidence_dir,
        label,
        [args.binary, "--cdp", args.cdp, "--db", str(db_path), "queue-status"],
        timeout=60,
    )


def latest_run(snapshot, task_id):
    runs = [run for run in snapshot.get("runs", []) if run.get("task_id") == task_id]
    if not runs:
        return None
    runs.sort(key=lambda run: run.get("started_at_ms", 0))
    return runs[-1]


def task_by_id(snapshot, task_id):
    return next((task for task in snapshot.get("tasks", []) if task.get("id") == task_id), None)


def checkpoint_counter(run, name):
    checkpoint = (run or {}).get("checkpoint") or {}
    counters = checkpoint.get("counters") or {}
    return int(counters.get(name) or 0)


def canonical_from_report(report):
    url = (report or {}).get("conversation_url")
    return url if canonical_chatgpt_url(url) else None


def discover_browser_process(cdp):
    _, port = cdp_base_url(cdp)
    wanted = f"--remote-debugging-port={port}"
    candidates = []
    proc_root = Path("/proc")
    if not proc_root.is_dir():
        return None
    for item in proc_root.iterdir():
        if not item.name.isdigit():
            continue
        try:
            raw = (item / "cmdline").read_bytes()
            argv = [part.decode("utf-8", "replace") for part in raw.split(b"\0") if part]
        except (OSError, PermissionError):
            continue
        if not argv:
            continue
        has_port = wanted in argv
        if not has_port:
            for index, value in enumerate(argv[:-1]):
                if value == "--remote-debugging-port" and argv[index + 1] == str(port):
                    has_port = True
                    break
        if not has_port:
            continue
        profile = None
        for index, value in enumerate(argv):
            if value.startswith("--user-data-dir="):
                profile = value.split("=", 1)[1]
                break
            if value == "--user-data-dir" and index + 1 < len(argv):
                profile = argv[index + 1]
                break
        if not profile:
            continue
        try:
            binary = os.readlink(item / "exe")
        except OSError:
            binary = argv[0]
        base = os.path.basename(binary).lower()
        if "chrome" not in base and "chromium" not in base:
            continue
        candidates.append(
            {"pid": int(item.name), "binary": binary, "profile": profile, "port": port, "argv": argv}
        )
    if not candidates:
        return None
    candidates.sort(key=lambda item: item["pid"])
    return candidates[0]


def run_target_crash(args, evidence_dir):
    preflight_status, artifacts = preflight(args, evidence_dir)
    baseline = {str(item.get("id")) for item in cdp_targets(args.cdp)}
    task_id = f"gate-d-target-{args.commit[:10]}-{int(time.time())}"
    db_path = Path(evidence_dir) / "target-crash.sqlite3"
    _, added = enqueue_fault_task(args, evidence_dir, db_path, task_id, "GATE_D_TARGET_CRASH_RECOVERED")
    artifacts += added
    process = spawn_queue_once(args, db_path, task_id)
    target = wait_new_canonical_target(args.cdp, baseline, process)
    closed_id = str(target.get("id"))
    closed_url = target.get("url")
    close_target(args.cdp, closed_id)
    report, run_artifacts = finish_process(evidence_dir, "target-crash-run", process, timeout=300)
    artifacts += run_artifacts
    snapshot, status_artifacts = queue_snapshot(args, evidence_dir, db_path, "target-crash-status")
    artifacts += status_artifacts
    run = latest_run(snapshot, task_id)
    recoveries = checkpoint_counter(run, "target_recoveries")
    url = canonical_from_report(report) or (run or {}).get("canonical_conversation_url")
    if recoveries < 1:
        raise ScenarioFailed(
            "shipping RunWorker did not record a target recovery after the real CDP target was closed",
            observations=[f"closed_target_id={closed_id}", f"target_recoveries={recoveries}"],
            artifact_files=artifacts,
        )
    if not canonical_chatgpt_url(url):
        raise ScenarioFailed("target recovery did not finish on a canonical ChatGPT conversation", artifact_files=artifacts)
    return {
        "conversation_url": url,
        "observations": [
            f"shipping RunWorker created canonical target {closed_id}",
            "repository driver closed that real CDP target through /json/close",
            f"durable checkpoint target_recoveries={recoveries}",
            f"run_state={report.get('state')}",
            f"preflight_model={preflight_status.get('observed_model')}",
        ],
        "fault_injection": "cdp-target-close",
        "artifact_files": artifacts,
    }


def run_browser_crash(args, evidence_dir):
    if not args.allow_browser_crash:
        raise NotConfigured(
            "browser crash is opt-in; dispatch workflow with allow_browser_crash=true",
            observations=["repository driver is present but destructive browser kill was not authorized"],
        )
    _, artifacts = preflight(args, evidence_dir)
    managed = discover_browser_process(args.cdp)
    if not managed:
        raise NotConfigured(
            "could not discover the authenticated Chromium process/profile from the CDP port",
            observations=["a Chromium process with --remote-debugging-port and --user-data-dir is required"],
            artifact_files=artifacts,
        )
    if managed["pid"] == os.getpid():
        raise ScenarioFailed("refusing to terminate the scenario driver process")
    baseline = {str(item.get("id")) for item in cdp_targets(args.cdp)}
    task_id = f"gate-d-browser-{args.commit[:10]}-{int(time.time())}"
    db_path = Path(evidence_dir) / "browser-crash.sqlite3"
    _, added = enqueue_fault_task(args, evidence_dir, db_path, task_id, "GATE_D_BROWSER_CRASH_RECOVERED")
    artifacts += added
    process = spawn_queue_once(args, db_path, task_id, managed=managed)
    target = wait_new_canonical_target(args.cdp, baseline, process)
    process_record = {
        "killed_pid": managed["pid"],
        "browser_binary": managed["binary"],
        "profile_sha256": sha256_text(managed["profile"]),
        "port": managed["port"],
        "target_id_before_kill": target.get("id"),
        "conversation_url_before_kill": target.get("url"),
    }
    write_json(Path(evidence_dir) / "browser-crash-process.json", process_record)
    artifacts.append("browser-crash-process.json")
    os.kill(managed["pid"], signal.SIGKILL)

    replacement = None
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline and process.poll() is None:
        try:
            candidate = discover_browser_process(args.cdp)
            if candidate and candidate["pid"] != managed["pid"]:
                replacement = candidate
                break
        except Exception:
            pass
        time.sleep(0.25)

    report, run_artifacts = finish_process(evidence_dir, "browser-crash-run", process, timeout=360)
    artifacts += run_artifacts
    snapshot, status_artifacts = queue_snapshot(args, evidence_dir, db_path, "browser-crash-status")
    artifacts += status_artifacts
    run = latest_run(snapshot, task_id)
    recoveries = checkpoint_counter(run, "browser_recoveries")
    url = canonical_from_report(report) or (run or {}).get("canonical_conversation_url")
    if recoveries < 1:
        raise ScenarioFailed(
            "shipping AccountBrowserActor did not record browser recovery after SIGKILL",
            observations=[f"killed_pid={managed['pid']}", f"browser_recoveries={recoveries}"],
            artifact_files=artifacts,
        )
    if not canonical_chatgpt_url(url):
        raise ScenarioFailed("browser recovery did not finish on a canonical ChatGPT conversation", artifact_files=artifacts)
    return {
        "conversation_url": url,
        "observations": [
            f"repository driver SIGKILLed authenticated Chromium pid {managed['pid']}",
            f"replacement_browser_pid={(replacement or {}).get('pid')}",
            f"durable checkpoint browser_recoveries={recoveries}",
            f"run_state={report.get('state')}",
        ],
        "fault_injection": "host-browser-sigkill",
        "artifact_files": artifacts,
    }


def default_route_interface():
    path = Path("/proc/net/route")
    if not path.is_file():
        return None
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines()[1:]:
        fields = line.split()
        if len(fields) >= 4 and fields[1] == "00000000" and int(fields[3], 16) & 0x2:
            return fields[0]
    return None


class NetworkFault:
    def __init__(self, allowed, interface):
        self.allowed = allowed
        self.interface = interface or default_route_interface()
        self.ip = shutil.which("ip")
        self.sudo = shutil.which("sudo")

    def command(self, state):
        if not self.allowed:
            raise NotConfigured(
                "network fault scenarios are opt-in; dispatch workflow with allow_network_faults=true"
            )
        if not self.interface or self.interface == "lo":
            raise NotConfigured("no non-loopback default network interface is available")
        if not Path(f"/sys/class/net/{self.interface}").exists():
            raise NotConfigured(f"network interface does not exist: {self.interface}")
        if not self.ip:
            raise NotConfigured("iproute2 'ip' command is required for repository-owned network faults")
        argv = [self.ip, "link", "set", "dev", self.interface, state]
        if os.geteuid() != 0:
            if not self.sudo:
                raise NotConfigured("root or passwordless sudo is required for network fault scenarios")
            argv = [self.sudo, "-n"] + argv
        return argv

    def set_state(self, state):
        argv = self.command(state)
        result = subprocess.run(argv, text=True, capture_output=True, check=False, timeout=20)
        if result.returncode != 0:
            raise NotConfigured(
                f"host network control is not configured for {self.interface}: {result.stderr[-600:]}"
            )


def response_in_flight(snapshot):
    return any(
        bool(snapshot.get(name))
        for name in ("stop_available", "assistant_streaming", "waiting_for_approval", "awaiting_assistant")
    )


def raw_status(args):
    return raw_json(shipping_base(args) + ["status"], timeout=20)


def send_argv(args, prompt, *, dispatch=5, stale=300, continuation=300, rate_pause=1, timeout=240):
    return shipping_base(args) + [
        "send",
        "--prompt",
        prompt,
        "--model",
        args.model,
        "--thinking",
        args.thinking,
        "--timeout-seconds",
        str(timeout),
        "--poll-ms",
        "200",
        "--stale-reload-seconds",
        str(stale),
        "--rate-limit-pause-seconds",
        str(rate_pause),
        "--dispatch-confirm-seconds",
        str(dispatch),
        "--continuation-seconds",
        str(continuation),
    ]


def wait_user_turn(args, baseline, process, timeout=30):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            return last
        try:
            last = raw_status(args)
            if int(last.get("user_turns") or 0) > int(baseline):
                return last
        except Exception:
            pass
        time.sleep(0.15)
    return last


def network_fault_scenario(args, evidence_dir, kind):
    baseline, artifacts = preflight(args, evidence_dir)
    controller = NetworkFault(args.allow_network_faults, args.network_interface)
    marker = f"GATE_D_{kind.upper()}_{args.commit[:10]}"
    prompt = (
        f"这是真实网络故障 Gate D 场景。最终恢复后请只用简短回复包含标记 {marker}。"
    )
    if kind == "message_confirmation_timeout":
        argv = send_argv(
            args,
            prompt,
            dispatch=2,
            stale=300,
            continuation=300,
            timeout=max(120, int(args.fault_window_seconds) + 90),
        )
        controller.set_state("down")
        process = subprocess.Popen(argv, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        process._fabushi_started_at = now_iso()
        observed = []
        try:
            deadline = time.monotonic() + max(3, args.fault_window_seconds)
            while time.monotonic() < deadline and process.poll() is None:
                try:
                    snap = raw_status(args)
                    observed.append(
                        {
                            "user_turns": snap.get("user_turns"),
                            "connection_interrupted": snap.get("connection_interrupted"),
                            "url": snap.get("url"),
                        }
                    )
                except Exception:
                    pass
                time.sleep(0.4)
        finally:
            controller.set_state("up")
        write_json(Path(evidence_dir) / "network-observation.json", sanitize(observed))
        artifacts.append("network-observation.json")
        report, run_artifacts = finish_process(evidence_dir, "message-confirmation-run", process, timeout=300)
        artifacts += run_artifacts
        retries = int(report.get("dispatch_retries") or 0)
        url = canonical_from_report(report)
        if retries < 1:
            raise ScenarioFailed(
                "real network outage did not drive the shipping dispatch-confirmation retry path",
                observations=[f"dispatch_retries={retries}"],
                artifact_files=artifacts,
            )
        if not url:
            raise ScenarioFailed("message-confirmation recovery lacks canonical conversation URL", artifact_files=artifacts)
        return {
            "conversation_url": url,
            "observations": [
                f"host interface {controller.interface} was taken down before dispatch confirmation",
                f"dispatch_retries={retries}",
                f"final_state={report.get('state')}",
            ],
            "fault_injection": f"host-network-interface:{controller.interface}",
            "artifact_files": artifacts,
        }

    continuation_seconds = 2 if kind == "continuation" else 300
    stale_seconds = 300 if kind == "continuation" else 3
    argv = send_argv(
        args,
        prompt,
        dispatch=5,
        stale=stale_seconds,
        continuation=continuation_seconds,
        timeout=max(180, int(args.fault_window_seconds) + 120),
    )
    process = subprocess.Popen(argv, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    process._fabushi_started_at = now_iso()
    first = wait_user_turn(args, baseline.get("user_turns", 0), process, timeout=30)
    if process.poll() is not None:
        report, run_artifacts = finish_process(evidence_dir, f"{kind}-run", process, timeout=5)
        artifacts += run_artifacts
        raise ScenarioFailed(
            f"{kind} shipping run settled before network fault could be injected",
            observations=[f"state={report.get('state')}"],
            artifact_files=artifacts,
        )
    if not first or int(first.get("user_turns") or 0) <= int(baseline.get("user_turns") or 0):
        process.terminate()
        try:
            process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
        raise ScenarioFailed(
            f"{kind} could not confirm the initial real user turn before fault injection",
            artifact_files=artifacts,
        )

    controller.set_state("down")
    observations = []
    disconnected = False
    idle_seen = False
    try:
        deadline = time.monotonic() + max(3, args.fault_window_seconds)
        while time.monotonic() < deadline and process.poll() is None:
            try:
                snap = raw_status(args)
                disconnected = disconnected or bool(snap.get("connection_interrupted"))
                idle_seen = idle_seen or not response_in_flight(snap)
                observations.append(
                    {
                        "connection_interrupted": snap.get("connection_interrupted"),
                        "assistant_streaming": snap.get("assistant_streaming"),
                        "awaiting_assistant": snap.get("awaiting_assistant"),
                        "url": snap.get("url"),
                    }
                )
            except Exception:
                pass
            time.sleep(0.4)
    finally:
        controller.set_state("up")

    write_json(Path(evidence_dir) / "network-observation.json", sanitize(observations))
    artifacts.append("network-observation.json")
    report, run_artifacts = finish_process(evidence_dir, f"{kind}-run", process, timeout=360)
    artifacts += run_artifacts
    url = canonical_from_report(report)
    if not url:
        raise ScenarioFailed(f"{kind} recovery lacks canonical conversation URL", artifact_files=artifacts)
    if kind == "continuation":
        count = int(report.get("continuations") or 0)
        if count < 1:
            raise ScenarioFailed(
                "real interrupted response did not drive the shipping continuation path",
                observations=[
                    f"connection_interrupted_observed={disconnected}",
                    f"idle_snapshot_observed={idle_seen}",
                    f"continuations={count}",
                ],
                artifact_files=artifacts,
            )
        observations_text = [
            f"host interface {controller.interface} was interrupted after initial user-turn confirmation",
            f"connection_interrupted_observed={disconnected}",
            f"idle_snapshot_observed={idle_seen}",
            f"continuations={count}",
            f"final_state={report.get('state')}",
        ]
    else:
        if not disconnected:
            raise ScenarioFailed(
                "host network was interrupted but shipping semantic observation never reported connection_interrupted",
                observations=[f"interface={controller.interface}", f"final_state={report.get('state')}"],
                artifact_files=artifacts,
            )
        observations_text = [
            f"host interface {controller.interface} was interrupted after initial user-turn confirmation",
            "shipping status observed connection_interrupted=true on the real ChatGPT page",
            f"final_state_after_network_restore={report.get('state')}",
            f"recoveries={report.get('recoveries')}",
        ]
    return {
        "conversation_url": url,
        "observations": observations_text,
        "fault_injection": f"host-network-interface:{controller.interface}",
        "artifact_files": artifacts,
    }


def natural_scenario(args, evidence_dir, kind):
    status, artifacts = preflight(args, evidence_dir, label=f"{kind}-preflight")
    flag = (
        bool(status.get("rate_limit_notice")) and bool(status.get("rate_limit_dialog_visible"))
        if kind == "rate_limit"
        else bool(status.get("conversation_too_long"))
    )
    natural_record = {
        "scenario": kind,
        "observed_at": now_iso(),
        "url": status.get("url"),
        "rate_limit_notice": status.get("rate_limit_notice"),
        "rate_limit_dialog_visible": status.get("rate_limit_dialog_visible"),
        "conversation_too_long": status.get("conversation_too_long"),
        "synthetic_ui": False,
    }
    write_json(Path(evidence_dir) / "natural-observation.json", natural_record)
    artifacts.append("natural-observation.json")
    if not flag:
        raise NotConfigured(
            f"real ChatGPT did not naturally present {kind} during this run",
            observations=[f"natural_condition_observed=false for {kind}"],
            artifact_files=artifacts,
            conversation_url=status.get("url"),
        )

    marker = f"GATE_D_{kind.upper()}_{args.commit[:10]}"
    prompt = f"这是 Gate D 自然状态验收。若页面允许继续，最终简短回复并包含 {marker}。"
    report, run_artifacts = command_json(
        evidence_dir,
        f"{kind}-shipping-run",
        send_argv(
            args,
            prompt,
            dispatch=5,
            stale=30,
            continuation=30,
            rate_pause=1,
            timeout=180,
        ),
        timeout=240,
    )
    artifacts += run_artifacts
    url = canonical_from_report(report) or (
        status.get("url") if canonical_chatgpt_url(status.get("url")) else None
    )
    if kind == "rate_limit":
        pauses = int(report.get("rate_limit_pauses") or 0)
        threshold_recovery = report.get("state") == "recovering" and report.get("message") == "rate_limit_threshold_exceeded"
        if pauses < 1 and not threshold_recovery:
            raise ScenarioFailed(
                "natural rate-limit state was visible but shipping handling did not record a pause/recovery",
                observations=[f"rate_limit_pauses={pauses}", f"message={report.get('message')}"],
                artifact_files=artifacts,
            )
        observations = [
            "current visible real ChatGPT rate-limit dialog was naturally observed",
            f"rate_limit_pauses={pauses}",
            f"threshold_fresh_conversation_requested={threshold_recovery}",
        ]
    else:
        if report.get("state") != "recovering" or report.get("message") != "conversation_too_long":
            raise ScenarioFailed(
                "natural conversation-too-long state was visible but shipping recovery semantics were not observed",
                observations=[f"state={report.get('state')}", f"message={report.get('message')}"],
                artifact_files=artifacts,
            )
        observations = [
            "current real ChatGPT conversation-too-long state was naturally observed",
            "shipping application returned Recovering/conversation_too_long for fresh-conversation handoff",
        ]
    if not url:
        raise ScenarioFailed(f"{kind} evidence lacks canonical ChatGPT conversation URL", artifact_files=artifacts)
    return {
        "conversation_url": url,
        "observations": observations,
        "fault_injection": "none",
        "natural_condition": True,
        "artifact_files": artifacts,
    }


def run_scenario(args, evidence_dir):
    if args.scenario == "target_crash_recovery":
        return run_target_crash(args, evidence_dir)
    if args.scenario == "browser_crash_recovery":
        return run_browser_crash(args, evidence_dir)
    if args.scenario in {"message_confirmation_timeout", "continuation", "disconnection"}:
        return network_fault_scenario(args, evidence_dir, args.scenario)
    if args.scenario in NATURAL_SCENARIOS:
        return natural_scenario(args, evidence_dir, args.scenario)
    raise ScenarioFailed(f"unsupported scenario: {args.scenario}")


def self_test():
    assert canonical_chatgpt_url("https://chatgpt.com/c/abc-123")
    assert canonical_chatgpt_url("https://chat.openai.com/c/abc_def?x=1")
    assert not canonical_chatgpt_url("https://example.com/c/abc")
    assert safe_argv(["send", "--prompt", "secret"])[-1].startswith("<sha256:")
    assert not response_in_flight(
        {
            "stop_available": False,
            "assistant_streaming": False,
            "waiting_for_approval": False,
            "awaiting_assistant": False,
        }
    )
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "obs.json"
        write_json(path, {"rate_limit_notice": False, "synthetic_ui": False})
        assert sha256_file(path)
    sample = ["chromium", "--remote-debugging-port=9222", "--user-data-dir=/tmp/profile"]
    assert "--remote-debugging-port=9222" in sample
    print("real-environment-scenario-driver self-test: PASS")


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    sub = parser.add_subparsers(dest="command")
    run = sub.add_parser("run")
    run.add_argument("scenario", choices=sorted(SCENARIOS))
    run.add_argument("--binary", required=True)
    run.add_argument("--cdp", required=True)
    run.add_argument("--evidence-dir", required=True)
    run.add_argument("--commit", required=True)
    run.add_argument("--workflow-run-id", required=True)
    run.add_argument("--model", required=True)
    run.add_argument("--thinking", required=True)
    run.add_argument("--allow-browser-crash", action="store_true")
    run.add_argument("--allow-network-faults", action="store_true")
    run.add_argument("--network-interface", default="")
    run.add_argument("--fault-window-seconds", type=float, default=12.0)
    args = parser.parse_args()
    if not args.self_test and args.command != "run":
        parser.error("run or --self-test is required")
    return args


def main():
    args = parse_args()
    if args.self_test:
        self_test()
        return 0

    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    status = "passed"
    reason = None
    real_chatgpt = True
    details = {}
    try:
        details = run_scenario(args, evidence_dir)
    except NotConfigured as error:
        status = "not-configured"
        reason = error.reason
        real_chatgpt = error.real_chatgpt
        details = {
            "conversation_url": error.conversation_url,
            "observations": error.observations,
            "artifact_files": error.artifact_files,
            "fault_injection": "none",
        }
    except ScenarioFailed as error:
        status = "failed"
        reason = error.reason
        details = {
            "observations": error.observations,
            "artifact_files": error.artifact_files,
            "fault_injection": "none",
        }
    except Exception as error:
        status = "failed"
        reason = f"unexpected repository-driver error: {type(error).__name__}: {error}"
        details = {"observations": [], "artifact_files": [], "fault_injection": "none"}

    observation_path = evidence_dir / "driver-observations.json"
    observation_payload = {
        "schema": "fabushi.repository-scenario-observation.v1",
        "scenario": args.scenario,
        "status": status,
        "exact_commit": args.commit,
        "workflow_run_id": args.workflow_run_id,
        "generated_at": now_iso(),
        "platform": platform.platform(),
        "real_chatgpt": real_chatgpt,
        "synthetic_ui": False,
        "conversation_url": details.get("conversation_url"),
        "observations": details.get("observations") or [],
        "fault_injection": details.get("fault_injection") or "none",
        "natural_condition": details.get("natural_condition"),
        "reason": reason,
    }
    write_json(observation_path, observation_payload)
    artifacts = list(dict.fromkeys((details.get("artifact_files") or []) + ["driver-observations.json"]))

    payload = {
        "schema": "fabushi.authenticated-external-scenario.v1",
        "scenario": args.scenario,
        "status": status,
        "exact_commit": args.commit,
        "workflow_run_id": args.workflow_run_id,
        "real_chatgpt": real_chatgpt,
        "synthetic_ui": False,
        "conversation_url": details.get("conversation_url"),
        "observations": details.get("observations") or [],
        "artifact_files": artifacts,
        "fault_injection": details.get("fault_injection") or "none",
    }
    if args.scenario in NATURAL_SCENARIOS:
        payload["natural_condition"] = True if status == "passed" else False
    if reason:
        payload["reason"] = reason
    print(json.dumps(payload, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
