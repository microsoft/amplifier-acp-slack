# Work status in Slack

Direct requests and explicit addresses can show Slack's native working
indicator. Actual work maps to `processing`; an unresolved permission or ACP
question maps to `suspended`; completed, failed, or cancelled work maps to
`active` (ready for another request). A reply awaiting delivery remains working.
Ambient participation checks do not announce themselves.

The connector uses Slack's optional
[`agents.sessions.setStatus`](https://docs.slack.dev/reference/methods/agents.sessions.setStatus/)
method with the existing bot's `chat:write` scope. The indicator belongs to the
existing thread root, or the triggering message for an unthreaded conversation.
This does not move replies into threads or post status messages. Each agent
updates only its own status; Slack combines statuses when several agents share
a thread.

Status delivery is durable. Only transitions and retry deadlines cause Slack
requests. Unchanged working states are not repeatedly sent. Completed targets
are removed from the reconciliation queue; operational activity retains the
transition history. Cleanup runs on graceful shutdown and after crash recovery.
Interrupted model turns are not replayed to recover an indicator. If Slack is
unreachable or rate limited, cleanup remains pending until access returns.

Some workspaces do not enable Slack's agent-task experience. A
`feature_disabled`, `unknown_method`, or access rejection disables further
indicator attempts until connection configuration or credentials change. Agent
work and replies continue normally. The connector does not ask for extra
permissions or substitute cosmetic chat messages. Diagnostic capability state
is available through `work_status::status`; it is separate from agent health.

Editing a source message invalidates pending approvals and stale output; it
does not by itself interrupt all running ACP work. A still-running turn remains
working until it finishes or is actually cancelled.

Run `python3 tests/work_status.py` after `cargo build` for actual adapter,
independent ACP process, and loopback Slack tests. No external messages, model
calls, or live Slack probes are used.
