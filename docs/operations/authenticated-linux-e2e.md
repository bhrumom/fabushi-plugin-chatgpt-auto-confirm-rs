# Authenticated Linux Gate D runbook

The workflow `.github/workflows/authenticated-linux-e2e.yml` runs only on a user-owned self-hosted Linux runner with an already authenticated ChatGPT Chromium session. It never imports or exports passwords, cookies, OTPs, access tokens, or raw browser storage.

## Default ownership

Gate D no longer depends on an unversioned runner-installed scenario program. The exact checked-out commit owns both orchestration layers:

- `scripts/authenticated-e2e-matrix.py` owns the 15-scenario matrix and final certification decision.
- `scripts/real-environment-scenario-driver.py` is the default real-environment driver for target crash, browser crash, message-confirmation timeout, continuation, disconnection, rate-limit, and conversation-too-long.

The optional `scenario_driver` workflow input is only an explicit override. Leaving it empty is the normal and certification-ready path.

## Modes

Use `certify=false` for preflight. The workflow still writes `evidence/matrix.json` and records unavailable host permissions or natural external conditions as `not-configured`. This mode is useful for checking the authenticated browser, model/thinking selection, shipping queue path, RecoveryEnvelope replay, Work/Acceptance switching, and repository-owned driver wiring without pretending final certification succeeded.

Use `certify=true` only for final Gate D certification. The matrix exits non-zero unless all 15 required scenarios are passed. The evidence artifact is uploaded even when certification fails.

## Repository-owned real-environment controls

The driver never fabricates ChatGPT DOM state, assistant text, dialogs, toasts, rate-limit surfaces, or conversation-too-long markers.

It uses only these real controls:

- target crash: close the RunWorker-owned real CDP page target through Chromium's local `/json/close/<target-id>`, then require the shipping RunWorker to recover to a canonical real conversation;
- browser crash: after explicit `allow_browser_crash=true`, discover the authenticated Chromium process/profile from the configured CDP port, run the shipping queue path in managed-browser mode, SIGKILL that Chromium process, then require AccountBrowserActor to restart the browser/profile and recover the run;
- message-confirmation timeout: after explicit `allow_network_faults=true`, temporarily bring the selected/default-route Linux interface down before confirmation and require a real shipping dispatch retry;
- continuation: interrupt the real network after the initial user turn is confirmed and require the shipping continuation counter to advance before terminal completion;
- disconnection: interrupt the real network after initial dispatch, require the real page semantic observation `connection_interrupted=true`, restore the interface, and require the shipping run to settle;
- rate-limit: inspect the current real page. If no currently visible semantic rate-limit surface exists, report `not-configured`. If it exists naturally, run shipping handling and require pause/recovery evidence. A passed result requires `natural_condition=true`;
- conversation-too-long: inspect the current real page. If the real condition is absent, report `not-configured`. If it exists naturally, require the shipping application to return its real fresh-conversation recovery semantic. A passed result requires `natural_condition=true`.

Network fault control uses the Linux `ip link set dev <iface> down/up` path. The workflow input `network_interface` may name the interface; when empty the repository driver discovers the default-route non-loopback interface. The runner must be root or have passwordless sudo permission for the required `ip` command. Missing permission is `not-configured`, never PASS.

Browser and network fault controls are destructive and therefore explicit workflow opt-ins. Final `certify=true` cannot succeed while those required scenarios remain `not-configured`.

## Built-in live scenarios

The matrix harness directly proves normal send/final completion, same-conversation multi-turn RecoveryEnvelope resume, Work completion, Acceptance completion, a new-process SQLite rehydration boundary, Work -> Acceptance -> Work switching, RecoveryEnvelope preservation, and no duplicate Acceptance after final completion.

The queue scenario intentionally uses separate CLI processes around the first incomplete Work result. The second process must reconstruct the task from SQLite and continue from the canonical conversation URL carried by RecoveryEnvelope. Subsequent runs prove the Work/Acceptance sequence `work -> work -> acceptance -> work -> acceptance` before terminal settlement.

## Evidence contract

Every repository-driver invocation receives:

```text
run <scenario>
  --binary <exact-HEAD shipping binary>
  --cdp <authenticated CDP endpoint>
  --evidence-dir <scenario directory>
  --commit <exact workflow SHA>
  --workflow-run-id <GitHub Actions run ID>
  --model <required model>
  --thinking <required thinking>
```

The driver prints exactly one JSON document with schema `fabushi.authenticated-external-scenario.v1`. Every status binds `exact_commit` and `workflow_run_id`. A passed result additionally requires:

- `real_chatgpt: true`;
- `synthetic_ui: false`;
- canonical `https://chatgpt.com/c/... ` or `https://chat.openai.com/c/...` conversation URL;
- non-empty concrete `observations`;
- at least one scenario-local artifact path;
- SHA-256 validation of every listed artifact by the matrix harness;
- `natural_condition: true` for rate-limit and conversation-too-long.

The matrix records the SHA-256 of the repository-owned driver from the exact checked-out commit. If `scenario_driver` overrides it, the override path and SHA-256 are recorded and the matrix source changes to `external-driver-override`.

## Rate-limit threshold semantics

Production application behavior is authoritative: a currently visible real rate-limit semantic surface is dismissed and backed off for 5 minutes, up to 3 pauses. If the real condition remains after the threshold, the application emits `FreshConversationRequested` with reason `rate_limit_threshold_exceeded`; runtime classifies that reason as requiring a fresh conversation and builds a RecoveryEnvelope with no stale conversation URL. It does not simply fail the task.

## Evidence review

Retain the uploaded `authenticated-linux-e2e-<sha>` artifact. Verify:

1. `matrix.json` has schema `fabushi.authenticated-linux-e2e-matrix.v2`;
2. `exact_commit` equals the PR HEAD and `workflow_run_id` equals the Actions run;
3. `real_chatgpt=true` and `synthetic_ui=false`;
4. `scenario_driver.repository_owned_default=true` unless an explicit override was intentionally used, and its SHA-256 is present;
5. every required scenario is `passed` for final certification;
6. every passed real-environment scenario has canonical conversation URL, concrete observations and non-empty `artifact_sha256`;
7. rate-limit and conversation-too-long have `natural_condition=true`;
8. `matrix.sha256` verifies `matrix.json`.

A green ordinary CI run is not Gate D. A preview Gate D run with `certification_complete=false` is not Gate D certification. A mock, fixture, synthetic DOM state, missing natural condition, or a run from another commit is not Gate D evidence.
