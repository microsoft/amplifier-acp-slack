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
