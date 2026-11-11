# Agents that participate usefully in Slack

People should be able to work with a persistent agent where their team already
coordinates: a direct conversation, a focused thread or a shared channel. The
value is useful work and follow-through with enough context to understand and
trust it, without making everyone manage another stream of agent chatter.

Slack gives this connection its social and access boundaries. An invitation,
mention, thread, private dialog and workspace approval each mean something to
the people using them. This connector should translate those meanings faithfully
between Slack and an independently chosen agent, and make delivery and uncertainty
understandable in the place where the work began.

## What this component contributes

The connector owns Slack authorization and renewal, verified source events,
conversation routing, scoped source retrieval, Slack interaction controls, media
transfer and evidence of Slack delivery. It exposes installation controls so
other experiences can manage a connection without recreating Slack setup.
The agent's reasoning, identity, durable work and tools belong to the selected
ACP endpoint. The family [responsibility boundaries](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/family.v1-candidate.md)
keep those services independently useful.

A team should be able to deepen this Slack integration while another team builds
better reasoning, conversation services, a personal assistant or a new product
experience. New ideas can change how these pieces work together; existing
prototypes do not bound that ambition. The packaged browser pages are development
and administration access, not a commitment to the eventual product UX.

## How to shape the next outcome

Start with a person's intended collaboration and the Slack setting. Use the
[contract map](../contracts/README.md) to find existing promises and their owners,
then inspect relevant implementation and behavior. Reuse services and remove
unnecessary coordination or configuration before adding a new mechanism. If an
idea changes a shared promise, shape it with the affected owners in the umbrella;
keep Slack-specific choices here.

Judge the outcome by the human effort to set up, direct, inspect and correct the
work, as well as whether useful results reach the right people. General interaction
and human-attention expectations live in the [Constellation vision](https://github.com/microsoft/amplifier-constellation/blob/main/docs/VISION.md)
and agreements, not a parallel Slack specification.
