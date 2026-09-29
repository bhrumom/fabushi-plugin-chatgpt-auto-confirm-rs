# Authenticated Linux Gate D operations

Status: final-environment acceptance runbook

The workflow .github/workflows/authenticated-linux-e2e.yml runs only on a user-owned self-hosted Linux runner with an already authenticated ChatGPT Chromium session. It never imports or exports passwords, cookies, OTPs, access tokens, or browser storage.

## Modes

Use certify=false for preflight. The workflow still writes evidence/matrix.json and records unavailable external conditions as not-configured. This mode is useful for checking the authenticated browser, model/thinking selection, the shipping queue path, RecoveryEnvelope replay, and Work/Acceptance switching without pretending final certification succeeded.

Use certify=true only for final Gate D certification. The matrix process exits non-zero unless all 15 required scenarios are passed. The evidence artifact is uploaded even when certification fails.

## Built-in live scenarios

The repository-owned harness directly proves: normal send/final completion; same-conversation multi-turn RecoveryEnvelope resume; Work completion; Acceptance completion; a new-process SQLite rehydration boundary; Work -> Acceptance -> Work switching; RecoveryEnvelope preservation; and no duplicate Acceptance after final completion.

The queue scenario intentionally uses separate CLI processes around the first incomplete Work result. The second process must reconstruct the task from SQLite and continue from the canonical conversation URL carried by RecoveryEnvelope. Subsequent runs prove the Work/Acceptance sequence work, work, acceptance, work, acceptance before terminal settlement.

## External real-environment scenario driver

The following scenarios are not safely reproducible on every authenticated runner without extra host state: target_crash_recovery, browser_crash_recovery, message_confirmation_timeout, continuation, disconnection, rate_limit, and conversation_too_long.

Install an executable driver on the self-hosted runner and pass its absolute path through the workflow scenario_driver input. The harness invokes it without a shell:

driver run <scenario> --binary <shipping-binary> --cdp <endpoint> --evidence-dir <scenario-dir> --commit <sha> --model <model> --thinking <thinking>

The driver writes any files under the supplied scenario directory and prints exactly one JSON document to stdout with schema fabushi.authenticated-external-scenario.v1. A passing document contains at least:

- scenario: the requested scenario name;
- status: passed;
- exact_commit: the workflow SHA;
- real_chatgpt: true;
- synthetic_ui: false;
- conversation_url: a canonical https://chatgpt.com/c/... or https://chat.openai.com/c/... URL;
- observations: a non-empty list of concrete live observations;
- artifact_files: optional paths relative to the scenario evidence directory;
- fault_injection: none, host, or network as applicable.

For rate_limit and conversation_too_long, a passed result also requires natural_condition=true. Injecting a fake dialog, toast, assistant turn, or DOM marker is not acceptable Gate D evidence.

The driver must not read or export cookies, tokens, passwords, OTPs, or raw authenticated browser storage. Host-level process/network controls and normal CDP actions against the already authorized profile are allowed when required by the scenario.

## Evidence review

Retain the uploaded authenticated-linux-e2e-<sha> artifact. Verify matrix.json has schema fabushi.authenticated-linux-e2e-matrix.v2, exact_commit equals the PR HEAD, certification_complete=true, and every required scenario is passed. Verify matrix.sha256 against matrix.json and retain any external scenario artifacts and their hashes.

A green ordinary ci run is not Gate D. A preview Gate D run with certification_complete=false is not Gate D certification. A mock, fixture, synthetic DOM state, or a run from another commit is not Gate D evidence.
