# Codex through standard ACP

The Slack adapter accepts the ordinary
[`@agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp)
entry point. Configure its command, arguments and working directory as for any
other ACP agent. Codex authentication and tool policy belong to that endpoint.
Constellation Instance and its optional profile/management APIs are not required.

The adapter negotiates capabilities. It retries an initialization failure once,
before creating a session or sending any work; a failed prompt is never retried
through that path. Keep independent agent installations and account state in
their own folders.

## Where compatibility ends

| Capability | Current boundary |
| --- | --- |
| Text chat, DMs, mentions | Standard ACP requests plus Slack delivery instructions; no agent-specific response schema needed for addressed chat. |
| Session load | Used when advertised. Otherwise the adapter creates a fresh session and supplies bounded Slack history. The no-load case is verified with an independent fixture. |
| Agent-side tools | Work under the endpoint's own permissions; Codex file tools were exercised. |
| Client filesystem/terminal tools | Added as opt-in local capabilities in 0.3.0; see [client capabilities](client-capabilities.md). The original 0.2.2 Codex tests below used agent-owned tools. |
| Interactive approvals | Single-use Slack decisions and private detail views for the initiating human, plus owner dashboard/API decisions. Set `acp.client.approvals_required=false` for audited automatic `allow_once` decisions; no separate approver list is needed. |
| Ambient judgment | Prompted speak/pass JSON convention. Tested examples passed; this is not an ACP feature or proof of reliable social judgment. |
| Cross-room retrieval | Optional, separately authorized context/MCP integration. Requires explicit configuration; ACP alone supplies no shared memory. |
| Attachments and proactive posting elsewhere | Supported media is described in media.md; replies stay attached to the originating conversation/thread. |
| Lifecycle/fleet management | Separate connector API and agent provisioning. No assumption that arbitrary ACP agents implement our management interface. |

## Repeat an opt-in check

Create an ACP config containing the endpoint's standard `command`, `args`, `env`
and absolute `cwd`, then run:

```sh
python3 tests/live_agent.py \
  --acp-config /absolute/path/to/acp.json \
  --expected-name Codex --allow-model-call \
  --binary target/release/acp-slack-adapter \
  --tool-check write --group-check \
  --output evidence/codex-compatibility.json
```

Use `--tool-check read` for a read-only endpoint. This check makes model calls
but sends no messages to actual Slack. It creates and removes only uniquely
named fixture files inside the configured working directory. To test cold native
state, point `CODEX_HOME` to a new private temporary directory containing a copy
of the authorized account cache and config, with no existing databases or sessions.
Do not delete a real agent's state to simulate a fresh installation.
