# A useful teammate in Slack

People should be able to work with a persistent agent where their team already
coordinates: a direct conversation, a focused thread or a shared channel. Invite
it into useful work, correct the direction and receive results without managing
another stream of chatter.

Slack gives that collaboration its setting. A mention, thread, private dialog and
workspace approval have meanings the connector must respect. This component
translates those meanings between Slack and an independently chosen ACP agent.
It owns the Slack connection, authorized source access, presentation and delivery;
the agent endpoint owns reasoning and its execution history.

For example, a team asks for a recommendation in a project thread. The agent uses
permitted context, asks a consequential question there and returns the result to
that thread. People can see what arrived and what remains unresolved. Broader
coordination can use the ecosystem's shared services without making Slack users
follow every exchange.

Our design choices are to reuse Slack's conversations, keep the connector
independently installable, and make its controls available to other clients.
The packaged administration pages do not prescribe the future product experience.

Judge the connection by useful contributions, correct audience and destination,
and the total effort to authorize, direct and recover the work. Reconsider a
mechanism when it creates more configuration or coordination than it removes.

The [Constellation vision](https://github.com/microsoft/amplifier-constellation/blob/main/docs/VISION.md)
owns the wider ambition. [Shared agreements and the Slack boundary](../contracts/README.md)
define what others can build on; exact APIs and platform setup stay in their
technical documents.
