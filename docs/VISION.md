# A useful teammate in Slack

Work with a continuing agent where the team already coordinates: a direct
conversation, focused thread or shared channel. Invite it into useful work,
correct the direction and receive results without managing another stream of chatter.

This connector supplies the [Connection](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/connection.md)
into Slack. Slack provides the human Experience and remains the source of its
Conversations. The connected agent keeps its own identity and internal design;
one engine, an inner team or another architecture can meet the same ACP boundary.
The connector owns Slack authorization, source access, routing and delivery.
Its supported context and client-tool capabilities let the agent use what the
connection permits without turning the connector into the agent itself.

For example, ask for a recommendation in a project thread. The agent uses permitted
context, asks a consequential question there and returns the result to that thread.
A colleague can continue in Slack's own interface while another supported experience
shows the same conversation. Broader coordination can use shared services without
making the team follow every exchange.

Reuse Slack's conversation structure and keep setup self-contained. Other clients
can use the connector's controls without adopting its administration pages. Neither
those pages nor the supplied stdio process arrangement defines every future
experience or agent architecture.

Judge useful contributions, correct destination and audience, and the total effort
to authorize, direct and recover work. Reconsider mechanisms that add more
configuration or coordination than they remove.

The [Constellation vision](https://github.com/microsoft/amplifier-constellation/blob/main/docs/VISION.md)
owns the wider ambition and five primitives. The [contract map](../contracts/README.md)
links shared promises and this component's Slack-specific interfaces.
