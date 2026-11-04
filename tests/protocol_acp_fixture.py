"""An ordinary, independent ACP stdio implementation for process integration.

No adapter imports, provider credentials, model calls, or network access. Session
state lives in this fixture endpoint's directory, independently of the adapter.
The tiny deterministic policy is a protocol fixture, not evidence of LLM quality.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid


ROOT = Path(sys.argv[1])
NAME = sys.argv[2]
ROOT.mkdir(parents=True, exist_ok=True)


def log(kind, **data):
    record = json.dumps({"kind": kind, "pid": os.getpid(), **data}) + "\n"
    fd = os.open(ROOT / "protocol.jsonl", os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    try:
        os.write(fd, record.encode())
    finally:
        os.close(fd)


def send(value):
    print(json.dumps({"jsonrpc": "2.0", **value}), flush=True)


def result(request, value):
    send({"id": request["id"], "result": value})


def session_path(sid):
    return ROOT / (str(uuid.UUID(sid)) + ".json")


def update(sid, kind, text):
    send({"method": "session/update", "params": {"sessionId": sid, "update": {
        "sessionUpdate": kind, "content": {"type": "text", "text": text}}}})


log("started")
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    if method == "initialize":
        log("initialize", params=params)
        failure = ROOT / "initialization-failure"
        if failure.exists():
            if failure.read_text() == "once":
                failure.unlink()
            send({"id": request["id"], "error": {"code": -32603, "message": "Fixture startup failure"}})
            continue
        result(request, {"protocolVersion": 1, "agentCapabilities": {"loadSession": os.getenv("FIXTURE_NO_LOAD") != "1"},
                         "agentInfo": {"name": "independent-python-fixture", "version": "1.0"}})
    elif method == "session/new":
        sid = str(uuid.uuid4())
        session_path(sid).write_text(json.dumps({"turns": 0}))
        log("session_new", session_id=sid, cwd=params["cwd"])
        result(request, {"sessionId": sid})
    elif method == "session/load":
        sid = params["sessionId"]
        if session_path(sid).exists():
            log("session_load", session_id=sid, cwd=params["cwd"])
            result(request, {})
        else:
            send({"id": request["id"], "error": {"code": -32602, "message": "Unknown session"}})
    elif method == "session/prompt":
        sid = params["sessionId"]
        prompt = "".join(p.get("text", "") for p in params["prompt"])
        if prompt.startswith("This is a direct conversation or explicit address."):
            log("direct_reply_repair", session_id=sid)
            update(sid, "agent_message_chunk", NAME + " repaired direct answer")
            result(request, {"stopReason": "end_turn"})
            continue
        conversation = json.loads(prompt.split("Latest conversation projection:\n", 1)[1])
        trigger = conversation["trigger"]["text"]
        if 'Decide quietly' in prompt and not any(word in trigger for word in ['fail-prompt-no-retry','stall-child','malformed']):
            log('participation' if 'ask '+NAME in trigger else 'prompt',session_id=sid,input=conversation)
            update(sid,'agent_message_chunk',json.dumps({'decision':'speak' if 'ask '+NAME in trigger else 'pass','reason':'fixture decision'}))
            result(request,{'stopReason':'end_turn'})
            continue
        state = json.loads(session_path(sid).read_text())
        state["turns"] += 1
        session_path(sid).write_text(json.dumps(state))
        log("prompt", session_id=sid, turn=state["turns"], input=conversation)
        if "fail-prompt-no-retry" in trigger:
            send({"id": request["id"], "error": {"code": -32603, "message": "Fixture turn failure"}})
            continue
        if "stall-child" in trigger:
            child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)", str(ROOT)])
            log("descendant", child_pid=child.pid, session_id=sid)
            time.sleep(120)
        if "slow" in trigger:
            time.sleep(3)
        if "permission" in trigger:
            permission_id = "permission-" + str(uuid.uuid4())
            send({"id": permission_id, "method": "session/request_permission", "params": {
                "sessionId": sid,
                "toolCall": {"toolCallId": "fixture-tool", "title": "Fixture permission", "kind": "execute"},
                "options": [{"optionId": "yes", "name": "Allow once", "kind": "allow_once"},
                            {"optionId": "no", "name": "Reject once", "kind": "reject_once"}]}})
            permission = json.loads(sys.stdin.readline())
            log("permission_reply", value=permission)
            assert permission["id"] == permission_id
            assert permission["result"]["outcome"]["outcome"] == "cancelled"
        # Thoughts and other sessions' output must not become public Slack text.
        update(sid, "agent_thought_chunk", "PRIVATE-THOUGHT-MUST-NOT-PUBLISH")
        update("unrelated-session", "agent_message_chunk", "WRONG-SESSION-MUST-NOT-PUBLISH")
        if "Return exactly one final JSON object" in prompt:
            if "malformed" in trigger:
                output = "this is not a participation decision"
            elif "ask " + NAME in trigger:
                output = json.dumps({"decision": "speak", "text": NAME + " result: " + trigger,
                                     "reason": "Requested contribution"})
            else:
                output = json.dumps({"decision": "pass", "reason": "No distinct contribution"})
        elif "retained-ambient-pass" in trigger:
            output = json.dumps({"decision": "pass", "reason": "No distinct contribution"})
        elif "explicit-silence-test" in trigger:
            output = json.dumps({"decision": "pass", "reason": "explicit_silence_request"})
        else:
            output = NAME + " plain answer: " + trigger
        midpoint = len(output) // 2
        update(sid, "agent_message_chunk", output[:midpoint])
        update(sid, "agent_message_chunk", output[midpoint:])
        result(request, {"stopReason": "end_turn"})
        log("turn_finished", session_id=sid)
    elif method == "session/cancel":
        log("cancel", session_id=params["sessionId"])
    elif "id" in request:
        send({"id": request["id"], "error": {"code": -32601, "message": "Unknown method"}})
