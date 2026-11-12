"""Deterministic ordinary ACP endpoint; huddle calls use the real granted MCP.

No model, production credential, external network, or fake adapter grant is used.
The parent fixture controls only Slack's loopback service and human input.
"""

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
        with urllib.request.urlopen(request, context=ssl._create_unverified_context(), timeout=10) as response:
            value = json.load(response)
    except urllib.error.HTTPError as error:
        record("tool_rejected", tool=args.get("name"), status=error.code, response=error.read().decode(errors="replace")[:2000])
        raise
    if "error" in value:
        record("tool_rejected", tool=args.get("name"), error=value["error"])
        raise RuntimeError(value["error"])
    value = value["result"]
    record("mcp", method=method, arguments=args, result=value)
    if value.get("isError"):
        raise RuntimeError(value)
    return value


def call(name, **args):
    return rpc("tools/call", {"name": name, "arguments": args})["structuredContent"]


def mark(case, phase, **fields):
    (ROOT / (case + "-" + phase + ".json")).write_text(json.dumps(fields))


def wait_for(test, description, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = test()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError("Timed out: " + description)


def request_id(case, operation):
    return str(uuid.uuid5(uuid.NAMESPACE_URL, "slack-huddle-fixture:" + case + ":" + operation))


def prompt(params):
    text = "".join(block.get("text", "") for block in params["prompt"])
    record("prompt_dispatch", session_id=params["sessionId"], has_projection="Latest conversation projection:\n" in text)
    if "Latest conversation projection:\n" not in text:
        return {"decision": "pass", "reason": "explicit_silence_request"}
    view = json.loads(text.split("Latest conversation projection:\n", 1)[1])
    trigger = view["trigger"]
    record("prompt", session_id=params["sessionId"], trigger=trigger)
    incoming = trigger["text"]

    if NAME == "beta":
        if "HUMAN_JOIN happy" in incoming and not trigger.get("bot"):
            reused = call("slack_huddle_ensure", purpose="HUDDLE_REUSE happy")
            mark("happy", "peer_reuse", descriptor=reused, trigger=trigger)
            assert reused["reused"] and reused["status"] == "existing_thread"
            assert reused["root_ts"] == trigger["thread"] and reused["huddle_id"] is None
            return {"decision": "pass", "reason": "explicit_silence_request"}
        match = re.search(r"HUDDLE_(?:CASE|WORK) ([a-z_]+)", incoming)
        if match and trigger.get("bot"):
            case = match.group(1)
            once = ROOT / (case + "-beta-once")
            if not once.exists():
                once.write_text(params["sessionId"])
                if case == "deferred":
                    wait_for(lambda: (ROOT / "deferred-peer-release").exists(), "original turn terminal and restart before peer reply")
                return "PEER_DONE " + case + ": reviewed the existing artifact." + (" <@Ualpha>" if case == "deferred" else "")
        return {"decision": "pass", "reason": "explicit_silence_request"}

    if incoming.startswith("PEER_DONE deferred") and trigger.get("bot"):
        initial = json.loads((ROOT / "deferred-ready.json").read_text())["descriptor"]
        assert trigger["thread"] == initial["root_ts"]
        assert any(item["huddle_id"] == initial["huddle_id"] for item in view["side_conversation"]["threads"])
        reused = call("slack_huddle_ensure", purpose="Reuse the existing review thread")
        assert reused["huddle_id"] == initial["huddle_id"] and reused["reused"]
        descriptor = call("slack_huddle_status", huddle_id=reused["huddle_id"])
        observed = call("slack_context_read", conversation_id=trigger["channel"], thread_id=trigger["thread"])
        assert "PEER_DONE deferred" in json.dumps(observed)
        args = dict(huddle_id=reused["huddle_id"], request_id=request_id("deferred", "result"), text="RESULT deferred: existing artifact reviewed; ready for the human decision.", kind="result")
        result = call("slack_huddle_result", **args)
        repeated = call("slack_huddle_result", **args)
        assert result["operation"]["request_id"] == repeated["operation"]["request_id"] == args["request_id"]
        mark("deferred", "result", descriptor=descriptor, result=result, repeated=repeated, side_session=params["sessionId"])
        return None

    match = re.search(r"HUDDLE_START ([a-z_]+)", incoming)
    if not match or trigger.get("bot"):
        return {"decision": "pass", "reason": "explicit_silence_request"}
    case = match.group(1)
    tools = rpc("tools/list", {})["tools"]
    expected = {"slack_huddle_ensure", "slack_huddle_status", "slack_huddle_post", "slack_huddle_result"}
    advertised = {t["name"] for t in tools}
    if case in {"private", "thread_only"}:
        assert not expected & advertised
    else:
        assert expected <= advertised
    try:
        first = call("slack_huddle_ensure", purpose="HUDDLE_CASE " + case, peer_user_ids=["Ubeta"])
    except (urllib.error.HTTPError, RuntimeError):
        assert case in {"private", "thread_only"}, case
        mark(case, "denied")
        return {"decision": "pass", "reason": "explicit_silence_request"}
    assert case not in {"private", "thread_only"}, first
    repeated = call("slack_huddle_ensure", purpose="HUDDLE_CASE " + case, peer_user_ids=["Ubeta"])
    assert repeated["huddle_id"] == first["huddle_id"] and repeated["reused"]
    huddle = first["huddle_id"]
    mark(case, "created", first=first, repeated=repeated)

    def ready():
        value = call("slack_huddle_status", huddle_id=huddle)
        return value if value.get("root_ts") and value.get("status") == "sent" else None

    descriptor = wait_for(ready, "confirmed side-conversation root")
    root_ts = descriptor["root_ts"]
    mark(case, "ready", descriptor=descriptor)
    if case in {"edited", "revoked", "stopped"}:
        wait_for(lambda: (ROOT / (case + "-release")).exists(), "test authority change")
        try:
            value = call("slack_huddle_result", huddle_id=huddle, request_id=request_id(case, "result"), text="MUST_NOT_SEND " + case, kind="result")
            mark(case, "after_change", value=value)
        except (urllib.error.HTTPError, RuntimeError):
            mark(case, "denied")
        return {"decision": "pass", "reason": "explicit_silence_request"}

    args = dict(huddle_id=huddle, request_id=request_id(case, "work"), text="HUDDLE_WORK " + case + " <@Ubeta>: inspect the existing artifact; keep discussion here.")
    first_post = call("slack_huddle_post", **args)
    repeated_post = call("slack_huddle_post", **args)
    # Repeated tool calls identify one delivery even if its observed state advances.
    assert first_post["operation"]["request_id"] == repeated_post["operation"]["request_id"] == args["request_id"]
    assert [item["id"] for item in first_post["operation"]["receipts"]] == [item["id"] for item in repeated_post["operation"]["receipts"]]
    assert first_post["operation"]["receipts"]
    if case == "deferred":
        mark(case, "awaiting_side", original_session=params["sessionId"])
        return None

    def read_peer():
        value = call("slack_context_read", conversation_id=trigger["channel"], thread_id=root_ts)
        return value if "PEER_DONE " + case in json.dumps(value) else None

    observed = wait_for(read_peer, "second real adapter reply in the side thread")
    mark(case, "inspected", descriptor=descriptor, observed=observed)
    if case == "happy":
        wait_for(lambda: (ROOT / "happy-join-release").exists(), "human inspect then join")

        def read_join():
            value = call("slack_context_read", conversation_id=trigger["channel"], thread_id=root_ts)
            return value if "HUMAN_JOIN happy" in json.dumps(value) else None

        mark(case, "joined", observed=wait_for(read_join, "human side-thread contribution"))
    args = dict(huddle_id=huddle, request_id=request_id(case, "result"), text="RESULT " + case + ": existing artifact reviewed; ready for the human decision.", kind="result")
    result = call("slack_huddle_result", **args)
    repeated_result = call("slack_huddle_result", **args)
    assert result["operation"]["request_id"] == repeated_result["operation"]["request_id"] == args["request_id"]
    assert [item["id"] for item in result["operation"]["receipts"]] == [item["id"] for item in repeated_result["operation"]["receipts"]]
    assert result["operation"]["receipts"]
    mark(case, "result", descriptor=descriptor, result=result, repeated=repeated_result)
    return None  # Tools already own public delivery; no duplicate final sentence.


for line in sys.stdin:
    request = json.loads(line)
    method, params, identity = request.get("method"), request.get("params", {}), request.get("id")
    try:
        if method == "initialize":
            result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True, "mcpCapabilities": {"http": True}}, "_meta": {"com.microsoft.amplifier.attention": {"version": 1, "enabled": True, "method": "_amplifier/attention"}}}
        elif method in {"session/new", "session/load"}:
            SERVER = params["mcpServers"][0]
            result = {"sessionId": "huddle-fixture-" + str(uuid.uuid4())} if method == "session/new" else {}
        elif method == "_amplifier/attention":
            result = {"action": "engage", "id": "fixture-offer", "ticket": "fixture-ticket"}
        elif method == "session/prompt":
            value = prompt(params)
            if value is not None:
                output({"method": "session/update", "params": {"sessionId": params["sessionId"], "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": json.dumps(value) if isinstance(value, dict) else value}}}})
            result = {"stopReason": "end_turn"}
        else:
            result = {}
        if identity is not None:
            output({"id": identity, "result": result})
    except BaseException as error:
        record("fixture_error", method=method, error=repr(error))
        raise
