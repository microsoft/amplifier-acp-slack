# Conversation continuity

The same agent can participate in several Slack conversations while each room
keeps its audience. Main messages and threads need enough orientation to match
what users see in Slack. Direct messages and explicit mentions normally expect
a response; ambient group participation can remain selective.

## Ownership boundaries

| Component | Responsibility |
| --- | --- |
| Slack | Conversations, membership, messages, threads and platform permissions. |
| Slack adapter | Platform credentials, event ingestion, delivery, verified audiences and bounded source retrieval. |
| Agent | Individual identity, behavior, execution and authorized use of connected sources. |
| ACP | Work/session transport and standard MCP configuration. |
| Optional context contract | Surface-neutral retrieval and explicit audience grants; not a mandatory ACP extension. |
| Browser or Toad experience | Its own conversations and display, with optional authorized source sharing. |
| Future fleet interface | Discovery, management and observation through public interfaces; not a runtime dependency. |

The adapter does not read the agent's private database or native model sessions.
The agent does not read the adapter's database to acquire Slack context. Different
rooms retain independent sessions; context is retrieved when relevant.

## Delivery and orientation

Fresh Slack metadata distinguishes a one-to-one DM from a group DM. An explicit
mention or verified one-to-one DM overrides ambient speak/pass behavior. A stale
ambient pass gets one correction that forbids repeating completed work. An
explicit request for silence may still pass. Budgets, tool permission blocks,
errors and uncertain sends remain visible outcomes.

Orientation includes the current conversation and reply destination, participants,
recent parent messages, thread references, coverage and source links. For an older
thread, a bounded excerpt around the root includes its preceding question where
available. Prompts direct the agent to retrieve referenced context proactively.
The adapter does not claim to know the user's viewport, selection, unread state
or unsent draft.

## Read-only source interfaces

When conversation context is enabled and the ACP peer advertises HTTP MCP, normal
session setup/load supplies a short-lived HTTP MCP descriptor. The adapter exposes
`slack_context_list`, `slack_context_read`, `slack_context_search` and
`slack_context_orientation` at
`/context/mcp`. These tools cannot post messages or change Slack configuration.
Generic ACP agents can use them without adopting Agent Instance's management API.

For cross-experience retrieval, the optional `agent-instance.context/1` provider
uses `/context/v1/query`, authenticated with a dedicated token carrying `context`
access. Register provider `slack:<adapter-id>` in the agent's context registry.
The operations are `authorize`, `list`, `read`, `search` and `orientation`; outputs identify
the source, freshness and completeness. Listings and message windows are bounded;
this release does not promise exhaustive archive search or automatic pagination
through all history.

`search` matches case-insensitive literal text in one authorized conversation or
thread window. Supply `query` (1–200 characters), then follow `next_cursor` to
inspect another window. An empty result can still have a continuation. Matching
uses only the bounded text that `read` would return; coverage retains truncation,
the number of scanned messages, and the original message IDs and source links.
Conversation-root searches do not implicitly search thread replies. Search does
not alter the audience rules or invoke Slack's workspace-wide search API.

Each prompt and the `context_tools_setup` activity record distinguish disabled
context, a peer without HTTP MCP, and a supplied descriptor. Supplying a descriptor
does not verify that the peer registered the tools or successfully authenticated
to the provider; that requires an actual tool result. A resumed session reports
the same distinction rather than assuming its old tools are still usable.

The optional ACP metadata `io.github.bkrabach.conversation` carries a provider ID
and temporary audience grant. It never includes Slack credentials or an adapter
administrator token. Agent Instance validates that grant before exposing its own
cross-provider context tools. The agent adds those tools independently of which
native ACP client is attached, including Toad.

A trusted reachable HTTPS public URL and HTTP MCP support are required for these
context tools. Ordinary ACP work remains a separate capability; a source failure
must be reported as unavailable context rather than evidence of no history.

## Audience checks

- The configured owner Slack ID is an explicit identity link. Only a verified
  one-to-one DM from that user receives owner-private authority. Another person's
  DM and group DMs do not acquire it through a display name or channel-ID prefix.
- Owner retrieval requires current access for both the owner and bot. Another
  person's DM is excluded. With no owner link, the adapter can still supply a
  grant for the current room but cannot authorize owner-wide retrieval.
- A shared room's grant is restricted to that room, including its parent and
  threads. Private browser, Toad and DM content is unavailable in that audience.
- Slack metadata/membership is checked on retrieval. Unavailable permission
  evidence denies access. Grants are random, stored hashed, expire after thirty
  minutes and become invalid when the configuration revision changes.
- Agent Instance binds resumed context to its recorded audience. A legacy session
  with no audience record is rebuilt from the authorized Slack projection. A
  private owner session cannot be reused as a group session.

These are API and working-context boundaries, not hostile-code isolation between
processes running as one OS account. Backend executables and their native tools
must be trusted or independently constrained. Revocation cannot erase text already
incorporated into a backend's working history.

## Verification

Deterministic tests cover room isolation, owner linking, group DMs, parent/thread
navigation, missing membership, expiry/revocation, resume and explicit silence.
The [opt-in continuity probe](continuity-check.md) can check a separately
configured installation. No private deployment transcript is included here.
