"""Real adapter processes, separate ordinary ACP programs, and loopback Slack.

Run after cargo build: python3 tests/process_integration.py
Requires aiohttp. Set ACP_SLACK_TEST_BINARY to exercise a release binary.
All Slack traffic stays on loopback and accepts only test-* credentials. This
tests protocol/lifecycle boundaries, not Slack's service or model/social quality.
ACP_SLACK_TEST_UPDATES may name a host-owned read-only mount with a 0400
admission.lock and 0500 directory, to test an actual container mount as well.
"""

import asyncio
import getpass
import json
import os
from pathlib import Path
import shutil
import sqlite3
import ssl
import sys
import tempfile
import time
import uuid

from aiohttp import ClientSession, web


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("ACP_SLACK_TEST_BINARY", ROOT / "target/debug/acp-slack-adapter"))


class SlackFixture:
    def __init__(self):
        self.sockets = {}
        self.messages = []
        self.posts = []
        self.indicators = []
        self.indicator_updates = []
        self.attempts = {}
        self.acks = set()
        self.sequence = 0
        self.port = 0
        self.retry_after = 1

    async def api(self, request):
        token = request.headers.get("Authorization", "").removeprefix("Bearer ")
        assert token.startswith("test-"), "Loopback fixture refuses real credentials"
        name = token.removeprefix("test-").removesuffix("-app")
        method = request.match_info["method"]
        body = dict(request.query) if request.method == "GET" else await request.json()
        if method == "auth.test":
            return web.json_response({"ok": True, "team_id": "Ttest", "team": "Test workspace",
                                      "user_id": "U" + name, "bot_id": "B" + name})
        if method == "bots.info":
            return web.json_response({"ok": True, "bot": {"id": "B" + name, "app_id": "A" + name}})
        if method == "apps.connections.open":
            return web.json_response({"ok": True, "url": f"ws://127.0.0.1:{self.port}/socket/{name}"})
        if method == "conversations.members":
            return web.json_response({"ok": True, "members": ["Ualpha", "Ubeta", "UHUMAN"]})
        if method == "users.info":
            return web.json_response({"ok": True, "user": {"id": body["user"],
                "is_bot": body["user"] != "UHUMAN", "profile": {"display_name": body["user"][1:]}}})
        if method in ("conversations.history", "conversations.replies"):
            messages = [m for m in self.messages if m["channel"] == body["channel"]]
            if method.endswith("replies"):
                messages = [m for m in messages if m["ts"] == body["ts"] or m.get("thread_ts") == body["ts"]]
            else:
                messages = [m for m in messages if not m.get("thread_ts")]
            return web.json_response({"ok": True, "messages": messages[-80:][::-1]})
        if method == "chat.postMessage" and body.get("metadata",{}).get("event_type") == "acp_work_status":
            assert body['mrkdwn'] is False and body['parse']=='none' and body['link_names'] is False
            assert body['blocks'][0]['text']['type']=='plain_text' and 'markdown_text' not in body
            message={**body,"ts":self.timestamp(),"text":body['blocks'][0]['text']['text'],"user":"U"+name,"bot_id":"B"+name}
            self.messages.append(message);self.indicators.append(message)
            self.indicator_updates.append(dict(message))
            return web.json_response({"ok":True,"ts":message["ts"]})
        if method in ("chat.update","chat.delete"):
            match=next((m for m in self.indicators if m["channel"]==body["channel"] and m["ts"]==body["ts"]),None)
            if match:
                if method=="chat.update":
                    match.update(body);match['text']=body['blocks'][0]['text']['text'];self.indicator_updates.append(dict(match))
                else:match["deleted"]=True;self.messages=[m for m in self.messages if m["ts"]!=body["ts"]]
            return web.json_response({"ok":bool(match),"ts":body["ts"],"error":"message_not_found"})
        if method == "chat.postMessage":
            assert "text" not in body and "blocks" not in body, "Markdown cannot be mixed with legacy fields"
            assert isinstance(body["markdown_text"], str) and len(body["markdown_text"]) <= 12000
            message_id = body["client_msg_id"]
            self.attempts[message_id] = self.attempts.get(message_id, 0) + 1
            if "rate-limit" in body["markdown_text"] and self.attempts[message_id] == 1:
                return web.Response(status=429, headers={"Retry-After": str(self.retry_after)})
            self.posts.append({**body, "agent": name})
            if "lost-response" in body["markdown_text"]:
                return web.Response(status=500)
            message = await self.send(body["channel"], body["markdown_text"], user="U" + name,
                bot_id="B" + name, thread=body.get("thread_ts", ""), client_id=message_id)
            return web.json_response({"ok": True, "ts": message["ts"]})
        return web.json_response({"ok": False, "error": "unknown_method"})

    async def socket(self, request):
        name = request.match_info["name"]
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        self.sockets[name] = ws
        await ws.send_json({"type": "hello", "connection_info": {"app_id": "A" + name}})
        try:
            async for item in ws:
                if item.type == web.WSMsgType.TEXT:
                    value = json.loads(item.data)
                    if "envelope_id" in value:
                        self.acks.add(value["envelope_id"])
        finally:
            if self.sockets.get(name) is ws:
                del self.sockets[name]
        return ws

    def timestamp(self):
        self.sequence += 1
        return f"{1800000000 + self.sequence}.000001"

    async def send(self, channel, text, *, user="UHUMAN", bot_id=None, thread="", client_id=None):
        message = {"type": "message", "channel": channel,
            "channel_type": "im" if channel.startswith("D") else "channel",
            "user": user, "text": text, "ts": self.timestamp()}
        if bot_id:
            message["bot_id"] = bot_id
        if thread:
            message["thread_ts"] = thread
        if client_id:
            message["client_msg_id"] = client_id
        self.messages.append(message)
        await self.deliver(message)
        return message

    async def edit(self, original, text):
        updated = {**original, "text": text, "edited": {"ts": self.timestamp()}}
        self.messages = [updated if m["channel"] == original["channel"] and m["ts"] == original["ts"] else m
                         for m in self.messages]
        await self.deliver({"type": "message", "subtype": "message_changed",
            "channel": original["channel"], "channel_type": original["channel_type"],
            "message": updated, "previous_message": original, "event_ts": updated["edited"]["ts"]})

    async def deliver(self, message, event_id=None, *, team="Ttest", app=None):
        event_id = event_id or str(uuid.uuid4())
        for name, ws in list(self.sockets.items()):
            if not ws.closed:
                await ws.send_json({"envelope_id": f"{name}:{event_id}", "type": "events_api", "payload": {
                    "team_id": team, "api_app_id": app or "A" + name,
                    "event_id": event_id, "event": message}})
        return event_id


