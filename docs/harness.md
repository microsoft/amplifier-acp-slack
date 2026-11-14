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

## Inspect an original submission

`POST /api/v1/submissions/inspect` uses the existing management `observe` scope.
Supply `{"sources":[{"team":"T…","channel":"C…","thread":"","ts":"…","revision":"…","sender":"U…"}]}`
with 1–16 exact Slack references. It searches retained jobs, including settled
records outside the recent prompt projection. Current configured team/channel
access and every supplied source field must match. Missing or mismatched records
return `unknown` without text. Each attempt separates processing, actual outbox
publication (including uncertain/multipart sends), and any last observed Instance
receipt. Inspection is local and read-only; it never prompts, resends or creates work.

When an endpoint advertises the existing Instance source-delivery profile with
`ordinaryPrompts` and `workReferences`, actual response prompts carry the original
source revision/time and stable job input ID. The existing Conversation grant
establishes the actor and audience. Quiet participation checks and scheduled work
continuations do not redeliver the original source. The connector retains a
bounded receipt snapshot on its existing job after a terminal response; a lost
transport remains unknown. Multiple work references are preserved, including
`proposed` records that are not accepted work. Snapshots expose their observation
time and do not claim current work state. A retained `receipt_query` supplies
the exact existing host method and source/session arguments for authorized
inspection, never replay. Old jobs without a snapshot stay unknown;
unadvertised endpoints stay ordinary ACP and report unsupported linkage.

The profile's authoritative shape and retention limits belong to
[Instance's harness interface](https://github.com/microsoft/amplifier-constellation-instance/blob/main/docs/harness.md).
A terminal response, a published Slack message and completed work remain separate
under the umbrella [work-continuity agreement](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/work-continuity.md).
