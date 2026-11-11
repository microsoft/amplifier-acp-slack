# Public speech, activity and source continuity

The shared [interaction agreement](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/interaction.md)
owns the meaning of public speech and delivery. This document describes its
Slack placement and optional attention integration. [Work status](work-status.md)
owns temporary indicators and their cleanup.

## Public speech

Thinking and tool activity are temporary status indicators. Completed public commentary
is a new, permanent Slack message, so people and other agents can reply between
contributions. Spoken messages are never edited into later answers. Explicit corrections
are new messages.

When an agent negotiates the shared ACP message-completion capability, commentary can be
delivered before its turn ends. Without that capability, explicitly identified
commentary waits for a successful turn result. Plain ACP and opaque agents still work; a
pause in text or an unfinished draft is not assumed to be a message boundary. Private
thought chunks are never published. Final-answer selection remains separate from
commentary.

A durable per-message delivery record prevents automatic replay after uncertain sends.
Confirmed Slack events or history can settle a missing receipt. Commentary and the final
answer share the turn’s existing reply allowance. The status API exposes `public_speech`
delivery counts, and Activity records bounded delivery receipts without duplicating
message contents.

## Optional shared attention

When an ACP endpoint advertises enabled `com.microsoft.amplifier.attention` v1,
ambient group events use its `_amplifier/attention` method after session setup.
The connector supplies bounded public channel/thread facts and the original Slack
timestamp. It checks the source again before attaching the returned one-use offer
to `session/prompt`. Instance owns the evaluator and budgets; no decision provider,
credential or evaluator executable is configured in this adapter. A hold creates
no prompt; an offer skips the legacy LLM participation preflight. The agent still
may return pass. Ordinary ACP endpoints retain the existing bounded preflight;
DMs, accepted-work continuation and explicit mentions-only installations retain
their existing behavior. Do not run the superseded adapter-owned helper alongside
this path. Source changes, permission checks and delivery receipts still apply.

ACP chunks reach this adapter as they arrive. Ordinary group final-response
output is buffered until its speak/pass decision, so private drafts and pass
envelopes do not leak into Slack. Completed commentary follows the separate
public-speech path above, including delivery before turn end when negotiated.
Work/tool progress uses separate activity indicators. Attention admission does
not itself turn a draft into completed speech.

## Source continuity

When the Instance advertises source-delivery v1, active updates and attention
use the same source ID, revision and original timestamp. The Instance's verified
delivery facts inform its social judgment. They do not grant task authority,
prove completion or automatically suppress another reply. An ordinary ACP agent
without this capability retains the existing input path.

See the [work continuity agreement](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/work-continuity.md).
