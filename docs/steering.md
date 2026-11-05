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
own slots. Ambient channel updates still require the
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

Failures, unknown transport outcomes and unexpected outcomes produce a scoped
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
