# Find the agreement for your change

This map does not duplicate the shared promises. Start with the Constellation
[family agreement](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/family.v1-candidate.md)
for ownership and composition, and the [interaction agreement](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/experience.v1-candidate.md)
for source, audience and destination. Then follow only the boundaries relevant to
your outcome in the [shared contract map](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/README.md).
Shared documents keep their stated acceptance scope; a candidate is not silently
ratified by a link here.

## Local homes

| If your work changes… | Authoritative Slack home |
| --- | --- |
| Purpose and design space | [Vision](../docs/VISION.md) |
| Conversation/session routing, delivery and management API | [Architecture](../docs/architecture.md) |
| Slack authorization, renewal and app association | [Onboarding](../docs/onboarding.md) |
| Approval, client tools, private questions and sign-in | [Client capabilities](../docs/client-capabilities.md) |
| Slack placement of speech and optional attention | [Harness integration](../docs/harness.md) |
| Temporary work indicators and cleanup | [Work status](../docs/work-status.md) |
| Authorized conversation retrieval | [Conversation continuity](../docs/conversation-continuity.md) |
| Slack files and ACP media | [Media](../docs/media.md) |
| Collaboration and delivery waits | [Collaboration waits](../docs/collaboration-waits.md) |
| Saved connection lifecycle or source updates | [Saved connections](../docs/saved-connections.md), [updates](../docs/updates.md) |

These documents own Slack-specific mappings and exact interfaces; they do not
redefine shared task authority, attention, public speech or recovery intent.
In particular, [session routing](../docs/architecture.md#conversationsession-routing)
labels current restoration fallback as implementation behavior, not a new
preservation promise.

## Change a boundary deliberately

Use [Converge](https://github.com/microsoft/amplifier-converge) and the umbrella
[working guide](https://github.com/microsoft/amplifier-constellation/blob/main/docs/working-together.md).
Keep a shared promise in its shared home and an exact local API in its serving
component. Link rather than copy. Guides and examples explain those homes and
must not be the only place a commitment exists.

Inspect the relevant implementation and test a useful combined outcome instead
of maintaining a second status or gap inventory. [Validation](../docs/validation.md)
explains local checks and what they cannot establish. This map neither advertises
an unimplemented capability nor authorizes installation changes.
