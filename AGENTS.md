# Working on the Slack connector

Use [Amplifier Converge](https://github.com/microsoft/amplifier-converge) and the
Constellation [working guide](https://github.com/microsoft/amplifier-constellation/blob/main/docs/working-together.md).
Start with [this component's vision](docs/VISION.md) and the
[contract map](contracts/README.md), then read only the interfaces needed for the
outcome. The umbrella owns shared promises; this repository owns its Slack
translation and implementation.

- Inspect current source and relevant behavior when choosing work. Do not add a
  maintained feature, release or gap inventory as onboarding context.
- Keep detailed local APIs and tests with their existing owners. Link shared
  agreements rather than copying them. If a shared promise changes, coordinate
  with its affected producers and consumers; routine implementation needs no new
  approval gate.
- Preserve the difference between intended behavior, current implementation and
  verified outcomes. A fixture result does not prove real platform authorization,
  delivery, agent quality or production recovery.
- Treat the packaged browser UI as development and administration access. Future
  product UX remains open; do not derive product constraints from that prototype.
- Keep credentials, personal profiles, transcripts, installation details and
  private experiment evidence outside published source. Do not exercise real
  accounts or installations as an incidental documentation check.
- For a documentation change, check links and inspect moved passages for lost
  commitments. Review linked examples when their promise changes; create no sync
  ledger. For code, use [validation](docs/validation.md) and the applicable CI
  checks, including meaningful cross-boundary tests when behavior changes.

Existing repository permissions and task ownership still govern publication and
operations. These instructions grant no new deployment or external-account access.
