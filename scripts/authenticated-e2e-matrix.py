#!/usr/bin/env python3
import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from urllib.parse import urlparse

SCENARIOS = [
    "normal_send",
    "multi_turn",
    "work_completion",
    "acceptance_completion",
    "target_crash_recovery",
    "browser_crash_recovery",
    "process_restart_recovery",
    "message_confirmation_timeout",
    "continuation",
    "disconnection",
    "rate_limit",
    "conversation_too_long",
    "work_acceptance_work_switch",
    "recovery_envelope",
    "final_completion_no_duplicate_acceptance",
]

BUILTIN_SCENARIOS = {
    "normal_send",
    "multi_turn",
    "work_completion",
    "acceptance_completion",
    "process_restart_recovery",
    "work_acceptance_work_switch",
    "recovery_envelope",
    "final_completion_no_duplicate_acceptance",
}

REAL_ENVIRONMENT_SCENARIOS = [
    "target_crash_recovery",
    "message_confirmation_timeout",
    "continuation",
    "disconnection",
    "rate_limit",
    "conversation_too_long",
    "browser_crash_recovery",
]

NATURAL_UI_SCENARIOS = {"rate_limit", "conversation_too_long"}


class CommandError(RuntimeError):
    pass


def sha256_bytes(value):
    return hashlib.sha256(value).hexdigest()


def sha256_text(value):
    return sha256_bytes(value.encode("utf-8"))


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


def safe_argv(argv):
    sensitive_value_flags = {"--prompt", "--acceptance-prompt"}
    result = []
    redact_next = False
    for value in argv:
        if redact_next:
            result.append(f"<sha256:{sha256_text(value)}>")
            redact_next = False
            continue
        result.append(value)
        if value in sensitive_value_flags:
            redact_next = True
    return result


def canonical_chatgpt_url(value):
    if not isinstance(value, str) or not value:
        return False
    parsed = urlparse(value)
    if parsed.scheme != "https":
        return False
    host = parsed.hostname or ""
    if host not in {"chatgpt.com", "chat.openai.com"}:
        return False
    parts = [part for part in parsed.path.split("/") if part]
    if len(parts) < 2 or parts[0] != "c":
        return False
    conversation_id = parts[1]
    return bool(conversation_id) and all(
        ch.isalnum() or ch in "-_" for ch in conversation_id
    )


def normalized_contains(observed, requested):
    if not observed or not requested:
        return False
    observed_norm = " ".join(str(observed).split()).lower()
    requested_norm = " ".join(str(requested).split()).lower()
    return observed_norm == requested_norm or requested_norm in observed_norm


def chromium_version():
    for candidate in (
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ):
        path = shutil.which(candidate)
        if not path:
            continue
        result = subprocess.run(
            [path, "--version"],
            text=True,
            capture_output=True,
            check=False,
            timeout=20,
        )
        value = result.stdout.strip() or result.stderr.strip()
        if value:
            return value
    return ""


