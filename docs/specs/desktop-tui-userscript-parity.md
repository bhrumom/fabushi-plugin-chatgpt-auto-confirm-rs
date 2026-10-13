# Desktop TUI and continuous goal supervision

Authority: explicit user request on 2026-10-12; userscript reference commit
`4ff2f386dc40ab0e542250c0710f05c77bf1f612` (2.10.47).

## Required behavior

- Operate one installed ChatGPT desktop instance and its main renderer only.
- Dispatch the complete supplied iOS and Android goals to separate conversations.
- Rotate supervision without confusing transcripts, models, approvals or goals.
- Provide a terminal workbench with task sidebar, selected task feed, multiline
  goal input, once/continuous modes, per-task model/reasoning, attachments,
  automatic authorization controls, pause/resume/cancel/delete/edit/open actions,
  recent activity, settings, import/export and renderer cleanup.
- Preserve persistent task/run state, dispatch intents, attachments, exact
  conversation/marker binding, recovery context and independent review reports.
- Continuous goals alternate work and fresh independent acceptance. Only a
  correctly bound acceptance report may declare the goal complete. A terminal
  assistant response is a completed turn, never proof of product completion.
- Explicit connection interruption takes precedence over terminal Copy/actions.
  After the eight-second guard and the normal send interval, dispatch a fresh
  conversation with the original goal, phase/round, source conversation id,
  recovery reason, prior checkpoint, work result and latest public tool/reply
  progress. Empty rendering must not erase the previous checkpoint. Recognize
  bilingual interruption banners beside the assistant body; exclude quotations.
- Conversation loading failures click only the error card retry action, at
  thirty-second intervals, preserving the same binding and checkpoint. They
  never count as a final reply or trigger a new conversation.
- Carry latest work/context into recovery; retries must not overwrite drafts,
  abandon pending authorization or duplicate confirmed sends.
- Model and reasoning must be verified before every new dispatch. Live edits
  affect later unsent rounds only. Unsupported values fail closed.
- Prefer the dropdown's current-conversation authorization over its default
  allow-once button. Recognize provider-specific labels such as `Allow GitHub
  for this conversation`, native cards without semantic roles, and shortcut
  badges. Use allow-once only when a conversation grant is unavailable; never
  persistent/global grants.
- Supervision continues after the TUI closes and resumes after worker restart.
- A single renewable endpoint lease and transactional revisions prevent multiple
  supervisors or a stale worker from overriding a manual pause/cancel/edit.

## Architecture

`cli -> runtime -> application -> domain`, with SQLite and desktop CDP adapters
pointing inward. Pure policy and report parsing live in domain/application.
The CDP adapter contains DOM projection and requested effects only. TUI lives
at the runtime composition boundary and uses the same durable store as the
headless supervisor. SQLite WAL stores tasks, sessions, events, settings and
endpoint leases. Local desktop conversation identity is persisted as native
identity with a verified sidebar binding; it is never fabricated as a remote URL.

## Reference mapping

- bootstrap/config -> typed settings, model/effort presets and policy defaults;
- storage/activity -> SQLite journal, bounded recent feed, import/export;
- workspace ownership -> endpoint lease, revisions and logical workspace;
- host navigation/memory -> desktop sidebar switching, checkpoint/reload;
- attachments/identity -> private copied files, content hashes and task markers;
- DOM/turn adapter -> native turn projection and exact conversation ownership;
- task state -> durable task lifecycle and manual controls;
- composer/model -> paced verified model/effort and upload-before-send;
- reply/approval -> stable latest-turn actions, scoped approvals and safe modals;
- send/navigation -> durable intent and post-send identity confirmation;
- review/settlement -> identity-bound JSON, fresh review and next-work handoff;
- supervision -> rotation, retries, stalls/rate limits/interruptions/length carry;
- workbench/lifecycle -> native terminal sidebar/feed/composer and background worker.

Browser-extension tabs/discard, page visibility timers, IndexedDB/localStorage
budgets, drag-to-new-browser-workspace and inline image/video previews are
replaced by a single desktop endpoint owner, independent Rust clock, SQLite,
native file references and terminal file metadata. No browser extension or
web-only recovery dependency is required.

## Verification

Regression gates cover report identity, stale completion, intent deduplication,
pause/edit races, recovery priority, leases, durable restart, attachments,
model readiness, sidebar switching and TUI controls. Run Rust fmt/test/clippy,
architecture gate and sanitized desktop DOM fixtures. Live evidence must show
both full goals dispatched into separate exact desktop conversations, rotation,
durable restart and TUI operation. Task progress and actual mobile migration
completion are recorded separately; no CI/source/packaged acceptance is inferred
from a ChatGPT prose summary.

## Observable supervision

The workbench prioritizes the current attempt, last confirmed task marker,
last scan age, elapsed scan time, phase/round and retry countdown. Attempted
switches do not increment confirmed switch counts. Every scan start, result,
confirmed conversation switch, recovery and failure produces a durable event
and console log. V shows combined task events, newest first; selected-task
events appear before the long original goal. The CLI logs command reads the
same journal. Sidebar switching must wait for the requested conversation's
marker or its own loading error before processing it.

## Native card recovery and one-hour operation journal

- Work-conversion cards may be ordinary containers with no dialog/group role.
  Match the card heading plus both choices, including the countdown badge,
  and click only the scoped Stay in chat control. Record success only when the
  action executes; do not advance to independent review while that card exists.
- Loading errors may be outside a cached transcript main and contain nested
  inline markup. A loading-failure latch blocks terminal classification and
  fresh handoff until the same task marker is visible again. This applies before
  dispatch confirmation as well as after durable binding. Possible-send intents
  never reset/replay merely because the 90-second confirmation window elapsed.
  A marker-verified transient binding can be retained for identity resolution.
