# Native one-shot wakeups

In an interactive session, ask Codex to return later and continue work you have
already authorized. For example:

> In 30 seconds, come back and check the build result. Register the wakeup now
> and finish this reply; I will keep this CLI open.

Codex can register the reminder immediately and finish its current answer. You
can keep talking or give it another task while the reminder waits. When it is
due, the framework starts a normal follow-up turn in the same conversation.
If another turn is running, it waits for a safe boundary. The follow-up keeps
the conversation context and uses the current tools, permissions and approval
policy. A reminder does not grant permission for additional work.

## Tools and management

The model sees three native tools in the `clock` namespace:

| Tool | Arguments | Result |
| --- | --- | --- |
| `schedule_wakeup` | `duration_ms`: integer 1–43,200,000; `instruction`: nonblank UTF-8 text, at most 16 KiB | A generated `task_id`, original turn ID, scheduling/due timestamps and status |
| `list_wakeups` | No arguments | This session's pending tasks, including tasks due while another turn runs |
| `cancel_wakeup` | `task_id` from a registration/list receipt | Whether that pending task was cancelled |

You do not have to call these tools yourself or write JSON. Ask naturally:

> What wakeups are pending? Cancel the reminder to check the build.

Each session permits up to 64 pending reminders. Invalid arguments, an oversized
instruction, an unavailable session or an exhausted limit produce an error
without registering a task. Cancel an existing task before retrying a capacity
error. Scheduling the same instruction again creates a separate task.

`scheduled` means the deadline is in the future; `due` means the task is still
pending and may be waiting for the active turn. Cancellation succeeds only before
the framework claims it. An unknown or already claimed ID returns not-pending;
it does not claim to stop work already running. Use the normal interrupt control
to interrupt a running turn. Each claimed task runs at most once; a model or tool
failure is shown through normal turn error handling and is not automatically
rescheduled.

## Lifetime and compatibility

Keep the session open. Completing an answer does not cancel reminders, and
waiting does not keep the current turn active. The transcript distinguishes
the actual registration receipt, the framework's **Wakeup started** marker,
and the model's subsequent work. Relative delay uses a monotonic clock; the
displayed UTC due time is informational, not a precision execution guarantee.

The embedded interactive TUI and TUI sessions hosted by the default shared daemon
or an explicitly selected remote app-server are supported. These hosts use the
existing `Cli` or `VSCode` session provenance for ordinary User threads, including
native `/side` forks. Eligible editor/app-server User threads receive the same
tools under the same lifecycle rules. No special server startup flag is needed.
Restricted roots, internal/reviewer/subagent threads and `codex exec`, MCP,
custom or unknown session sources do not expose the tools. Managed clock
prohibitions and tool policy still apply. Existing current-time and synchronous
sleep tools retain their behavior.

Leaving a thread with no remaining live subscription cancels its pending
reminders immediately, even if a shared server keeps the thread loaded briefly.
This includes closing the CLI and navigation that actually detaches the last
subscriber. A retained side pane or a second client still subscribed keeps the
thread alive. Reopening a detached thread does not restore cancelled reminders;
forks start with an empty reminder registry. Reminders are in memory, not a
persistent scheduler: they do not survive session teardown or process restart.

## Verification

Build with the repository's pinned Rust toolchain, Cargo and Git available on
`PATH`, plus Python 3.11 or newer (an existing uv-managed interpreter also works).
The build command reuses the repository's checksum-verified Codex V8 package
artifacts for the native code-mode host. The existing `dev-small` profile keeps
debug artifacts small and disables
incremental object caches for bounded verification disk use. Cargo uses Git's
native transport for pinned Git sources and recursive submodules; no global Git
or Cargo configuration is required.

Native Rust tests cover tool/policy contracts, pending-state read-back,
cancellation, serialized admission, conversation continuity, subscriber cleanup,
typed history and TUI presentation. `scripts/wakeup_verification.py` runs one
explicit native test command and translates its real harness results for Loops.
It rejects empty selections, failures and ignored required tests, and uses the
repository test runner’s standard 8 MiB Rust test stack.

`scripts/wakeup_live.py` runs the built candidate TUI against an owned private
app-server endpoint using the approved isolated runtime binding. It retains a
sanitized prompt, native task/turn identities, input timeline, transcript views
and exact independent file read-back. It distinguishes immediate registration,
normal first-turn completion, framework-initiated continuation with no new user
input, and completed work. A separate post-success pending reminder and CLI-exit
tail checks cancellation while the server remains alive. It does not replace the
installed CLI, global configuration or existing conversation history.

To reproduce with the candidate checkout:

```sh
python3 scripts/wakeup_build.py -- cargo build --locked --profile dev-small -p codex-cli --bin codex -p codex-code-mode-host --bin codex-code-mode-host
cd codex-rs
RUST_MIN_STACK=8388608 cargo test --locked --profile dev-small -p codex-core --lib wakeup
RUST_MIN_STACK=8388608 cargo test --locked --profile dev-small -p codex-core --test all suite::wakeup::
RUST_MIN_STACK=8388608 cargo test --locked --profile dev-small -p codex-tui --lib wakeup
cd ..
python3 scripts/wakeup_live.py --model gpt-5.6-sol --runtime-env /path/to/runtime.env
```

The runtime binding must name `CODEX_WAKEUP_SMOKE_HOME` and
`CODEX_WAKEUP_AUTH_SOURCE`. The latter is an approved read-only credential file.
Select a real model available to the candidate client and account; the model
catalog is client-version dependent. The evidence records the actual model from
native turn contexts. A terminal provider error fails the check immediately.
The driver copies it into a fresh task-owned home, uses a temporary fixture and
private Unix socket, and cleans up only its own processes and fixture. Copied
credentials remain in the reported isolated runtime home for operator removal
after all live verification finishes. Do not publish that home or its auth file.