def run_json(label, argv, evidence_dir, timeout_seconds=7200):
    evidence_dir = Path(evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    started = datetime.now(timezone.utc).isoformat()
    try:
        result = subprocess.run(
            argv,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as error:
        meta = {
            "started_at": started,
            "argv": safe_argv(argv),
            "timeout_seconds": timeout_seconds,
            "timed_out": True,
        }
        write_json(evidence_dir / f"{label}.command.json", meta)
        raise CommandError(f"{label} timed out after {timeout_seconds}s") from error

    meta = {
        "started_at": started,
        "finished_at": datetime.now(timezone.utc).isoformat(),
        "argv": safe_argv(argv),
        "returncode": result.returncode,
        "stderr_tail": result.stderr[-12000:],
    }
    write_json(evidence_dir / f"{label}.command.json", meta)
    if result.returncode != 0:
        (evidence_dir / f"{label}.stdout.txt").write_text(
            result.stdout, encoding="utf-8"
        )
        raise CommandError(
            f"{label} failed with exit {result.returncode}: {result.stderr[-2000:]}"
        )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        (evidence_dir / f"{label}.stdout.txt").write_text(
            result.stdout, encoding="utf-8"
        )
        raise CommandError(f"{label} did not return one JSON document") from error
    write_json(evidence_dir / f"{label}.json", payload)
    return payload


def report_block(report):
    return (
        "MAHAYANA_TASK_REPORT_V1_BEGIN\n"
        + json.dumps(report, ensure_ascii=False, separators=(",", ":"))
        + "\nMAHAYANA_TASK_REPORT_V1_END"
    )


def task_report(task_id, status, all_complete, summary, completed, remaining, next_task):
    return {
        "protocol": "mahayana.task-report.v1",
        "task_id": task_id,
        "applied_task_revision": 1,
        "applied_spec_digest": "",
        "status": status,
        "all_tasks_complete": all_complete,
        "summary": summary,
        "completed": completed,
        "remaining": remaining,
        "blockers": [],
        "verification": ["Gate D authenticated live conversation"],
        "next_task": next_task,
    }


def find_task(snapshot, task_id):
    for task in snapshot.get("tasks", []):
        if task.get("id") == task_id:
            return task
    return None


def run_kinds(snapshot, task_id):
    runs = [
        run
        for run in snapshot.get("runs", [])
        if run.get("task_id") == task_id
    ]
    runs.sort(key=lambda run: run.get("started_at_ms", 0))
    return [
        ((run.get("checkpoint") or {}).get("conversation_kind") or "work")
        for run in runs
    ], runs


def scenario(status, source, evidence=None, reason=None):
    payload = {"status": status, "source": source}
    if evidence:
        payload["evidence"] = evidence
    if reason:
        payload["reason"] = reason
    return payload


def validate_external_result(
    payload, scenario_name, commit, workflow_run_id, scenario_dir
):
    if payload.get("schema") != "fabushi.authenticated-external-scenario.v1":
        raise ValueError("scenario driver returned unsupported schema")
    if payload.get("scenario") != scenario_name:
        raise ValueError("scenario driver scenario mismatch")
    if payload.get("exact_commit") != commit:
        raise ValueError("scenario driver exact_commit mismatch")
    if str(payload.get("workflow_run_id") or "") != str(workflow_run_id):
        raise ValueError("scenario driver workflow_run_id mismatch")
    status = payload.get("status")
    if status not in {"passed", "not-configured", "failed"}:
        raise ValueError("scenario driver status must be passed/not-configured/failed")
    observations = payload.get("observations")
    if not isinstance(observations, list):
        raise ValueError("scenario driver observations must be a list")
    if status != "passed":
        return {
            "status": status,
            "reason": payload.get("reason") or "scenario driver did not pass scenario",
            "observations": observations,
            "natural_condition": payload.get("natural_condition"),
        }
    if payload.get("real_chatgpt") is not True:
        raise ValueError("passed scenario must assert real_chatgpt=true")
    if payload.get("synthetic_ui") is not False:
        raise ValueError("passed scenario must assert synthetic_ui=false")
    if scenario_name in NATURAL_UI_SCENARIOS and payload.get("natural_condition") is not True:
        raise ValueError(
            f"{scenario_name} may only pass from a naturally observed real ChatGPT condition"
        )
    url = payload.get("conversation_url")
    if not canonical_chatgpt_url(url):
        raise ValueError("passed scenario lacks canonical ChatGPT conversation URL")
    if not observations:
        raise ValueError("passed scenario requires non-empty observations")

    artifact_files = payload.get("artifact_files")
    if not isinstance(artifact_files, list) or not artifact_files:
        raise ValueError("passed scenario requires at least one scenario artifact")
    artifact_hashes = {}
    root = Path(scenario_dir).resolve()
    for relative in artifact_files:
        rel = Path(relative)
        if rel.is_absolute():
            raise ValueError("scenario artifact path must be relative")
        candidate = (root / rel).resolve()
        if root != candidate and root not in candidate.parents:
            raise ValueError("scenario artifact escaped its scenario evidence directory")
        if not candidate.is_file():
            raise ValueError(f"scenario artifact is missing: {relative}")
        artifact_hashes[str(rel)] = sha256_file(candidate)

    return {
        "status": "passed",
        "conversation_url": url,
        "observations": observations,
        "fault_injection": payload.get("fault_injection") or "none",
        "natural_condition": payload.get("natural_condition"),
        "artifact_sha256": artifact_hashes,
        "real_chatgpt": True,
        "synthetic_ui": False,
        "exact_commit": commit,
        "workflow_run_id": str(workflow_run_id),
    }


def driver_command(driver_path):
    path = Path(driver_path)
    if path.suffix == ".py":
        return [sys.executable, str(path)]
    return [str(path)]


def run_real_environment_driver(args, matrix, evidence_dir):
    repository_driver = Path(__file__).with_name("real-environment-scenario-driver.py").resolve()
    override = Path(args.external_driver).resolve() if args.external_driver else None
    driver_path = override or repository_driver
    driver_source = "external-driver-override" if override else "repository-driver"

    matrix["scenario_driver"] = {
        "source": driver_source,
        "path": str(driver_path),
        "sha256": sha256_file(driver_path) if driver_path.is_file() else None,
        "repository_owned_default": override is None,
    }

    for scenario_name in REAL_ENVIRONMENT_SCENARIOS:
        if not driver_path.is_file():
            matrix["scenarios"][scenario_name] = scenario(
                "not-configured",
                driver_source,
                reason=f"scenario driver is missing: {driver_path}",
            )
            continue
        if driver_path.suffix != ".py" and not os.access(driver_path, os.X_OK):
            matrix["scenarios"][scenario_name] = scenario(
                "not-configured",
                driver_source,
                reason=f"scenario driver is not executable: {driver_path}",
            )
            continue

        scenario_dir = Path(evidence_dir) / "real-environment" / scenario_name
        scenario_dir.mkdir(parents=True, exist_ok=True)
        argv = driver_command(driver_path) + [
            "run",
            scenario_name,
            "--binary",
            args.binary,
            "--cdp",
            args.cdp,
            "--evidence-dir",
            str(scenario_dir),
            "--commit",
            args.commit,
            "--workflow-run-id",
            str(args.workflow_run_id),
            "--model",
            args.model,
            "--thinking",
            args.thinking,
            "--fault-window-seconds",
            str(args.fault_window_seconds),
        ]
        if args.allow_browser_crash:
            argv.append("--allow-browser-crash")
        if args.allow_network_faults:
            argv.append("--allow-network-faults")
        if args.network_interface:
            argv += ["--network-interface", args.network_interface]

        try:
            result = run_json(
                f"scenario-{scenario_name}",
                argv,
                evidence_dir,
                timeout_seconds=args.external_timeout_seconds,
            )
            checked = validate_external_result(
                result,
                scenario_name,
                args.commit,
                args.workflow_run_id,
                scenario_dir,
            )
            matrix["scenarios"][scenario_name] = scenario(
                checked["status"],
                driver_source,
                evidence=checked if checked["status"] == "passed" else None,
                reason=checked.get("reason"),
            )
            if checked["status"] != "passed":
                matrix["scenarios"][scenario_name]["observations"] = checked.get(
                    "observations", []
                )
                if scenario_name in NATURAL_UI_SCENARIOS:
                    matrix["scenarios"][scenario_name]["natural_condition"] = bool(
                        checked.get("natural_condition")
                    )
        except Exception as error:
            matrix["scenarios"][scenario_name] = scenario(
                "failed",
                driver_source,
                reason=str(error),
            )


def build_queue_prompts(task_id):
    recovery_complete = task_report(
        task_id,
        "complete",
        True,
        "GATE_D_RECOVERY_PASS",
        ["durable RecoveryEnvelope resumed in a new process"],
        [],
        "",
    )
    first_incomplete = task_report(
        task_id,
        "incomplete",
        False,
        "GATE_D_WORK_FIRST_PASS",
        ["first authenticated work turn completed"],
        ["resume from RecoveryEnvelope"],
        (
            "你现在处于 RecoveryEnvelope 恢复轮。先输出自然语言标记 "
            "GATE_D_RECOVERY_PASS，然后严格输出下面报告块并结束：\n"
            + report_block(recovery_complete)
        ),
    )

    accept_ready_work = task_report(
        task_id,
        "complete",
        True,
        "GATE_D_ACCEPT_READY",
        ["acceptance-requested work follow-up completed"],
        [],
        "",
    )
    acceptance_incomplete = task_report(
        task_id,
        "incomplete",
        False,
        "GATE_D_ACCEPTANCE_RETURN_TO_WORK",
        ["independent acceptance inspected prior Work output"],
        ["Work must produce GATE_D_ACCEPT_READY"],
        (
            "下一 Work 轮先输出自然语言标记 GATE_D_ACCEPT_READY，然后严格输出下面报告块：\n"
            + report_block(accept_ready_work)
        ),
    )
    acceptance_complete = task_report(
        task_id,
        "complete",
        True,
        "GATE_D_ACCEPTANCE_COMPLETE",
        ["independent acceptance observed GATE_D_ACCEPT_READY"],
        [],
        "",
    )

    first_prompt = (
        "这是 Gate D 的真实 ChatGPT Work 会话状态机验收。"
        "本轮故意只完成第一阶段，不要提前完成后续。"
        "先输出自然语言标记 GATE_D_WORK_FIRST_PASS，然后严格输出下面报告块并结束：\n"
        + report_block(first_incomplete)
    )
    acceptance_prompt = (
        "这是独立 Acceptance/Planning 会话。不要代替 Work 执行。"
        "检查“最新 Work 会话自然语言结果”："
        "只有它包含 GATE_D_ACCEPT_READY 时，才输出 GATE_D_ACCEPTANCE_COMPLETE 并报告 complete；"
        "否则必须输出 GATE_D_ACCEPTANCE_RETURN_TO_WORK 并报告 incomplete，要求下一 Work 轮补 GATE_D_ACCEPT_READY。\n"
        "未满足条件时使用：\n"
        + report_block(acceptance_incomplete)
        + "\n满足条件时使用：\n"
        + report_block(acceptance_complete)
    )
    return first_prompt, acceptance_prompt


def run_builtin(args, matrix, evidence_dir):
    binary = args.binary
    base = [binary, "--cdp", args.cdp]

    try:
        preflight = run_json("preflight", base + ["status"], evidence_dir, timeout_seconds=60)
        if preflight.get("authentication_required"):
            raise CommandError("ChatGPT authentication is required")
        if not preflight.get("conversation_loaded"):
            raise CommandError("ChatGPT conversation UI is not loaded")
        if not preflight.get("composer_ready"):
            raise CommandError("ChatGPT composer is not ready")
        matrix["preflight"] = {
            "status": "ready",
            "conversation_url": preflight.get("url"),
            "observed_model": preflight.get("observed_model"),
            "observed_thinking_effort": preflight.get("observed_thinking_effort"),
        }
    except Exception as error:
        matrix["preflight"] = {"status": "not-configured", "reason": str(error)}
        for name in BUILTIN_SCENARIOS:
            matrix["scenarios"][name] = scenario(
                "not-configured",
                "builtin-live",
                reason=f"authenticated ChatGPT preflight failed: {error}",
            )
        return

    normal_marker = f"FABUSHI_GATE_D_NORMAL_{args.commit[:12]}"
    normal_prompt = (
        args.prompt
        + "\n\n请在最终回复中原样包含这个验收标记："
        + normal_marker
    )
    try:
        normal = run_json(
            "normal-send",
            base
            + [
                "send",
                "--prompt",
                normal_prompt,
                "--model",
                args.model,
                "--thinking",
                args.thinking,
            ],
            evidence_dir,
            timeout_seconds=args.command_timeout_seconds,
        )
        if normal.get("state") != "complete":
            raise CommandError("normal send did not settle complete")
        if not canonical_chatgpt_url(normal.get("conversation_url")):
            raise CommandError("normal send lacks canonical conversation URL")
        if normal_marker not in (normal.get("assistant_text") or ""):
            raise CommandError("normal send assistant reply omitted the live marker")
        matrix["scenarios"]["normal_send"] = scenario(
            "passed",
            "builtin-live",
            evidence={
                "conversation_url": normal.get("conversation_url"),
                "assistant_text_sha256": sha256_text(normal.get("assistant_text") or ""),
                "approvals_clicked": normal.get("approvals_clicked", 0),
                "terminal_message": normal.get("message"),
            },
        )
    except Exception as error:
        matrix["scenarios"]["normal_send"] = scenario(
            "failed", "builtin-live", reason=str(error)
        )

    task_id = f"gate-d-{args.commit[:12]}-{int(time.time())}"
    queue_db = Path(evidence_dir) / "gate-d-queue.sqlite3"
    if queue_db.exists():
        queue_db.unlink()
    first_prompt, acceptance_prompt = build_queue_prompts(task_id)
    queue_base = [binary, "--cdp", args.cdp, "--db", str(queue_db)]
    try:
        enqueue = queue_base + [
            "queue-enqueue",
            "--task-id",
            task_id,
            "--account-id",
            "gate-d",
            "--prompt",
            first_prompt,
            "--revision",
            "1",
            "--model",
            args.model,
            "--thinking",
            args.thinking,
            "--acceptance-prompt",
            acceptance_prompt,
            "--conversation-kind",
            "work",
            "--known-exact-head",
            args.commit,
            "--ci-evidence",
            f"github-actions:{args.workflow_run_id or 'unknown'}",
            "--current-stage",
            "gate-d-authenticated-matrix",
            "--pending-work",
            "prove durable recovery and Work/Acceptance switching",
        ]
        run_json("queue-enqueue", enqueue, evidence_dir, timeout_seconds=60)

        first = run_json(
            "queue-first-process",
            queue_base + ["queue-run-once", "--account-id", "gate-d"],
            evidence_dir,
            timeout_seconds=args.command_timeout_seconds,
        )
        if not isinstance(first, dict) or first.get("state") != "complete":
            raise CommandError("first Work process did not finish a terminal assistant turn")

        after_first = run_json(
            "queue-after-first",
            queue_base + ["queue-status"],
            evidence_dir,
            timeout_seconds=60,
        )
        first_task = find_task(after_first, task_id)
        if not first_task:
            raise CommandError("queue task disappeared after first process")
        recovery = first_task.get("recovery_context")
        if first_task.get("status") != "queued" or not isinstance(recovery, dict):
            raise CommandError("first Work result did not durable-requeue with RecoveryEnvelope")
        if recovery.get("original_goal") != first_prompt:
            raise CommandError("RecoveryEnvelope lost the original goal")
        if recovery.get("acceptance_prompt") != acceptance_prompt:
            raise CommandError("RecoveryEnvelope lost the acceptance prompt")
        if not recovery.get("interrupted_turn_visible_content"):
            raise CommandError("RecoveryEnvelope lost visible assistant progress")
        if recovery.get("conversation_kind") != "work":
            raise CommandError("RecoveryEnvelope conversation kind is not work")
        first_url = recovery.get("conversation_url")
        if not canonical_chatgpt_url(first_url):
            raise CommandError("RecoveryEnvelope lacks canonical live conversation URL")

        resumed = run_json(
            "queue-resumed-process",
            queue_base
            + ["queue-run", "--account-id", "gate-d", "--max-runs", "8"],
            evidence_dir,
            timeout_seconds=args.command_timeout_seconds,
        )
        final_snapshot = run_json(
            "queue-final",
            queue_base + ["queue-status"],
            evidence_dir,
            timeout_seconds=60,
        )
        final_task = find_task(final_snapshot, task_id)
        if not final_task or final_task.get("status") != "completed":
            raise CommandError("Work/Acceptance queue did not settle completed")
        if final_task.get("conversation_kind") != "acceptance":
            raise CommandError("terminal queue task is not the final Acceptance conversation")

        kinds, runs = run_kinds(final_snapshot, task_id)
        expected_kinds = ["work", "work", "acceptance", "work", "acceptance"]
        if kinds != expected_kinds:
            raise CommandError(
                f"unexpected Work/Acceptance run sequence: {kinds}, expected {expected_kinds}"
            )
        if len(runs) != 5:
            raise CommandError(f"expected 5 durable runs, observed {len(runs)}")
        if not all(run.get("state") == "complete" for run in runs):
            raise CommandError("one or more durable queue runs did not complete")

        first_two_urls = [
            runs[0].get("canonical_conversation_url"),
            runs[1].get("canonical_conversation_url"),
        ]
        if not all(canonical_chatgpt_url(url) for url in first_two_urls):
            raise CommandError("multi-turn recovery runs lack canonical conversation URLs")
        if first_two_urls[0] != first_two_urls[1]:
            raise CommandError("RecoveryEnvelope did not resume the same live conversation")

        idle = run_json(
            "queue-idle-after-final",
            queue_base + ["queue-run-once", "--account-id", "gate-d"],
            evidence_dir,
            timeout_seconds=60,
        )
        if idle is not None:
            raise CommandError("completed task was claimed again after final Acceptance")

        run_urls = [run.get("canonical_conversation_url") for run in runs]
        common = {
            "task_id": task_id,
            "run_count": len(runs),
            "run_kinds": kinds,
            "run_urls": run_urls,
            "resumed_report_count": len(resumed.get("reports", []))
            if isinstance(resumed, dict)
            else None,
        }
        matrix["scenarios"]["multi_turn"] = scenario(
            "passed",
            "builtin-live",
            evidence={
                **common,
                "same_conversation_url": first_two_urls[0],
            },
        )
        matrix["scenarios"]["work_completion"] = scenario(
            "passed", "builtin-live", evidence=common
        )
        matrix["scenarios"]["acceptance_completion"] = scenario(
            "passed", "builtin-live", evidence=common
        )
        matrix["scenarios"]["process_restart_recovery"] = scenario(
            "passed",
            "builtin-live",
            evidence={
                **common,
                "boundary": "queue-run-once process exited; new queue-run process rehydrated SQLite state",
            },
        )
        matrix["scenarios"]["work_acceptance_work_switch"] = scenario(
            "passed", "builtin-live", evidence=common
        )
        matrix["scenarios"]["recovery_envelope"] = scenario(
            "passed",
            "builtin-live",
            evidence={
                "conversation_url": first_url,
                "envelope_version": recovery.get("version"),
                "visible_progress_count": len(
                    recovery.get("interrupted_turn_visible_content") or []
                ),
                "known_exact_head": recovery.get("exact_commit"),
            },
        )
        matrix["scenarios"]["final_completion_no_duplicate_acceptance"] = scenario(
            "passed",
            "builtin-live",
            evidence={
                "task_status": final_task.get("status"),
                "idle_reclaim": idle,
                "final_conversation_kind": final_task.get("conversation_kind"),
            },
        )
    except Exception as error:
        for name in (
            "multi_turn",
            "work_completion",
            "acceptance_completion",
            "process_restart_recovery",
            "work_acceptance_work_switch",
            "recovery_envelope",
            "final_completion_no_duplicate_acceptance",
        ):
            matrix["scenarios"][name] = scenario(
                "failed", "builtin-live", reason=str(error)
            )


def self_test():
    assert canonical_chatgpt_url("https://chatgpt.com/c/abc-123")
    assert canonical_chatgpt_url("https://chat.openai.com/c/abc_def?x=1")
    assert not canonical_chatgpt_url("https://example.com/c/abc")
    assert normalized_contains("GPT-5.6 Sol", "GPT-5.6 Sol")
    assert normalized_contains("Thinking: Extra High", "Extra High")

    matrix = {"scenarios": {name: {"status": "passed"} for name in SCENARIOS}}
    assert all(
        matrix["scenarios"].get(name, {}).get("status") == "passed"
        for name in SCENARIOS
    )
    matrix["scenarios"]["rate_limit"] = {"status": "not-configured"}
    assert not all(
        matrix["scenarios"].get(name, {}).get("status") == "passed"
        for name in SCENARIOS
    )

    with __import__("tempfile").TemporaryDirectory() as tmp:
        artifact = Path(tmp) / "observation.json"
        write_json(artifact, {"synthetic_ui": False, "natural_condition": True})
        payload = {
            "schema": "fabushi.authenticated-external-scenario.v1",
            "scenario": "rate_limit",
            "status": "passed",
            "exact_commit": "abc",
            "workflow_run_id": "123",
            "real_chatgpt": True,
            "synthetic_ui": False,
            "natural_condition": True,
            "conversation_url": "https://chatgpt.com/c/live",
            "observations": ["visible current rate-limit dialog"],
            "artifact_files": ["observation.json"],
        }
        checked = validate_external_result(
            payload, "rate_limit", "abc", "123", tmp
        )
        assert checked["status"] == "passed"
        assert checked["artifact_sha256"]["observation.json"] == sha256_file(artifact)

        missing_natural = dict(payload)
        missing_natural["natural_condition"] = False
        try:
            validate_external_result(
                missing_natural, "rate_limit", "abc", "123", tmp
            )
            raise AssertionError("natural scenario incorrectly passed without natural_condition")
        except ValueError:
            pass

        unavailable = {
            "schema": "fabushi.authenticated-external-scenario.v1",
            "scenario": "conversation_too_long",
            "status": "not-configured",
            "exact_commit": "abc",
            "workflow_run_id": "123",
            "real_chatgpt": True,
            "synthetic_ui": False,
            "natural_condition": False,
            "observations": ["natural condition absent"],
            "reason": "real ChatGPT did not naturally present the condition",
        }
        checked = validate_external_result(
            unavailable, "conversation_too_long", "abc", "123", tmp
        )
        assert checked["status"] == "not-configured"

    repository_driver = Path(__file__).with_name(
        "real-environment-scenario-driver.py"
    )
    assert repository_driver.is_file()
    assert sha256_file(repository_driver)
    print("authenticated-e2e-matrix self-test: PASS")


def parse_args():
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--binary")
    parser.add_argument("--cdp")
    parser.add_argument("--evidence-dir")
    parser.add_argument("--commit")
    parser.add_argument("--workflow-run-id", default="")
    parser.add_argument("--prompt", default="回复：Linux E2E PASS")
    parser.add_argument("--model", default="GPT-5.6 Sol")
    parser.add_argument("--thinking", default="Extra High")
    parser.add_argument(
        "--external-driver",
        default="",
        help="optional explicit override; empty uses the repository-owned driver",
    )
    parser.add_argument("--allow-browser-crash", action="store_true")
    parser.add_argument("--allow-network-faults", action="store_true")
    parser.add_argument("--network-interface", default="")
    parser.add_argument("--fault-window-seconds", type=float, default=12.0)
    parser.add_argument("--certify", action="store_true")
    parser.add_argument("--command-timeout-seconds", type=int, default=14400)
    parser.add_argument("--external-timeout-seconds", type=int, default=7200)
    args = parser.parse_args()
    if args.self_test:
        return args
    for name in ("binary", "cdp", "evidence_dir", "commit", "workflow_run_id"):
        if not getattr(args, name):
            parser.error(f"--{name.replace('_', '-')} is required")
    return args


def main():
    args = parse_args()
    if args.self_test:
        self_test()
        return 0

    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    matrix = {
        "schema": "fabushi.authenticated-linux-e2e-matrix.v2",
        "exact_commit": args.commit,
        "workflow_run_id": args.workflow_run_id,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "environment": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "chromium": chromium_version(),
            "python": platform.python_version(),
        },
        "requested_profile": {
            "model": args.model,
            "thinking_effort": args.thinking,
        },
        "scenario_driver": {"status": "pending"},
        "real_chatgpt": False,
        "synthetic_ui": False,
        "preflight": {"status": "pending"},
        "scenarios": {},
    }

    run_builtin(args, matrix, evidence_dir)
    matrix["real_chatgpt"] = matrix.get("preflight", {}).get("status") == "ready"
    run_real_environment_driver(args, matrix, evidence_dir)

    for name in SCENARIOS:
        matrix["scenarios"].setdefault(
            name,
            scenario("not-configured", "matrix", reason="scenario did not run"),
        )

    approvals = []
    for file_name in (
        "normal-send.json",
        "queue-first-process.json",
        "queue-resumed-process.json",
    ):
        path = evidence_dir / file_name
        if not path.is_file():
            continue
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except Exception:
            continue
        if isinstance(payload, dict):
            if "approvals_clicked" in payload:
                approvals.append(int(payload.get("approvals_clicked") or 0))
            reports = payload.get("reports")
            if isinstance(reports, list):
                for report in reports:
                    approvals.append(int(report.get("approvals_clicked") or 0))
    matrix["approval_observation"] = {
        "status": "observed" if any(value > 0 for value in approvals) else "not-observed",
        "approvals_clicked": sum(approvals),
        "note": "An actual Allow once card is required before this can be evidence of auto-confirm.",
    }

    matrix["certification_complete"] = all(
        matrix["scenarios"][name]["status"] == "passed" for name in SCENARIOS
    )
    write_json(evidence_dir / "matrix.json", matrix)
    matrix_hash = sha256_file(evidence_dir / "matrix.json")
    (evidence_dir / "matrix.sha256").write_text(
        f"{matrix_hash}  matrix.json\n", encoding="utf-8"
    )

    print(json.dumps(matrix, ensure_ascii=False, sort_keys=True, indent=2))
    if args.certify and not matrix["certification_complete"]:
        print(
            "Gate D certification requested but one or more required scenarios are not passed.",
            file=sys.stderr,
        )
        return 3
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
