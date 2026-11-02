# Move the same individual to another host

The adapter and the agent can run on different machines. The ACP command runs
locally in the adapter's client directory; `acp.cwd` is the absolute working
directory **on the agent host**. A copied attach client can connect to a remote
service without sharing its filesystem or provider credentials.

Ordinary configuration edits deliberately refuse changes to a populated
connection's Slack identity or ACP endpoint. An operator moving the same
individual can explicitly relocate only its working directory:

```sh
acp-slack-adapter --home /path/to/adapter config relocate-workspace \
  --cwd '/new/agent/workspace' --expected-revision 4 --same-agent \
  --reason 'Verified the same instance identity, migrated history, and remote connection'
```

Stop this adapter first. Complete or reconcile pending work, approvals, and
uncertain deliveries before moving it. Independently verify the agent identity
and copied state, and update any remote attach-client address and certificate
trust using that client's supported mechanism. Then run this command, restart
the adapter, and use `check --target all` to verify ACP session creation and the
Slack Socket Mode connection. The check does not send a prompt or Slack message.

Relocation retains the Slack identity, credentials, ACP executable, arguments,
environment, messages, delivery receipts, and existing session bindings. It
changes only the working directory and configuration revision, updates matching
session-binding keys, and records the operator's reason in Activity. Concurrent
service use, stale revisions, or unresolved work are rejected. An interrupted
configuration/database commit leaves a `workspace_relocation_started` event for
inspection; keep the service stopped until the configuration and bindings agree.

Base ACP does not provide a universally verifiable persistent individual ID.
`--same-agent` records an explicit operator attestation, not protocol-level proof
of identity. Moving a different individual into this history still requires a
separate adapter installation.
