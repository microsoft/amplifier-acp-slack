# Slack's boundary and shared agreements

Start with the [vision](../docs/VISION.md) and the local
[Slack participation and delivery agreement](connector.md).

This repository implements the umbrella's [Connection](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/connection.md)
role. Slack supplies the [Experience](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/experience.md) and owns
its [Conversations](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/conversation.md). The connected
[Agent Instance](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/agent-instance.md) keeps its internals open;
capabilities it can invoke follow the [Tool](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/tool.md) boundary
where supported. These roles describe interfaces, not five required processes.

Shared promises have one home in the umbrella:

| Your change concerns… | Shared home |
| --- | --- |
| Identity, audience, ownership and reuse | [Composition](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/composition.md) |
| Attention, coordination and result return | [Interaction](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/interaction.md) |
| Assigning, delegating or stopping work | [Work authority](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/work-authority.md) |
| Input, interruption and recovery | [Work continuity](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/work-continuity.md) |

For this supplied connector, also apply the scoped [installation](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/installations.md)
and [protocol compatibility](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/engine-compatibility.md) agreements.
Optional shared attention follows [ACP hosting](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/profiles/acp-hosting.md)
when negotiated. These implementation choices do not define every agent's internals.

## Exact Slack interfaces

[Architecture and management](../docs/architecture.md) ·
[authorization](../docs/onboarding.md) ·
[client tools and questions](../docs/client-capabilities.md) ·
[source context](../docs/conversation-continuity.md) ·
[speech integration](../docs/harness.md) ·
[work status](../docs/work-status.md) ·
[media](../docs/media.md) ·
[collaboration waits](../docs/collaboration-waits.md) ·
[saved connections](../docs/saved-connections.md) ·
[updates](../docs/updates.md).

These technical documents describe supported shapes, settings and implementation
limits. They do not redefine the shared promises. Use [validation](../docs/validation.md)
to check the combined outcome; documentation and fixture results alone do not
establish live support.