async def until(check, label, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        await asyncio.sleep(.05)
    raise AssertionError("Timed out: " + label)


def rows(home, sql, args=()):
    with sqlite3.connect(home / "state.sqlite3") as db:
        return db.execute(sql, args).fetchall()


def count(home, sql, args=()):
    return rows(home, sql, args)[0][0]


def protocol(home, kind=None):
    path = home / "endpoint/protocol.jsonl"
    if not path.exists():
        return []
    records = [json.loads(line) for line in path.read_text().splitlines()]
    return [record for record in records if kind is None or record["kind"] == kind]


def live_pid(pid):
    # A killed, not-yet-reaped Linux zombie is not an executing descendant.
    path = Path(f"/proc/{pid}/stat")
    if path.exists():
        return path.read_text().split(") ", 1)[1][0] != "Z"
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def private_json(path, value):
    path.write_text(json.dumps(value, indent=2))
    path.chmod(0o600)


async def main():
    if not BINARY.is_file():
        raise SystemExit("Build the adapter first: cargo build")
    fixture = SlackFixture()
    app = web.Application()
    app.router.add_post("/api/{method}", fixture.api)
    app.router.add_get("/api/{method}", fixture.api)
    app.router.add_get("/socket/{name}", fixture.socket)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    fixture.port = site._server.sockets[0].getsockname()[1]
    checks, processes, log_files, homes = [], {}, {}, {}

    def passed(description):
        checks.append(description)
        print("PASS " + description, flush=True)

    with tempfile.TemporaryDirectory(prefix="acp-slack-process-test-") as temporary:
        try:
            for name, participation in (("alpha", "ambient"), ("beta", "mentions")):
                home = Path(temporary) / name
                homes[name] = home
                for folder in (home, home / "bin", home / "secrets", home / "endpoint"):
                    folder.mkdir(exist_ok=True)
                    folder.chmod(0o700)
                shutil.copy2(BINARY, home / "bin/acp-slack-adapter")
                shutil.copy2(ROOT / "tests/protocol_acp_fixture.py", home / "endpoint/agent.py")
                config = {"schema": 1, "id": str(uuid.uuid4()), "label": name, "revision": 1,
                    "enabled": name == "alpha", "setup_complete": name == "alpha", "participation": participation,
                    "acp": {"command": sys.executable, "args": [str(home / "endpoint/agent.py"),
                        str(home / "endpoint"), name], "cwd": str(home / "endpoint"), "timeout_seconds": 60,
                        "env": {"FIXTURE_NO_LOAD": "1"} if name == "beta" else {}},
                    "slack": {"team_id": "Ttest", "app_id": "A" + name, "bot_user_id": "U" + name,
                        "bot_id": "B" + name, "channels": ["Cshared", "C" + name, "D" + name],
                        "allow_dms": True, "loopback_test": True,
                        "api_base": f"http://127.0.0.1:{fixture.port}/api/"},
                    "attention": {"parallel_contexts": 2, "settle_ms": 30,
                        "minimum_reply_interval_seconds": 0, "max_replies_per_context_per_hour": 100,
                        "max_replies_per_agent_per_hour": 100, "max_decisions_per_agent_per_hour": 200,
                        "max_bot_replies_without_human": 4},
                    "server": {"bind": "127.0.0.1", "port": 0, "owner": getpass.getuser(),
                        "pam_service": "acp-slack-adapter", "public_url": None, "embed_origins": []}}
                private_json(home / "adapter.json", config)
                private_json(home / "secrets/slack.json", {
                    "app_token": f"test-{name}-app", "bot_token": f"test-{name}"})
                if name == "alpha":
                    # Host-managed update controls may be read-only inside an
                    # installation. Normal Slack intake must not modify them.
                    updates = home / "updates"
                    if os.environ.get("ACP_SLACK_TEST_UPDATES"):
                        updates.symlink_to(os.environ["ACP_SLACK_TEST_UPDATES"], target_is_directory=True)
                    else:
                        updates.mkdir(mode=0o700)
                        (updates / "admission.lock").touch(mode=0o400)
                        updates.chmod(0o500)

            async def start(name, connect=True):
                home = homes[name]
                log_files[name] = open(home / "service.log", "ab", buffering=0)
                process = await asyncio.create_subprocess_exec(str(home / "bin/acp-slack-adapter"),
                    "serve", "--home", str(home), stdout=log_files[name], stderr=log_files[name])
                processes[name] = process
                try:
                    await until(lambda: (name in fixture.sockets if connect else (home / "endpoints.json").exists())
                                or process.returncode is not None,
                                name + " connection")
                    assert process.returncode is None, (home / "service.log").read_text()
                    if not connect:
                        await asyncio.sleep(.1)
                except Exception:
                    print((home / "service.log").read_text(), file=sys.stderr)
                    raise

            async def stop(name):
                process = processes[name]
                if process.returncode is None:
                    process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), 10)
                except TimeoutError:
                    process.kill()
                    await process.wait()
                    raise AssertionError(name + " did not shut down within 10 seconds")
                log_files[name].close()
                await until(lambda: name not in fixture.sockets, name + " disconnected")

            async def api(name, method, path, body=None, expect=None):
                home = homes[name]
                endpoint = json.loads((home / "endpoints.json").read_text())["base_url"]
                trust = ssl.create_default_context(cafile=str(home / "tls/root-ca.crt"))
                token = (home / "secrets/control-token").read_text().strip()
                async with ClientSession() as client:
                    async with client.request(method, endpoint + "/api/v1/" + path, json=body,
                        headers={"Authorization": "Bearer " + token}, ssl=trust) as response:
                        value = await response.json()
                        assert (response.status in expect if expect else response.status < 300), (response.status, value)
                        return value

            def has_post(marker):
                return any(marker in post["markdown_text"] for post in fixture.posts)

            def has_prompt(name, marker):
                return any(marker in p["input"]["trigger"]["text"] for p in protocol(homes[name], "prompt"))

            await start("alpha")
            await start("beta", connect=False)
            checked = await api("beta", "POST", "check", {"target": "all"})
            assert checked["ok"] and checked["acp"]["ok"] and checked["slack"]["ok"]
            assert not protocol(homes["beta"], "prompt") and not fixture.posts
            await api("beta", "POST", "credentials/slack", {
                "app_token": "test-unrelated-app", "bot_token": "test-beta", "app_id": "Abeta"}, expect=(400, 422))
            assert json.loads((homes["beta"] / "secrets/slack.json").read_text())["app_token"] == "test-beta-app"
            await api("beta", "POST", "credentials/slack", {
                "app_token": "test-beta-app", "bot_token": "test-beta", "app_id": "Abeta"})
            await api("beta", "POST", "setup/complete", {})
            await until(lambda: "beta" in fixture.sockets, "setup activates connection")
            passed("Setup verifies ordinary ACP and matching Slack credentials without prompting or posting")
            assert processes["alpha"].pid != processes["beta"].pid
            passed("Separate installed adapter processes, endpoint implementations, credentials, and state")

            await fixture.send("Dalpha", "hello from a DM with host-managed updates")
            await until(lambda: has_post("hello from a DM with host-managed updates"), "read-only updates DM reply")
            updates = homes["alpha"] / "updates"
            assert updates.stat().st_mode & 0o777 == 0o500
            assert (updates / "admission.lock").stat().st_mode & 0o777 == 0o400
            passed("A direct message receives a reply without modifying host-managed update controls")

            duplicate = await asyncio.create_subprocess_exec(str(homes["alpha"] / "bin/acp-slack-adapter"),
                "serve", "--home", str(homes["alpha"]), stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL)
            await asyncio.wait_for(duplicate.wait(), 10)
            assert duplicate.returncode != 0
            passed("A duplicate adapter process cannot claim an existing installation")

            message = await fixture.send("Calpha", "ask alpha: ambient request")
            await until(lambda: has_post("ambient request"), "ambient reply")
            await until(lambda: count(homes["alpha"], "SELECT COUNT(*) FROM outbox WHERE status='sent'") == 2,
                        "own echo recorded")
            before = len(fixture.posts)
            jobs_before = count(homes["alpha"], "SELECT COUNT(*) FROM jobs")
            await fixture.deliver(message, "redelivery")
            await fixture.deliver(message, "redelivery")
            await fixture.deliver({**message, "type": "app_mention"}, "duplicate-mention")
            await asyncio.sleep(.7)
            assert len(fixture.posts) == before
            assert count(homes["alpha"], "SELECT COUNT(*) FROM jobs") == jobs_before
            passed("Event replay, message/mention duplication, and self echoes do not cause extra turns")

            await fixture.send("Calpha", "Thanks; nothing else needed")
            await until(lambda: count(homes["alpha"], "SELECT COUNT(*) FROM jobs WHERE status='pass'") >= 1,
                        "ambient pass")
            assert len(fixture.posts) == before
            passed("Ambient participation can choose silence without publishing acknowledgements")

            await fixture.send("Cbeta", "ordinary message without addressing the agent")
            await asyncio.sleep(.6)
            assert not protocol(homes["beta"], "prompt")
            direct = await fixture.send("Cbeta", "<@Ubeta> direct plain reply")
            await fixture.deliver({**direct, "type": "app_mention"}, "beta-mention-copy")
            await until(lambda: has_post("beta plain answer: <@Ubeta> direct plain reply"), "plain ACP reply")
            assert len(protocol(homes["beta"], "prompt")) == 1
            passed("Mentions mode uses ordinary ACP text; unaddressed messages do not invoke the agent")

            await fixture.send("Dbeta", "A private direct message")
            await until(lambda: has_post("beta plain answer: A private direct message"), "direct message")
            passed("Direct messages work without a mention")

            markdown = (
                "FORMAT-MARKDOWN\n## Latest conversations\n\n"
                "- **[Project chat](https://example.com/chat?first=1&second=2)**\n"
                "  **You:** Try _nested emphasis_, ~~old wording~~, and `a_b < 3`.\n"
                "  **Me:** Café → ready.\n\n"
                "> A quoted note\n\n"
                "```python\nprint(\"**literal** [not a link](x) <&>\")\n```\n\n"
                "| Task | Status |\n| --- | --- |\n| Build | Done |"
            )
            await fixture.send("Dbeta", markdown)
            await until(lambda: has_post("FORMAT-MARKDOWN"), "formatted DM reply")
            formatted = next(p for p in fixture.posts if "FORMAT-MARKDOWN" in p["markdown_text"])
            original = "beta plain answer: " + markdown
            assert formatted["markdown_text"] == original
            assert "thread_ts" not in formatted
            assert formatted["unfurl_links"] is False and formatted["unfurl_media"] is False
            assert formatted["parse"] == "none"
            assert rows(homes["beta"], "SELECT text FROM outbox WHERE id=?", (formatted["client_msg_id"],)) == [(original,)]
            passed("Standard Markdown reaches Slack's native renderer unchanged; the outbox retains ACP text")

            await fixture.send("Dalpha", "A private message to the ambient agent")
            await until(lambda: has_post("alpha plain answer: A private message to the ambient agent"), "ambient agent direct reply")
            await fixture.send("Calpha", "<@Ualpha> explicitly addressing the ambient agent")
            await until(lambda: has_post("alpha plain answer: <@Ualpha> explicitly addressing"), "ambient mention direct reply")
            passed("An ambient agent replies normally in a private DM and when explicitly mentioned")

            await fixture.send("Dalpha", "retained-ambient-pass")
            await until(lambda: has_post("alpha repaired direct answer"), "direct answer repair")
            assert len(protocol(homes["alpha"], "direct_reply_repair")) == 1
            passed("A stale group silence decision in a DM is reconsidered once without repeating work")

            before_silence = len(fixture.posts)
            await fixture.send("Dalpha", "Please do not reply: explicit-silence-test")
            await until(lambda: count(homes["alpha"], "SELECT COUNT(*) FROM jobs WHERE status='pass' AND trigger LIKE '%explicit-silence-test%'") == 1,
                        "explicit no-reply request")
            assert len(fixture.posts) == before_silence
            passed("A direct conversation still respects an explicit request not to respond")

            await fixture.send("Calpha", "ask alpha: slow PRIVATE-ALPHA")
            await until(lambda: has_prompt("alpha", "PRIVATE-ALPHA"), "slow conversation started")
            await fixture.send("Cshared", "ask alpha: fast shared conversation")
            await until(lambda: has_post("fast shared conversation"), "parallel conversation")
            assert not has_post("PRIVATE-ALPHA")
            await until(lambda: has_post("PRIVATE-ALPHA"), "slow conversation finished")
            await fixture.send("Cbeta", "<@Ubeta> PRIVATE-BETA")
            await until(lambda: has_post("PRIVATE-BETA"), "private beta response")
            for home in homes.values():
                for record in protocol(home, "prompt"):
                    setup = record["input"]["context_tools"]
                    assert setup["status"] == "disabled"
                    assert setup["descriptor_supplied"] is False
                    assert setup["authentication"] == "not_verified_by_session_setup"
                    if record["input"]["conversation"]["channel"] == "Cshared":
                        text = json.dumps(record["input"])
                        assert "PRIVATE-ALPHA" not in text and "PRIVATE-BETA" not in text
            assert count(homes["alpha"], "SELECT COUNT(*) FROM messages WHERE channel='Cbeta'") == 0
            assert count(homes["beta"], "SELECT COUNT(*) FROM messages WHERE channel='Calpha'") == 0
            passed("Simultaneous rooms progress independently, with separate histories and membership scope")

            beta_prompts = [p for p in protocol(homes["beta"], "prompt")
                            if p["input"]["conversation"]["channel"] == "Cbeta"]
            assert len(beta_prompts) >= 2
            assert len({p["session_id"] for p in beta_prompts}) == len(beta_prompts)
            assert not protocol(homes["beta"], "session_load")
            assert any("direct plain reply" in m["text"] for m in beta_prompts[-1]["input"]["history"])
            passed("An ACP agent without session/load works through fresh sessions and bounded Slack history")

            await fixture.send("Cbeta", "<@Ubeta> slow REQUEST-SURVIVES-ASIDE")
            await until(lambda: has_prompt("beta", "REQUEST-SURVIVES-ASIDE"), "direct request in progress")
            await fixture.send("Cbeta", "An unmentioned aside with UPDATED-CONTEXT")
            await until(lambda: has_post("REQUEST-SURVIVES-ASIDE"), "direct request re-evaluated after aside")
            contributions = [p for p in fixture.posts if "REQUEST-SURVIVES-ASIDE" in p["markdown_text"]]
            assert len(contributions) == 1
            reevaluations = [p for p in protocol(homes["beta"], "prompt")
                            if "REQUEST-SURVIVES-ASIDE" in p["input"]["trigger"]["text"]]
            assert len(reevaluations) >= 2
            assert any("UPDATED-CONTEXT" in message["text"] for message in reevaluations[-1]["input"]["history"])
            passed("An unmentioned aside updates a direct request without silently abandoning its answer")

            alpha_prompts = [p for p in protocol(homes["alpha"], "prompt")
                             if p["input"]["conversation"]["channel"] == "Calpha"]
            assert len({p["session_id"] for p in alpha_prompts}) == 1
            assert len({p["pid"] for p in alpha_prompts}) == len(alpha_prompts)
            assert any(p["session_id"] == alpha_prompts[0]["session_id"]
                       for p in protocol(homes["alpha"], "session_load"))
            assert alpha_prompts[0]["input"]["context_tools"]["session_resumed"] is False
            assert alpha_prompts[-1]["input"]["context_tools"]["session_resumed"] is True
            passed("A conversation resumes its ordinary ACP session across separate endpoint processes")

            root = await fixture.send("Cshared", "A thread root; no contribution needed")
            await until(lambda: has_prompt("alpha", "A thread root"), "thread root observed")
            await asyncio.sleep(.3)
            await fixture.send("Cshared", "ask alpha: threaded contribution **with [formatting](https://example.com/thread)**", thread=root["ts"])
            await until(lambda: has_post("threaded contribution"), "thread reply")
            post = next(p for p in fixture.posts if "threaded contribution" in p["markdown_text"])
            assert post["thread_ts"] == root["ts"]
            assert "**with [formatting](https://example.com/thread)**" in post["markdown_text"]
            bindings = rows(homes["alpha"], "SELECT channel,thread,session_id FROM session_bindings")
            assert len({b[2] for b in bindings}) == len(bindings)
            assert ("Cshared", root["ts"]) in {(b[0], b[1]) for b in bindings}
            passed("Slack threads retain their reply address and a distinct internal ACP session")

            fixture.retry_after = 3
            await fixture.send("Cshared", "ask alpha: rate-limit ROOT-EDIT-QUEUE", thread=root["ts"])
            await until(lambda: count(homes["alpha"],
                "SELECT COUNT(*) FROM outbox WHERE text LIKE '%ROOT-EDIT-QUEUE%' AND status='ready' AND note='Slack rate limit'") == 1,
                "thread reply queued after rate limit")
            root_delivery = rows(homes["alpha"], "SELECT id FROM outbox WHERE text LIKE '%ROOT-EDIT-QUEUE%'")[0][0]
            await fixture.edit(root, "The corrected thread root; stay quiet")
            await until(lambda: count(homes["alpha"],
                "SELECT COUNT(*) FROM outbox WHERE id=? AND status NOT IN ('ready','sending')", (root_delivery,)) == 1,
                "queued thread answer invalidated by root edit")
            assert not has_post("ROOT-EDIT-QUEUE")
            assert fixture.attempts[root_delivery] == 1
            fixture.retry_after = 1
            passed("A root edit invalidates an already queued thread reply before its delivery retry")

            # An admitted bot turn has no human who can approve its action.
            # Human-initiated approvals are exercised in client_capabilities.py.
            await fixture.send("Calpha", "ask alpha: permission request", user="Ubeta", bot_id="Bbeta")
            await until(lambda: has_post("permission request"), "permission outcome")
            reply = protocol(homes["alpha"], "permission_reply")[-1]["value"]
            assert reply["result"]["outcome"] == {"outcome": "cancelled"}
            assert all("PRIVATE-THOUGHT" not in p["markdown_text"] and "WRONG-SESSION" not in p["markdown_text"]
                       for p in fixture.posts)
            for handshake in protocol(homes["alpha"], "initialize"):
                params = handshake["params"]
                observing = params.get("_meta", {}).get("com.microsoft.amplifier.identity", {}).get("mode") == "observe"
                expected = {} if observing else {"elicitation": {"form": {}, "url": {}}}
                assert params["clientCapabilities"] == expected, params
            assert count(homes["alpha"], "SELECT COUNT(*) FROM permissions") == 0
            passed("Bot turns without a human approver cancel permission; local tools remain disabled and private output stays private")

            stale = await fixture.send("Calpha", "ask alpha: slow STALE-ORIGINAL")
            await until(lambda: has_prompt("alpha", "STALE-ORIGINAL"), "stale turn started")
            await fixture.edit(stale, "Remain silent after EDITED-SOURCE")
            await until(lambda: count(homes["alpha"],
                "SELECT COUNT(*) FROM audit WHERE kind='stale_contribution_withheld'") > 0, "stale reply withheld")
            await until(lambda: has_prompt("alpha", "EDITED-SOURCE"), "edited source processed")
            revised = next(p["input"] for p in protocol(homes["alpha"], "prompt")
                           if "EDITED-SOURCE" in p["input"]["trigger"]["text"])
            old_turn = next(p["input"] for p in protocol(homes["alpha"], "prompt")
                            if "STALE-ORIGINAL" in p["input"]["trigger"]["text"])
            assert "NOT posted to Slack" in revised["continuation_note"]
            receipt = next(r for r in revised["response_delivery"]["items"]
                           if r["job_id"] == old_turn["job_id"])
            assert receipt["status"] == "not_delivered" and receipt["slack_message_id"] is None
            await asyncio.sleep(.3)
            assert not has_post("STALE-ORIGINAL")
            passed("A queued revision receives the withheld-draft notice and delivery receipt through real ACP")

            before = len(fixture.posts)
            await fixture.send("Calpha", "ask alpha: malformed decision")
            await until(lambda: count(homes["alpha"], "SELECT COUNT(*) FROM jobs WHERE status='failed'") >= 1,
                        "invalid ambient answer rejected")
            assert len(fixture.posts) == before
            passed("Invalid ambient participation output is visible as failure and never published")

            for suffix, arguments in (("foreign-team", {"team": "Tother"}), ("foreign-app", {"app": "Aother"})):
                foreign = {**message, "ts": fixture.timestamp(), "text": "ask alpha: " + suffix}
                event_id = await fixture.deliver(foreign, **arguments)
                await until(lambda: "alpha:" + event_id in fixture.acks, "foreign envelope acknowledged")
                assert count(homes["alpha"], "SELECT COUNT(*) FROM messages WHERE ts=?", (foreign["ts"],)) == 0
            forbidden = await fixture.send("Cforbidden", "ask alpha: outside configured rooms")
            await asyncio.sleep(.3)
            assert count(homes["alpha"], "SELECT COUNT(*) FROM messages WHERE ts=?", (forbidden["ts"],)) == 0
            passed("Foreign workspace/app envelopes and disallowed rooms are not ingested")

            await fixture.send("Calpha", "ask alpha: rate-limit")
            await until(lambda: has_post("rate-limit"), "rate limited delivery retried")
            limited = next(p for p in fixture.posts if "rate-limit" in p["markdown_text"])
            assert fixture.attempts[limited["client_msg_id"]] == 2
            passed("An explicit Slack rate limit retries the same delivery after Retry-After")

            fixture.retry_after = 3
            await fixture.send("Calpha", "ask alpha: rate-limit SCOPE-REVOKED")
            await until(lambda: count(homes["alpha"],
                "SELECT COUNT(*) FROM outbox WHERE text LIKE '%SCOPE-REVOKED%' AND status='ready' AND note='Slack rate limit'") == 1,
                "reply waiting before room revocation")
            scoped_delivery = rows(homes["alpha"], "SELECT id FROM outbox WHERE text LIKE '%SCOPE-REVOKED%'")[0][0]
            configuration = await api("alpha", "GET", "config")
            original_channels = list(configuration["slack"]["channels"])
            configuration["slack"]["channels"] = [room for room in original_channels if room != "Calpha"]
            await api("alpha", "PUT", "config", {"expected_revision": configuration["revision"], "config": configuration})
            await until(lambda: count(homes["alpha"],
                "SELECT COUNT(*) FROM outbox WHERE id=? AND status NOT IN ('ready','sending')", (scoped_delivery,)) == 1,
                "removed room's queued reply cancelled")
            await asyncio.sleep(3.2)
            assert not has_post("SCOPE-REVOKED")
            assert fixture.attempts[scoped_delivery] == 1
            configuration = await api("alpha", "GET", "config")
            configuration["slack"]["channels"] = original_channels
            previous_socket = fixture.sockets.get("alpha")
            await api("alpha", "PUT", "config", {"expected_revision": configuration["revision"], "config": configuration})
            await until(lambda: "alpha" in fixture.sockets
                        and fixture.sockets["alpha"] is not previous_socket
                        and not fixture.sockets["alpha"].closed, "updated room scope active")
            fixture.retry_after = 1
            passed("Removing a room cancels its queued delivery and does not retry it after reconfiguration")

            await fixture.send("Calpha", "ask alpha: lost-response")
            await until(lambda: count(homes["alpha"], "SELECT COUNT(*) FROM outbox WHERE status='uncertain'") == 1,
                        "uncertain delivery recorded")
            lost = next(p for p in fixture.posts if "lost-response" in p["markdown_text"])
            await stop("alpha")
            await start("alpha")
            await asyncio.sleep(1.3)
            assert fixture.attempts[lost["client_msg_id"]] == 1
            passed("A service restart does not duplicate a delivery whose HTTP outcome is unknown")

            await fixture.send("Calpha", "ask alpha: after restart")
            await until(lambda: has_post("after restart"), "agent reply after restart")
            after = next(p for p in protocol(homes["alpha"], "prompt")
                         if p["input"]["trigger"]["text"].endswith("after restart"))
            assert after["session_id"] == alpha_prompts[0]["session_id"]
            passed("Restart preserves conversation-to-agent session continuity")

            await fixture.send("Calpha", "ask alpha: stall-child during pause")
            await until(lambda: protocol(homes["alpha"], "descendant"), "endpoint descendant started")
            child = protocol(homes["alpha"], "descendant")[-1]
            await api("alpha", "POST", "lifecycle", {"action": "stop"})
            await until(lambda: "alpha" not in fixture.sockets, "paused socket closed")
            await until(lambda: not live_pid(child["pid"]) and not live_pid(child["child_pid"]),
                        "ACP process tree terminated")
            assert not has_post("stall-child")
            await fixture.send("Calpha", "ask alpha: ignored while stopped")
            await asyncio.sleep(.3)
            assert not has_prompt("alpha", "ignored while stopped")
            passed("Stopping through management closes Slack and kills the in-flight ACP process tree")

            await api("alpha", "POST", "lifecycle", {"action": "start"})
            await until(lambda: "alpha" in fixture.sockets, "resumed Slack connection")
            assert count(homes["alpha"], "SELECT COUNT(*) FROM jobs WHERE status='interrupted'") >= 1
            await fixture.send("Calpha", "ask alpha: safely resumed")
            await until(lambda: has_post("safely resumed"), "resumed reply")
            assert not has_post("stall-child")
            passed("Resuming recovers interrupted work without replaying it or losing the ACP session")

            failure = homes["alpha"] / "endpoint/initialization-failure"
            for operation in ("check", "turn"):
                failure.write_text("once")
                initializations = len(protocol(homes["alpha"], "initialize"))
                prompts = len(protocol(homes["alpha"], "prompt"))
                posts = len(fixture.posts)
                if operation == "check":
                    checked = await api("alpha", "POST", "check", {"target": "acp"})
                    assert checked["ok"] and checked["acp"]["ok"]
                else:
                    await fixture.send("Calpha", "ask alpha: initialization-recovered")
                    await until(lambda: has_post("initialization-recovered"), "reply after startup retry")
                expected_turns = int(operation == "turn")
                assert len(protocol(homes["alpha"], "initialize")) == initializations + 2
                assert len(protocol(homes["alpha"], "prompt")) == prompts + expected_turns
                assert len(fixture.posts) == posts + expected_turns
            assert count(homes["alpha"], "SELECT COUNT(*) FROM audit WHERE kind='acp_initialization_retry'") == 2
            passed("A transient ACP initialization failure is retried once, before any session work or prompt")

            failure.write_text("always")
            initializations = len(protocol(homes["alpha"], "initialize"))
            prompts = len(protocol(homes["alpha"], "prompt"))
            posts = len(fixture.posts)
            await fixture.send("Calpha", "ask alpha: initialization-permanent-failure")
            await until(lambda: count(homes["alpha"], "SELECT COUNT(*) FROM jobs WHERE status='failed' AND trigger LIKE '%initialization-permanent-failure%'") == 1,
                        "permanent initialization failure stops")
            assert len(protocol(homes["alpha"], "initialize")) == initializations + 2
            assert len(protocol(homes["alpha"], "prompt")) == prompts
            assert len(fixture.posts) == posts
            failure.unlink()
            passed("A permanent initialization failure stops after two attempts without sending a prompt")

            initializations = len(protocol(homes["alpha"], "initialize"))
            await fixture.send("Calpha", "ask alpha: fail-prompt-no-retry")
            await until(lambda: count(homes["alpha"], "SELECT COUNT(*) FROM jobs WHERE status='failed' AND trigger LIKE '%fail-prompt-no-retry%'") == 1,
                        "failed prompt stops without a retry")
            assert len(protocol(homes["alpha"], "initialize")) == initializations + 1
            assert len(protocol(homes["alpha"], "prompt")) == prompts + 1
            assert len(fixture.posts) == posts
            passed("A failure after prompting is not retried and cannot duplicate tool work")

            before_children = len(protocol(homes["alpha"], "descendant"))
            await fixture.send("Calpha", "ask alpha: stall-child during shutdown")
            await until(lambda: len(protocol(homes["alpha"], "descendant")) > before_children,
                        "shutdown endpoint started")
            child = protocol(homes["alpha"], "descendant")[-1]
            await stop("alpha")
            await until(lambda: not live_pid(child["pid"]) and not live_pid(child["child_pid"]),
                        "shutdown process tree terminated")
            await fixture.send("Cbeta", "<@Ubeta> still independent")
            await until(lambda: has_post("still independent"), "other installation unaffected")
            passed("Graceful service shutdown tears down endpoint descendants without affecting another adapter")

            status = await api("beta", "GET", "status")
            activity = await api("beta", "GET", "activity")
            conversations = await api("beta", "GET", "conversations")
            assert status["state"]["jobs"]
            assert activity["items"] and conversations["items"]
            assert all(item["kind"] not in ("view_ack", "view_changed", "message") for item in activity["items"])
            serialized = json.dumps({"status": status, "activity": activity, "conversations": conversations})
            assert "test-beta-app" not in serialized and "PRIVATE-BETA" not in serialized
            passed("Authenticated observations expose useful outcomes/routes without transcripts or credentials")
        except Exception:
            for name, home in homes.items():
                if (home / "service.log").exists():
                    print(name + " service diagnostic:\n" + (home / "service.log").read_text()[-4000:], file=sys.stderr)
            raise
        finally:
            for name, process in processes.items():
                if process.returncode is None:
                    process.terminate()
                    try:
                        await asyncio.wait_for(process.wait(), 10)
                    except TimeoutError:
                        process.kill()
                        await process.wait()
                if not log_files[name].closed:
                    log_files[name].close()
            # Defensive cleanup in a failing test, restricted to this fixture's
            # explicitly recorded child processes in its temporary directory.
            for home in homes.values():
                for record in protocol(home, "descendant"):
                    for key in ("pid", "child_pid"):
                        command = Path(f"/proc/{record[key]}/cmdline")
                        owned = command.exists() and str(home).encode() in command.read_bytes()
                        if owned and live_pid(record[key]):
                            try:
                                os.kill(record[key], 9)
                            except ProcessLookupError:
                                pass
            await runner.cleanup()
    print(json.dumps({"kind": "loopback-slack-and-independent-acp-processes",
                      "passed": len(checks), "checks": checks}, indent=2))


if __name__ == "__main__":
    asyncio.run(main())
