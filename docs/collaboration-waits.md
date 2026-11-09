# Collaboration waits

The connector preserves completed replies in its durable outbox while waiting for
reply spacing or hourly capacity. It retries delivery after the limit clears;
it does not rerun the completed ACP prompt or repeat its tool actions. Model
capacity similarly defers the pending job with a durable availability time rather
than permanently dropping it. Recovery still treats interrupted model turns and
uncertain sends as requiring reconciliation, not permission to replay work.

Settings includes **Agent collaboration**. Owners may allow agents to continue
conversing after a short pause without waiting for another human message. This
sets `attention.bot_exchange_cooldown_seconds` to 60. Zero preserves the existing
wait-for-a-person policy; allowed nonzero values are 30–3600 seconds. Hourly reply
and model budgets still apply. This is an optional conversation policy, not an
execution grant or an unlimited background-work authorization.

New bot messages do not discard a completed contribution solely by advancing the
conversation version. New human input and edits/deletions of existing messages
still cause reconsideration. Editing the root of a thread invalidates its snapshot.
The completed work is not rolled back or blindly repeated. The conversation's
read-only response-delivery tool reports pending state, the reason for a delivery
wait, and a retry time when one is scheduled.

Every setting remains available through the configuration API/CLI. The browser's
`collaboration.toggle` semantic action changes the same durable draft as the
visible control; Save preferences applies it. Existing installations keep the
human gate until the owner opts in. No agent identity, ACP endpoint, permission,
channel scope, or secret is changed by this preference.

## Continuing accepted work

When work continues in a background process, the agent should schedule a check
before ending its response. `slack_follow_up` exposes the existing continuation
through the conversation's MCP server and returns a durable receipt. It takes
`after_seconds` and `goal`; zero seconds cancels this turn's proposal. Calls are
bound to the active response's grant, cannot target another job or conversation,
and an identical retry preserves the original wake time. A promise to return is
not a schedule. The next turn inspects the existing run and reports a meaningful
result or failure, rather than launching the work again.

Plain ACP endpoints without these tools can use the existing private final line:

```text
SLACK_FOLLOW_UP {"after_seconds":60,"goal":"Verify the already-created artifact"}
```

This is a prompt convention over ordinary ACP; there is no proprietary ACP method
or required shared scheduler. Both entry points record the same single durable
continuation. The connector strips the compatibility line and queues the check
after a confirmed response terminal, including a quiet pass. Public delivery is
independent: pending or uncertain sends remain accurately represented in the
next turn's delivery receipts and must not be blindly resent. A failed or crashed
turn does not activate its proposal.
The original human, conversation, thread, endpoint, and session binding are kept.
A fresh source-message and Slack membership check runs before the continuation.
Reading another room does not grant a return route there.

Delays are 5–3600 seconds, at most eight follow-ups in 24 hours per human request,
with the installation's existing model/reply budgets also enforced. These turns
are for concrete progress on an accepted goal, not keep-alive chatter. Bot-origin
messages cannot schedule this chain. New human input, source changes, scope or
endpoint changes, and expiry supersede/cancel queued work. A new human turn must
reassess outstanding work rather than silently dropping it: the existing compact
work handoff retains a superseded result-check goal for that reassessment. An
explicitly withdrawn follow-up is not carried forward. No model is assumed
to obey this convention perfectly; deterministic protocol tests and actual-model
behavior are separate evidence.

The continuation control is offered only on human-origin turns. If a peer-origin
response nevertheless includes one (for example from older session instructions),
the connector strips it and records `continuation_not_scheduled` with reason
`bot_origin` privately. It does not append a scheduling notice to the agent's
public contribution or turn a pass into speech. This changes presentation, not
continuation eligibility, authority or budgets.

Routine delivery verification and work-record closure stay internal. A useful
result, changed plan, question or actionable blocker belongs in the conversation;
a successful bookkeeping operation alone does not require a message. These are
generation instructions, not keyword filters that delete model-written replies.

Completed ACP results are saved before delivery processing. Restart can finish a
saved response without rerunning its tools. A Slack text send with a lost receipt
is reconciled against the original client ID, including individually delivered
multipart chunks. An absent result in bounded history is not proof of non-delivery.
Unconfirmed media completion or a crashed prompt without a completed result stays
explicitly unresolved; neither is blindly rerun. The source conversation receives
a failure/uncertainty notice where Slack still permits writes. This does not claim
exactly-once external tool execution across arbitrary backend failures.

`max_turn_requests` and `max_tokens` are known prompt terminals, not transport
failures. For accepted human work, the existing continuation can inspect completed
results and continue, within the same limits above. This requires complete terminal
tool observations and no unresolved client operation or incoming steering. Partial
answer text is not published as a result. Cancellation, refusal, incomplete tool
observations and unknown outcomes never automatically repeat work.

A host advertising attention `noForwardRejectionVersion: 1` can attest that a
particular offer was not forwarded. Only an exact offer/session match in the
reserved error data changes the public explanation to “not forwarded”; it does
not authorize retrying the offer or mark the source fulfilled. Unmarked errors,
including the same numeric code, retain the uncertain-outcome treatment.
