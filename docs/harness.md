# Public progress and source continuity

After the agent chooses to respond, the connector maintains one working line in
that conversation. Public model commentary can update it, alongside tool and
compaction activity. Commentary is throttled, bounded and rendered as plain text;
private reasoning, tool arguments and opaque provider state are never status
labels. A commentary-only response is not promoted to a final answer.

When the Instance advertises source-delivery v1, active updates and attention
use the same source ID, revision and original timestamp. The Instance's verified
delivery facts inform its social judgment. They do not grant task authority,
prove completion or automatically suppress another reply. An ordinary ACP agent
without this capability retains the existing input path.

See the [family harness contract](https://github.com/microsoft/amplifier-constellation/blob/main/contracts/harness-lifecycle.v1-candidate.md).
