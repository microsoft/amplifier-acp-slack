# Opt-in live continuity check

This operator tool reads the selected Slack conversation and asks the installed ACP agent one question. Its answer is captured in a private local file. It does not post a Slack message, ingest a synthetic message, or reuse the live conversation's session binding.

Build on the adapter host:

```sh
cargo build --locked --release --example continuity_check
```

Put the question in a private UTF-8 file (`chmod 600`). Then run:

```sh
./target/release/examples/continuity_check \
  --home /home/operator/.local/share/acp-slack-adapter/helper \
  --channel D_YOUR_EXISTING_DM \
  --prompt-file /private/path/question.txt \
  --output /private/path/result.json \
  --allow-model-call
```

Add `--thread TIMESTAMP` to inspect an existing thread. The output file must not already exist. Without `--output`, results go into the installation's `validation/continuity-UUID.json`. The terminal prints only outcome/count metadata; the private JSON contains the actual final answer and operational reason. Prompt files must have no group/other access; newly written results use mode `600` on Linux. Existing parent directory permissions are preserved.

`--allow-model-call` is mandatory and is checked before filesystem or network access. Context must be enabled and the connector must have a configured Slack owner. That owner is used as the explicitly synthetic operator identity, never represented as a newly received Slack event. Fresh Slack metadata and membership checks run before history collection and the model call. Group probes retain group scope even when their operator is the configured owner.

Each probe uses a fresh `validation:UUID` binding. The normal driver can create private grant/session/audit records and the underlying agent can persist its ACP session. The probe does not call the outbox, publisher, ingestion, or service lifecycle paths. The driver is invoked once; its ordinary direct-response correction may make a second prompt if the agent incorrectly chooses group silence.

The synthetic request explicitly restricts the agent to read-only context discovery/retrieval, forbids actions and alternate access through files or native session stores, and asks for sources. These instructions are not a new operating-system isolation mechanism; existing endpoint tool policy still applies.

Useful evaluations:

1. Introduce a distinctive fact through the agent's web conversation. Ask about it from an owner-DM probe without putting the fact in the probe question. Inspect whether the answer retrieves the correct source.
2. Ask the same question from a shared-channel probe. Private owner context must remain unavailable; the answer should report that boundary rather than obtain the fact through another route.
3. Select an existing DM thread and ask about the question preceding its parent reply and the latest parent-conversation message. Compare the cited sources with Slack.

A completed probe is evidence about an observed model response, not an automatic pass/fail assertion about its accuracy. The deterministic access-control checks remain in `tests/context.rs`.

Offline example checks:

```sh
cargo test --locked --example continuity_check
```
