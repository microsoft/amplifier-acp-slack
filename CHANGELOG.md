# Release notes

## 0.9.0

- New human messages preserve in-flight approvals, completed effects and final replies. Source retraction, explicitly stopped jobs and policy revocation still apply.
- Compatible ACP agents receive ordered mid-turn input through negotiated steering. Other agents retain each busy input for a later ordinary prompt without cancelling active work.
- Durable input receipts, scoped error notices and conservative recovery prevent automatic replay after ambiguous delivery. Ambient fallback reuses only an explicitly restored, unexpired attention ticket.
- Continuations retain their original budget and source authority; queued or unresolved input prevents automatic continuation.

See [steering behavior and compatibility](docs/steering.md). This release does not infer privileged cancellation commands from ordinary message text.
