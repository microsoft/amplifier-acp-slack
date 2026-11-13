# Slack participation and delivery

For Slack installation owners, ACP endpoints and clients using this connector.
The promise: an independent agent can participate through Slack while respecting
Slack's source, audience and delivery boundaries. It implements the umbrella
[Connection](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/connection.md)
role; the contract below adds Slack-specific obligations.

**Example:** A reply to a project thread remains in that thread after a person
switches views. If Slack accepted a message but its response was lost, the
connector reports uncertainty and reconciles that message rather than posting
another copy.

## Local commitments

1. **Keep the Slack installation independent.** It owns the Slack app association,
   credential renewal, conversation projection, inbox/outbox and session references.
   The ACP endpoint owns agent identity and its internal execution, including
   provider authentication and history where applicable. Importing an agent command
   is setup convenience, not a dependency on the supplied Constellation Instance
   host or a fleet service. [Setup](../docs/onboarding.md) owns workspace authorization
   and renewal details.
2. **Route by the actual Slack source.** Workspace, channel and thread identify a
   conversation context. Main-channel discussion and individual threads remain
   distinct; one context has at most one active turn while separate contexts may
   progress independently. Verify current source revision, deletion and connection
   scope before sending. [Architecture](../docs/architecture.md#conversationsession-routing)
   owns the exact routing and documents current session-load fallback; fallback
   is not proof that the original session or effects were recovered.
3. **Expose only authorized Slack context.** Source retrieval remains scoped to
   the configured connection and audience. A local conversation projection is
   not a workspace archive. An endpoint's tools or recognized identity do not
   grant access to other Slack conversations. Exact source interfaces and grant
   checks belong to [conversation continuity](../docs/conversation-continuity.md).
4. **Preserve Slack delivery evidence.** Permanent speech, temporary work status
   and unpublished drafts remain distinguishable. Multipart replies preserve
   thread and order with a receipt per part. Confirmed parts are not repeated;
   attempted sends with ambiguous outcomes stay uncertain. Source edits or revoked
   scope can stop remaining parts. [Delivery](../docs/architecture.md#long-replies)
   and [work status](../docs/work-status.md) define Slack-specific limits and cleanup.
5. **Keep control authority explicit.** Slack decisions arrive through the verified
   app connection; management uses separate scoped access. Credentials stay outside
   prompts and view state. File/terminal tools, permissions, private questions and
   media use their declared authority and platform constraints, defined in
   [client capabilities](../docs/client-capabilities.md) and [media](../docs/media.md).

Shared attention, work authority, continuity and outcome meanings remain in the
[umbrella agreements](README.md); this contract does not redefine them.

**Check:** Exercise direct messages, a main channel and two threads; edit/delete a
source during multipart delivery; lose a send acknowledgment; revoke context
access; compare Slack and authenticated management decisions. Verify actual
thread, audience and posted-message identity. [Validation](../docs/validation.md)
distinguishes disposable fixtures from real Slack authorization and delivery.

**Left open:** Product layout, agent internals and collaboration strategy.
The current one-active-turn-per-Slack-context routing is a connector choice,
not a limit on workers or sessions inside the agent.
Supported optional capabilities are negotiated with the endpoint. The connector's
admin UI is one client of its controls.
