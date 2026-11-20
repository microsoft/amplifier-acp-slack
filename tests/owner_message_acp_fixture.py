"""Deterministic ordinary ACP endpoint; owner-message calls use the real granted MCP.

No model, production credential, external network, or fake adapter grant is used.
The parent fixture controls only Slack's loopback service and human input.
"""

import hashlib
import json
from pathlib import Path
import re
import ssl
import sys
import time
import urllib.error
import urllib.request
import uuid


ROOT = Path(sys.argv[1])
NAME = sys.argv[2]
SERVER = None
TOOL = "slack_owner_message"


def record(kind, **fields):
    with (ROOT / (NAME + "-acp.jsonl")).open("a") as log:
        log.write(json.dumps({"kind": kind, **fields}) + "\n")


def output(value):
    print(json.dumps({"jsonrpc": "2.0", **value}), flush=True)


def rpc(method, args):
    assert SERVER["url"].startswith("https://127.0.0.1:")
    headers = {h["name"]: h["value"] for h in SERVER["headers"]}
    headers["Content-Type"] = "application/json"
    request = urllib.request.Request(
        SERVER["url"],
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": args}).encode(),
        headers=headers,
    )
    try:
        with urllib.request.urlopen(request, context=ssl._create_unverified_context(), timeout=30) as response:
            value = json.load(response)
    except urllib.error.HTTPError as error:
        record("tool_rejected", tool=args.get("name"), status=error.code)
        return {"http": error.code}
    record("mcp", method=method, arguments=args, result=value)
    return value.get("result", {"rpc_error": value.get("error")})


def tool_list():
    """The names this response was offered, and one hash of every definition, so that
    two responses can be compared for the same list exactly."""
    tools = rpc("tools/list", {}).get("tools", [])
    return {"names": sorted(tool["name"] for tool in tools),
            "sha256": hashlib.sha256(json.dumps(tools, sort_keys=True).encode()).hexdigest()}


def call(**args):
    return rpc("tools/call", {"name": TOOL, "arguments": args})


def mark(case, **fields):
    (ROOT / (case + ".json")).write_text(json.dumps(fields))


def request_id(case, number=0):
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"owner-message-fixture:{case}:{number}"))


def settled(args, timeout=60):
    """The receipt once it can no longer change: sent, or withheld for good.

    A refused call is returned as it is: the connector stops serving a response
    whose asker can no longer read the conversation. A call that fails because
    Slack is unavailable is asked again.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = call(**args)
        if value == {"http": 403} or value.get("structuredContent", {}).get("status") in ("sent", "cancelled", "failed"):
            return value
        time.sleep(0.1)
    raise AssertionError("Timed out: settled owner message")


def prompt(params):
    text = "".join(block.get("text", "") for block in params["prompt"])
    if "Latest conversation projection:\n" not in text:
        return {"decision": "pass", "reason": "explicit_silence_request"}
    view = json.loads(text.split("Latest conversation projection:\n", 1)[1])
    trigger = view["trigger"]
    record("prompt", session_id=params["sessionId"], trigger=trigger)
    match = re.search(r"OWNER_NOTE ([a-z]+)", trigger["text"])
    if not match or f"<@U{NAME}>" not in trigger["text"] or trigger.get("bot") != match.group(1).startswith("agent"):
        return {"decision": "pass", "reason": "explicit_silence_request"}
    case = match.group(1)
    tools = tool_list()
    offered = TOOL in tools["names"]
    if case.startswith("agent"):
        # Another agent's message started this response.
        mark(case, listed=offered, tools=tools, attempt=call(request_id=request_id(case), text="MUST_NOT_SEND agent"))
    elif case.startswith("seen"):
        # A person's message, used only to compare the tool list with other responses.
        mark(case, listed=offered, tools=tools)
    elif case == "direct":
        mark(case, listed=offered, attempt=call(request_id=request_id(case), text="MUST_NOT_SEND direct"),
             history=[{"sender": m["sender"], "bot": m["bot"], "text": m["text"][:60]} for m in view["history"]])
    elif case == "named":
        mark(case, listed=offered, attempt=call(request_id=request_id(case), text="MUST_NOT_SEND named", user_id="UPEER"))
    elif case == "limit":
        mark(case, listed=offered, results=[call(request_id=request_id(case, n), text=f"PRIVATE limit {n}") for n in range(4)])
    else:
        args = dict(request_id=request_id(case), text=f"PRIVATE {case}: the review needs your decision.")
        first = call(**args)
        second = call(**args)
        changed = call(request_id=args["request_id"], text="CHANGED " + case)
        final = None
        if first.get("structuredContent", {}).get("accepted") and case != "lost":
            final = settled(args)
        mark(case, listed=offered, first=first, second=second, changed=changed, final=final)
    return "DONE " + case


for line in sys.stdin:
    request = json.loads(line)
    method, params, identity = request.get("method"), request.get("params", {}), request.get("id")
    try:
        if method == "initialize":
            result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True, "mcpCapabilities": {"http": True}}}
        elif method in {"session/new", "session/load"}:
            SERVER = params["mcpServers"][0]
            result = {"sessionId": "owner-message-fixture-" + str(uuid.uuid4())} if method == "session/new" else {}
        elif method == "session/prompt":
            value = prompt(params)
            output({"method": "session/update", "params": {"sessionId": params["sessionId"], "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": json.dumps(value) if isinstance(value, dict) else value}}}})
            result = {"stopReason": "end_turn"}
        else:
            result = {}
        if identity is not None:
            output({"id": identity, "result": result})
    except BaseException as error:
        record("fixture_error", method=method, error=repr(error))
        raise
