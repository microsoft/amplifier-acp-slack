# New messages during work

An ordinary new human message no longer revokes a running operation's approval
or withholds its reply. Source edits/deletion, a stopped job and installation
policy changes still apply. The connector does not infer privileged stop commands
from natural-language text; new guidance is delivered to the agent for interpretation.

Busy human inputs have separate durable source records in intake order. Repeated
source revisions are deduplicated; equal text from different messages remains
separate. A queued update retains its channel/thread and author. It cannot enter
another conversation's running session. Status and permission cards remain outside
participant intake. New work does not reset the active prompt's execution limit.

When initialize advertises `_meta.steering.supported`, the connector uses the
optional [ACP steering extension](https://github.com/agentclientprotocol/codex-acp/blob/main/docs/steering.md).
Each request includes `idleBehavior: "promptRequired"` and a stable installation-local
`inputId`. One steering request is outstanding at a time; input can arrive while
tools and approval callbacks continue. Those updates share the active lifecycle's
execution and hourly model-decision budget; deferred ordinary prompts consume their
own slots. Ambient human channel updates still require the
Instance attention offer, and Instance remains responsible for admission. A definite hold remains held;
a failed or uncertain admission evaluation is not silently repeated.

`injected` transfers ownership to the running lifecycle; it does not establish
that the model incorporated the update or completed work. Optional native input
receipts distinguish those states. The original prompt owns final output. If its
terminal response races a steering response, delivery waits for the latter.

`promptRequired` retains the update for an ordinary prompt after the current one
ends. An endpoint without steering uses that queued path directly. Neither path
cancels and restarts the active task. A restored attention ticket is reused only
when the backend explicitly restores that original offer and expiry; it is not
silently renewed. Instance revalidates source, policy and expiry when consumed. Native Instance
handoff across a new transport additionally requires the same authenticated
principal, saved session and verified context source binding. An unbound plain
ACP session fails closed; the connector does not renew its offer.

For human input, failures, unknown transport outcomes and unexpected outcomes produce a scoped
visible error and durable diagnostics. They never trigger blind replay. A backend
that starts a detached turn despite `promptRequired` is disconnected before the
connector accepts additional operations from that unowned lifecycle. Submitted
inputs whose ownership cannot be recovered after restart remain uncertain.
A runtime recovery-required rejection (`-32010`) produces a fixed same-conversation
notice; it does not echo private error details, acknowledge uncertain input or retry.
Review and any decision to proceed belong to the installation's explicit controls.

Automatic continuation remains within its original depth/deadline allowance.
An incorporated update can be followed by a newly requested continuation, but
queued or unresolved input prevents that automatic continuation. Original source
authorization versions are never advanced merely to make a continuation eligible.

The authenticated installation status includes steering state counts and oldest
queued age. The private `steering_inputs` ledger retains source revision, intake
sequence, active job/session and receipt linkage; it does not publish input text
in activity indicators. `tests/steering.py` exercises independent ACP processes
against loopback Slack; `tests/steering.rs`, `tests/followup.rs` and approval/delivery
tests verify durability and the control boundaries. These fixtures do not establish
live model quality or compatibility with every third-party ACP release.


## Peer awareness and human-visible controls

Real peer messages in the same channel/thread may update an already authorized
active turn, including unmentioned replies while that turn is running. They share
the human-input FIFO, source revision checks and execution budget. Each peer input
is bound to that job at arrival and creates no new job, attention offer, progress
indicator or automatic continuation. Idle peer participation still belongs to the
configured Instance attention policy.

This optional path requires both steering support and
`_meta["io.github.bkrabach.agent-instance"].inputContinuity.awareness` advertising
`{ "version": 1, "activeOnly": true, "promptIdRequired": true }`. The connector sends
`_meta.steering.intent = "awareness"` and `activePromptId` equal to the original
active `session/prompt` request ID. Instance binds the update to that exact prompt
on the same connection and revalidates the current context/tool authorization.
The projection keeps the original trigger and human authority; the separately
attributed `awareness_input` is evidence, not a new commission or tool permission.

Unsupported, idle (`promptRequired`) and too-late peer inputs remain normal source
history. They never fall back to a new prompt. Unknown/rejected peer delivery is
recorded privately without another public error message or automatic retry.
`received` does not mean `incorporated`; neither means a report was published.

Structured `acp_work_status`, `acp_permission` and `acp_question` messages are
excluded from intake, history, context reads, search and thread candidates, from
any author and through edit/delete wrappers. Known local control IDs cover later
metadata-free events. History reads request Slack metadata and backfill up to four
upstream pages, preserving the cursor after each fully consumed page. Controls
remain visible in Slack. Human messages that merely say “Thinking” remain evidence.

## Final-response coverage

The original final answer is preserved when newer ordinary conversation arrives.
An outstanding steering response is resolved before final delivery; queued peer
inputs left after completion become `historyOnly`. This release does **not**
guarantee a last model reconsideration: once the final prompt response arrives,
there is no active turn into which a client can safely drain pending deltas.
A future pre-final checkpoint would need explicit protocol negotiation and a
bounded same-turn budget; this connector adds no late prompt or model call.

Private authenticated activity includes `steering_delivery` (input intent,
message/revision ID and receipt stage) and `turn_context_coverage` at turn completion
and delivery claim. Coverage links the original/latest Slack message IDs, prompt
and observed context versions, up to 32 newest input receipts, active job and outbox
ID. Existing delivery receipts link that outbox to the published Slack message ID.
These traces omit conversation text and do not claim unconfirmed incorporation.
`tests/peer_awareness.py` exercises peer bursts, unsupported/idle/unknown outcomes,
control events and the final-response race with independent fixture processes.
