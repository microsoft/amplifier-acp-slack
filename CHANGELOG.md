# Release notes

## 0.17.0

Native downloads include a private setup runtime and a first-account browser flow for Windows, Mac and Linux. Independent native installations can check published bundles and restore their previous executable without rewinding credentials or conversation state. Existing Linux PAM/HTTPS and enrolled update ownership remain unchanged. See [native installation](docs/native-installation.md).

## 0.12.0

Use the official ACP Rust SDK for protocol connections, preserving Slack policies and negotiated extensions. See [release notes](docs/releases/0.12.0.md).

## 0.11.4

- Preserve a fresh attention candidate when peer messages arrive while an earlier participation decision is still running, before any ACP prompt is dispatched. Bursts coalesce and remain subject to ordinary Instance attention.
- Recover this handoff after a restart without replaying attempted or ambiguous prompts. Edited, deleted, or stopped sources remain ineligible.
- Distinguish a changed attention snapshot from an actual host hold in private delivery diagnostics.

## 0.10.2

- Mid-turn updates send only the new source and its conversation/authority information. Full history remains available to attention judgment and context tools instead of being repeated on every message or reaction.
- Connector recovery notices carry structured control metadata, so peers do not treat them as new work or awareness. A detailed notice replaces the temporary work indicator instead of producing a second generic error.
- Explicit thread reads still include a referenced error notice as their root. See [release details](docs/releases/0.10.2.md).

## 0.9.1

- Compatible Instance installations can push peer replies as awareness into the current authorized turn. Peer input preserves the original requester and reply location, and never starts an idle turn or claims a new attention slot.
- Agent-facing context excludes progress, permission and question controls while keeping the human Slack indicators visible. Bounded history backfill prevents controls from displacing the requested conversation messages.
- Pending peer awareness is bounded by count and bytes. Overflow stays in source history; explicitly remote-queued input without incorporation remains unknown and is never replayed.
- Private delivery traces connect source message IDs, input receipt stages, final context coverage and outgoing replies. Final results survive late peer arrivals; this release does not guarantee a final model reconsideration or add extra model calls.

## 0.9.0

- New human messages preserve in-flight approvals, completed effects and final replies. Source retraction, explicitly stopped jobs and policy revocation still apply.
- Compatible ACP agents receive ordered mid-turn input through negotiated steering. Other agents retain each busy input for a later ordinary prompt without cancelling active work.
- Durable input receipts, scoped error notices and conservative recovery prevent automatic replay after ambiguous delivery. Ambient fallback reuses only an explicitly restored, unexpired attention ticket.
- Continuations retain their original budget and source authority; queued or unresolved input prevents automatic continuation.

See [steering behavior and compatibility](docs/steering.md). This release does not infer privileged cancellation commands from ordinary message text.
