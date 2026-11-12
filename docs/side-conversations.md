# Side conversations in Slack

Agents can move routine coordination into an ordinary Slack thread and bring a
useful result back to the conversation that requested it. People can open the
thread, read the attributed discussion and reply using Slack's usual controls.
These are discussion threads, not Slack's audio Huddles.

The initiating agent creates or reuses a bot-started thread in the same channel.
The first message explains the purpose and links the source. Routine replies stay
in that thread with `reply_broadcast: false`. A separate root avoids automatically
subscribing the person who wrote the original request to every coordination reply.
Slack still owns notifications for participants, mentions, followed threads and
personal preferences; the connector does not promise silence for every viewer.

The agent brings meaningful progress, decisions, results or a blocker needing
human input back to the recorded original channel or thread, with a link to the
discussion. It should not repeat the side conversation, narrate delivery checks
or add a second acknowledgment after the result is already retained for delivery.
When people are actively collaborating or brainstorming, ordinary conversation
remains appropriate. No classifier or additional participation approval is added.

## Tools and routing

The existing granted Slack MCP connection advertises these tools for active jobs
with channel-wide source access:

| Tool | Purpose |
| --- | --- |
| `slack_huddle_ensure` | Create or reuse this source's side thread; supply a purpose and optional actual Slack peer user IDs. |
| `slack_huddle_status` | Inspect the retained thread and delivery receipts. |
| `slack_huddle_post` | Send coordination to that thread using a stable request UUID. |
| `slack_huddle_result` | Return a meaningful `progress`, `result`, `blocked` or `decision` message to its recorded origin, using a stable request UUID. |

Destinations come from the retained source and mapping, not model-supplied channel
or thread arguments. Existing Slack context tools read the discussion. Each
thread retains its ordinary ACP conversation/session; it is not silently aliased
to the original channel session. Peer statements keep their attribution and do
not become a new human request or work grant.

Creation and reuse are durable within the initiating installation and exact
source. Participants should use the linked thread rather than each creating a
new one. This does not claim a workspace-wide atomic coordinator across unrelated
bot installations. A new request is not silently attached to unrelated work.

Direct messages and explicitly thread-restricted grants do not authorize creating
a sibling channel thread. The tools are unavailable for those scopes. A private
channel remains that same private channel; no public-channel fallback is used.
If the capability is unavailable, the agent remains in its authorized conversation.

## Delivery and recovery

Side messages use the connector's existing durable public-message journal and
publisher. The source, its revision, current audience and connection scope are
checked before delivery. Creation and result acknowledgments retain the original
message identity. An ambiguous send stays uncertain and is reconciled from Slack;
it is not submitted again under a new ID. Reusing an ID with changed content is
refused. Confirmed delivery means Slack accepted the message, not that a person
read it or that the described work was independently verified.

Disconnection, revoked access and changed/deleted sources continue to use the
existing controls. Reconnecting does not invent a replacement request, revive
expired work or turn an unknown effect into a successful one.

See [conversation continuity](conversation-continuity.md) for source grants and
[validation](validation.md) for the actual-adapter fixture. The umbrella owns the
[shared attention and interaction promises](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/interaction.md).

Slack documents [thread notifications](https://slack.com/help/articles/115000769927-Use-threads-to-organize-discussions)
and [thread delivery](https://docs.slack.dev/reference/methods/chat.postMessage/).
