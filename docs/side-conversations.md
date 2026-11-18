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
The opening message asks peers to mention the initiating agent when a meaningful
result or blocker is ready, so ordinary mention-based participation can handle
the handoff. Routine exchanges need no mention.

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

## A private message to the owner

An agent answering in a channel or group conversation can also send its configured
owner a private message with `slack_owner_message`: for example when the owner
asks in a channel to be told something privately, or when a discussion needs the
owner's decision and that should not be said in the room. The reply to the room
remains an ordinary reply.

| Argument | Meaning |
| --- | --- |
| `request_id` | A new UUID for a new message; the same one to check a message already asked for. |
| `text` | The message, in standard Markdown, at most 8,000 characters. |

The connector fixes the recipient: the owner Slack ID in the connection's context
settings, in that person's one-to-one conversation with the agent, which the
connector opens if it does not exist. No argument names a person or conversation,
and a call carrying any other argument is refused. Each message ends with a link
to the message that prompted it.

The tool is listed only for a response to a person in a group conversation, when
an owner is configured and direct messages are enabled for the connection. A
response to another agent's message does not have it. Each call checks
with Slack that the source message is unchanged and that the owner is a member
of the source conversation, so a private message tells the owner nothing they
could not already read there. In a direct conversation with someone else, or in
the owner's own, the tool is absent and a call is refused. A connection limited
to listed conversations must list the owner's direct conversation. One response
can send at most three such messages.

Delivery uses the same journal and publisher as other messages. Repeating a
`request_id` reports the saved message and its receipts, whatever text the
repeat carries; it never sends again. A send whose acknowledgment was lost stays
uncertain until Slack's history confirms it. A changed or deleted source cancels
what has not been sent. A refusal says why in words meant for the agent, for
example that the app was not granted `im:write`.

A message Slack accepted is not proof that the owner read it. The message appears
in the owner's direct conversation as the agent's own; a later reply there starts
that conversation's ordinary turn with its own session.

See [conversation continuity](conversation-continuity.md) for source grants and
[validation](validation.md) for the actual-adapter fixture. The umbrella owns the
[shared attention and interaction promises](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/interaction.md).

Slack documents [thread notifications](https://slack.com/help/articles/115000769927-Use-threads-to-organize-discussions)
and [thread delivery](https://docs.slack.dev/reference/methods/chat.postMessage/).