- TUI and CLI logs show specific operations from the preceding hour. The SQLite
  query filters scan/reply rows before applying the history limit. Navigation,
  model/file preflight, send trigger/confirmation, approval, card retry, review
  and context handoff remain separately visible. Scans remain in the raw journal
  and operational worker log without displacing the action timeline.
- Regression coverage includes role-less Work cards with countdowns, quoted
  controls, nested loading errors outside cached main, unconfirmed-send loading
  failures, successful/failed Stay actions and historical actions after 500 polls.

Only task-owned navigation is permitted: a missing temporary sidebar id never
triggers iteration/clicks through unrelated conversations. A current exact
marker may resolve its own temporary id in place; otherwise retain the pending
session and report that ownership cannot yet be recovered. The 15-second
navigation interval is desktop-wide, not separately per task. Native load-error
cards may have direct text nodes followed by their retry button; strip decision
controls when projecting the error label and retry from that same container.

A report with complete terminal controls receives its second stable check
without being starved by a different task's approval state. Authorization
priority is bounded by a sixty-second overdue-task threshold. Exact unique
user markers confirm a desktop dispatch even when a cached transcript makes
the user-turn baseline equal to the fresh conversation's count.

Fresh-chat model preflight tolerates up to three seconds of native trigger
mounting before failing closed, without selecting a guessed model.

Another registered task's authorization card may be parked to start a ready
execution/review stage only after its exact marker and stable binding are
verified and the composer is empty. Preserve that task's binding and pending
card for its next supervision visit; never dismiss or grant it on behalf of
the new stage. Unowned authorization and all drafts still block navigation.

## Autonomous sidebar recovery

A missing mounted sidebar row is not proof that the conversation was deleted.
Locate only the registered conversation id: open an explicitly labelled sidebar
control if collapsed, scroll its list to mount virtual rows, then click only the
matching id. Never open other conversations to search their transcripts. All
render waits are bounded timers, not indefinite animation-frame promises.
Navigation-unavailable errors preserve the dispatch/binding and retry after
thirty seconds, independent of prior retry count. Startup prevents background
renderer/timer throttling; TUI checks worker leases and can restart a missing
worker, while an operator service can keep supervision alive without the TUI.
The dashboard distinguishes an online worker from tasks undergoing recovery.

An exact owned marker may identify the already open task even when its sidebar
row is unmounted and only a transient native identity is projected. Keep its
registered durable id; a different stable sidebar id still fails ownership.
Bound each list scan to 24 steps and retain its scroll position for a later
retry, avoiding an endless restart from the top of a long virtual list.

## Embedded desktop workbench entry

The supervisor remounts a fixed Fabushi workbench entry in the desktop main renderer. Its isolated shadow DOM preserves native composer and authorization selectors. Opening the panel exposes durable task state, round, one-hour operation journal, goal/once dispatch with model and effort, per-task pause/resume/retry and verified conversation open, global supervision controls, and a user-requested native TUI launcher. Commands are allowlisted and applied by runtime to the existing Queue; the panel does not own supervision policy or create a second database. Text from goals, task titles and journals is rendered with textContent. Inputs survive snapshot refresh, and a failed add preserves the user's input. Reload/remount reinstalls the entry on the next supervisor cycle.

## Desktop embedded UI parity (2026-10-13)

Reference verified against userscript main `4ff2f386dc40ab0e542250c0710f05c77bf1f612`, `workbench-and-lifecycle.inc.js` mount/paint contract. Replace the simplified panel with the same purple bottom-right launcher, 880×700 split desk, 250px task sidebar, header actions, settings, scrollable task feed, notice and bottom composer. Preserve responsive dimensions and safe literal text rendering.

Required acceptance matrix:
- New-task selection, task details/status badges, logical workspace groups and drag/move controls. Browser new-tab ownership/restoration maps to durable groups within the user's explicitly required single desktop instance; never open unrelated chats or additional instances.
- Goal editing; independent per-task pause/resume, cancellation, terminal/paused-only deletion, exact recorded conversation opening and non-replaying retry. Active dispatch remains identity-bound.
- Once/goal mode, three existing model presets, five correctly labelled reasoning positions, new-task defaults distinct from selected-task future-round preferences, per-task/default auto approval and global approval control.
- Native file picker, clipboard/drop files, duplicate selection handling, image/video previews, clear attachments, bounded private local save before Queue insertion and existing upload-before-send contract. Failed submissions preserve text/files; acknowledgement cannot clear newer input.
- Selected goal/profile/attachments, retained two-hour task events and public reply/checkpoint, current round/phase/session id, authorization/recovery counts, last scan age and cooldown. A separate one-hour concrete operation feed remains available with task/all filtering.
- Global pause/resume, safe renderer memory cleanup, storage/scan/worker metrics, same-database native TUI. Cleanup defers on native response/approval/draft or embedded unsaved text/files; no browser discard API required.
- Queue JSON import/export, imports paused and attachments explicitly reselected rather than trusting imported host filesystem paths.
- Retain the same UI node, drafts, files, handlers and preview URLs across hydration removals through a narrowly scoped mount observer. Reinstall after full renderer reload. Commands acknowledge individually, remain allowlisted, and use transactional Queue revisions.

Verify meaningful DOM and runtime regressions, Rust gates, CI at exact HEAD and installed desktop entry metadata. Record any browser-only substitution explicitly; do not claim standalone userscript replacement from UI parity alone.

The embedded UI refresh/command loop runs independently of native conversation polling under the same endpoint owner; its scoped lifetime aborts with that worker. Cancelled tasks resume without replacing a confirmed send identity. Deletion uses revision compare-and-swap, and failed attachment staging cleans temporary bytes.
