# Work status in Slack

In the ordinary ACP fallback, an ambient group contribution has two turns: a
private participation
decision, then the accepted response. Negotiated shared attention uses the
[attention path](harness.md#optional-shared-attention) instead. A pass produces no
public indicator. Direct
messages and explicit addresses already commit to a response under the connector's
participation policy. The agent's tools and permission rules are unchanged; the
participation prompt asks for no tools and client callbacks are refused in that
phase. This prompt convention is not a universal engine-side tool sandbox.

For an existing thread the connector first tries Slack's
[`agents.sessions.setStatus`](https://docs.slack.dev/reference/methods/agents.sessions.setStatus/).
If that optional API is unavailable, a temporary working message uses the same
bot's existing `chat:write` access. Unthreaded DMs and channel replies use that
message directly, so status never creates a different thread. There is no global
presence update, cross-room broadcast, or additional credential setup.

The message stays in the exact reply destination. It shows the current action and
the agent's public ACP tool title, plus its tool name when supplied and distinct:
“Running · Run unit tests (workspace_run)”, for example. Partial progress updates
retain that label; parallel tools add a count to the same line. Completed tools
leave the active set, and once all finish the line says “Thinking…”.
Existing threads with native enum-only status also receive this detail line when
tools run. A pending approval or question says it is waiting for the person;
queued delivery shows whether it is waiting for reply capacity or reply spacing. Actual delivery says it is sending. Private transport diagnostics are not copied into public status text.

Labels use plain text, are limited to one line and 180 characters, and cannot
trigger Slack mentions or formatting. Only ACP presentation fields (`title`,
`name`, `kind`, `status`) are used; raw arguments, output, locations, and private
reasoning are not copied. Agents should keep secrets out of their public tool
titles. Reasoning tools show only “Thinking…”. Updates reuse the same message,
are coalesced at a maximum of one every 1.5 seconds, and honor Slack rate limits.
Successful delivery or a pass removes the temporary message. Failed/interrupted
work leaves a scoped explanation instead of silently disappearing. Operational
messages carry `acp_work_status` metadata and are excluded from this connector's
context and agent-trigger processing, including messages from peer connectors.
When a detailed recovery notice is queued, the same indicator shows delivery in
progress and is removed once the notice is delivered. It does not add another
generic failure message. Recovery notices carry `acp_connector_notice` metadata
and also stay outside peer work intake; a person can still ask about one in its
thread and retrieve that root as context.

Status transitions and receipts are durable. A lost POST response is looked up
by its exact client message ID before cleanup; it is never blindly reposted.
Deletes and updates are retriable, and rate-limit deadlines are honored. If Slack
itself is unreachable, delivery and cleanup remain pending. An API rejection is
recorded in Activity; the connector cannot guarantee a visible message when Slack
rejects all writes.

Run `tests/work_status.py` for native lifecycle, permission, cancellation, and
recovery checks, and `tests/scoped_work.py` for fallback visibility, quiet
participation, simultaneous tool activity, lost delivery receipts, and bounded continuations. Both use actual
adapter and independent ACP processes with loopback Slack; neither posts to Slack.

This adapter follows the shared [interaction agreement](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/interaction.md). Activity meaning originates with the ACP producer; this adapter owns Slack placement, aggregation, fallback and cleanup.
