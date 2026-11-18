"""Two actual Slack adapters + granted MCP + ordinary ACP + loopback Slack.

Run after cargo build: python3 tests/owner_message.py (requires aiohttp).
ACP_SLACK_TEST_BINARY selects an immutable candidate/release.
This is deterministic protocol/behavior coverage of the owner-message tool, not
a real Slack workspace, a person's notification preferences, or a real model's
judgment about what deserves its owner's attention.
"""

import asyncio
import getpass
import json
from pathlib import Path
import socket
import sys
import tempfile
import uuid

from aiohttp import web
from process_integration import BINARY, ROOT, SlackFixture, private_json, rows, until


class Fixture(SlackFixture):
    """Each adapter has one direct conversation with the owner: D<name>owner."""

    def __init__(self):
        super().__init__()
        self.opened = []
        self.lost = False

    async def api(self, request):
        method = request.match_info["method"]
        body = dict(request.query) if request.method == "GET" else await request.json()
        name = request.headers.get("Authorization", "").removeprefix("Bearer ").removeprefix("test-").removesuffix("-app")
        channel = body.get("channel")
        if method == "conversations.info":
            return web.json_response({"ok": True, "channel": {"id": channel, "name": channel, "is_im": channel.startswith("D"), "is_mpim": False, "is_private": False, "is_member": True, "user": "UHUMAN"}})
        if method == "conversations.members":
            members = ["Ualpha", "UHUMAN"] if channel.startswith("D") else ["Ualpha", "Ubeta", "UPEER"] if channel == "Cabsent" else ["Ualpha", "Ubeta", "UHUMAN", "UPEER"]
            return web.json_response({"ok": True, "members": members})
        if method == "users.info":
            return web.json_response({"ok": True, "user": {"id": body["user"], "is_bot": body["user"] in ("Ualpha", "Ubeta"), "profile": {"display_name": body["user"][1:]}}})
        if method == "chat.getPermalink":
            return web.json_response({"ok": True, "permalink": "https://fixture.slack.com/archives/" + channel + "/p" + body.get("message_ts", "").replace(".", "")})
        if method == "conversations.open":
            self.opened.append({"agent": name, **body})
            if name == "beta":
                return web.json_response({"ok": False, "error": "missing_scope", "needed": "im:write"})
            assert body == {"users": "UHUMAN", "return_im": True}, body
            return web.json_response({"ok": True, "channel": {"id": "D" + name + "owner", "is_im": True, "user": "UHUMAN"}})
        if method == "chat.postMessage" and body.get("markdown_text", "").startswith("PRIVATE lost") and not self.lost:
            # Slack committed the message; both its HTTP acknowledgment and the
            # socket echo are lost. Recovery must use the original client_msg_id.
            self.lost = True
            self.attempts[body["client_msg_id"]] = self.attempts.get(body["client_msg_id"], 0) + 1
            self.posts.append({**body, "agent": name})
            self.messages.append({"type": "message", "channel": channel, "channel_type": "im", "text": body["markdown_text"], "user": "U" + name, "bot_id": "B" + name, "ts": self.timestamp(), "client_msg_id": body["client_msg_id"]})
            return web.Response(status=503)
        return await super().api(request)

    async def deliver(self, message, event_id=None, *, team="Ttest", app=None):
        # Slack sends a direct conversation's events only to the app that is in it.
        channel = message.get("channel", "")
        if channel.startswith("D"):
            event_id = event_id or str(uuid.uuid4())
            ws = self.sockets.get(channel.removeprefix("D").removesuffix("owner"))
            if ws and not ws.closed:
                await ws.send_json({"envelope_id": event_id, "type": "events_api", "payload": {"team_id": team, "api_app_id": "A" + channel.removeprefix("D").removesuffix("owner"), "event_id": event_id, "event": message}})
            return event_id
        return await super().deliver(message, event_id, team=team, app=app)


async def main():
    binary = BINARY.resolve(strict=True)
    fixture = Fixture()
    app = web.Application()
    app.router.add_get("/api/{method}", fixture.api)
    app.router.add_post("/api/{method}", fixture.api)
    app.router.add_get("/socket/{name}", fixture.socket)
    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", 0)
    await site.start()
    fixture.port = site._server.sockets[0].getsockname()[1]
    processes, logs, homes = {}, {}, {}

    def passed(message):
        print("PASS " + message, flush=True)

    with tempfile.TemporaryDirectory(prefix="owner-message-") as temporary:
        temporary = Path(temporary)
        work = temporary / "work"
        work.mkdir()

        def marker(case):
            return work / (case + ".json")

        def private(text):
            return [post for post in fixture.posts if post["markdown_text"].startswith(text)]

        async def ask(case, *, agent="alpha", channel=None, user="UHUMAN", thread="", bot_id=None):
            channel = channel or "C" + case
            source = await fixture.send(channel, f"<@U{agent}> OWNER_NOTE {case}: tell your owner privately.", user=user, thread=thread, bot_id=bot_id)
            await until(lambda: marker(case).exists(), case + " tool calls", timeout=75)
            await until(lambda: any(post["markdown_text"] == "DONE " + case and post["channel"] == channel for post in fixture.posts), case + " ordinary reply in the source conversation", timeout=45)
            return source, json.loads(marker(case).read_text())

        try:
            for name in ("alpha", "beta"):
                home = temporary / name
                home.mkdir(mode=0o700)
                (home / "secrets").mkdir(mode=0o700)
                homes[name] = home
                with socket.socket() as reservation:
                    reservation.bind(("127.0.0.1", 0))
                    port = reservation.getsockname()[1]
                config = {"schema": 1, "id": str(uuid.uuid4()), "label": name, "revision": 1, "enabled": True, "setup_complete": True, "participation": "mentions", "acp": {"command": sys.executable, "args": [str(ROOT / "tests/owner_message_acp_fixture.py"), str(work), name], "cwd": str(work), "timeout_seconds": 120, "client": {"approvals_required": False}}, "slack": {"team_id": "Ttest", "app_id": "A" + name, "bot_user_id": "U" + name, "bot_id": "B" + name, "channels": [], "allow_dms": True, "api_base": f"http://127.0.0.1:{fixture.port}/api/", "loopback_test": True}, "context": {"enabled": True, "owner_slack_user_id": "UHUMAN"}, "attention": {"settle_ms": 10, "minimum_reply_interval_seconds": 0, "max_replies_per_context_per_hour": 100, "max_replies_per_agent_per_hour": 100, "max_decisions_per_agent_per_hour": 100, "max_bot_replies_without_human": 20}, "server": {"bind": "127.0.0.1", "port": port, "owner": getpass.getuser(), "pam_service": "acp-slack-adapter", "public_url": f"https://127.0.0.1:{port}", "embed_origins": []}}
                private_json(home / "adapter.json", config)
                private_json(home / "secrets/slack.json", {"app_token": "test-" + name + "-app", "bot_token": "test-" + name})
                logs[name] = (home / "service.log").open("ab", buffering=0)
                processes[name] = await asyncio.create_subprocess_exec(str(binary), "serve", "--home", str(home), stdout=logs[name], stderr=logs[name])
                await until(lambda: name in fixture.sockets or processes[name].returncode is not None, name + " connected")
                assert processes[name].returncode is None, (home / "service.log").read_text()

            source, happy = await ask("happy")
            sent = private("PRIVATE happy")
            assert happy["listed"] and len(sent) == 1, (happy, sent)
            assert sent[0]["channel"] == "Dalphaowner" and "thread_ts" not in sent[0] and sent[0]["agent"] == "alpha"
            assert sent[0]["markdown_text"] == "PRIVATE happy: the review needs your decision.\n\n[From this conversation](https://fixture.slack.com/archives/Chappy/p" + source["ts"].replace(".", "") + ")"
            assert sent[0]["parse"] == "none" and sent[0]["unfurl_links"] is False
            first, second, changed, final = (happy[key]["structuredContent"] for key in ("first", "second", "changed", "final"))
            assert first["accepted"] and first["recipient"] == "owner" and first["conversation_id"] == "Dalphaowner"
            assert [r["id"] for r in first["receipts"]] == [r["id"] for r in second["receipts"]] == [r["id"] for r in changed["receipts"]] == [sent[0]["client_msg_id"]]
            assert final["status"] == "sent" and final["receipts"][0]["slack_message_id"]
            assert fixture.attempts[sent[0]["client_msg_id"]] == 1 and not private("CHANGED")
            assert fixture.opened[0] == {"agent": "alpha", "users": "UHUMAN", "return_im": True}
            passed("The owner asks in a channel and gets one private message with a link back; repeating the request, even with other words, sends nothing more")

            root = await fixture.send("Cpeer", "A thread that a colleague started.", user="UPEER")
            source, peer = await ask("peer", channel="Cpeer", user="UPEER", thread=root["ts"])
            sent = private("PRIVATE peer")
            assert len(sent) == 1 and sent[0]["channel"] == "Dalphaowner" and "thread_ts" not in sent[0]
            assert sent[0]["markdown_text"].endswith("(https://fixture.slack.com/archives/Cpeer/p" + source["ts"].replace(".", "") + ")")
            assert not any(post["channel"].startswith("D") and post["channel"] != "Dalphaowner" for post in fixture.posts)
            passed("A colleague's request in a thread reaches the owner, never the colleague, and links the message that asked")

            _, absent = await ask("absent", user="UPEER")
            refused = absent["first"]
            assert refused["isError"] and refused["structuredContent"]["accepted"] is False
            assert "not a member of this conversation" in refused["structuredContent"]["reason"]
            assert not private("PRIVATE absent") and absent["final"] is None
            passed("Nothing is sent about a conversation the owner is not a member of")

            _, named = await ask("named")
            assert named["attempt"]["isError"] and "recipient is fixed" in named["attempt"]["structuredContent"]["reason"]
            assert not private("MUST_NOT_SEND")
            passed("A call that names another recipient is refused")

            _, limit = await ask("limit")
            accepted = [result["structuredContent"]["accepted"] for result in limit["results"]]
            assert accepted == [True, True, True, False], limit
            await until(lambda: len(private("PRIVATE limit")) == 3, "three bounded owner messages")
            await asyncio.sleep(0.3)
            assert [post["markdown_text"].split("\n")[0] for post in private("PRIVATE limit")] == ["PRIVATE limit 0", "PRIVATE limit 1", "PRIVATE limit 2"]
            passed("One response sends its owner at most three messages, in order")

            _, direct = await ask("direct", channel="Dalphaowner")
            assert direct["listed"] is False and direct["attempt"] == {"http": 403}
            assert not private("MUST_NOT_SEND")
            earlier = [m for m in direct["history"] if m["text"].startswith("PRIVATE ")]
            assert len(earlier) == 5 and all(m["sender"] == "Ualpha" and m["bot"] for m in earlier), direct["history"]
            passed("In the owner's own direct conversation the tool is neither listed nor callable, and the earlier private messages are in that conversation's history as the agent's own")

            _, other = await ask("agent", user="Ubeta", bot_id="Bbeta")
            assert other["listed"] is False and other["attempt"] == {"http": 403}
            assert not private("MUST_NOT_SEND")
            passed("Another agent's message cannot lead to a private message to the owner")

            _, scope = await ask("scope", agent="beta")
            refused = scope["first"]["structuredContent"]
            assert refused["accepted"] is False and "im:write" in refused["reason"]
            assert not private("PRIVATE scope") and not rows(homes["beta"], "SELECT 1 FROM public_speech WHERE channel LIKE 'D%'")
            passed("An app without permission to start a direct message says so and sends nothing")

            _, lost = await ask("lost")
            sent = private("PRIVATE lost")
            assert len(sent) == 1 and lost["first"]["structuredContent"]["accepted"]
            await until(lambda: rows(homes["alpha"], "SELECT status FROM public_speech WHERE id=?", (sent[0]["client_msg_id"],)) == [("sent",)], "lost acknowledgment reconciled from the direct conversation's history", timeout=90)
            assert fixture.attempts[sent[0]["client_msg_id"]] == 1
            assert len([m for m in fixture.messages if m["text"].startswith("PRIVATE lost")]) == 1
            assert rows(homes["alpha"], "SELECT COUNT(*) FROM audit WHERE kind='owner_message_ready'") == [(6,)]
            passed("A lost acknowledgment is reconciled from Slack by its original identity, not sent again")
        finally:
            for name, process in processes.items():
                if process.returncode is None:
                    process.terminate()
                    try:
                        await asyncio.wait_for(process.wait(), 10)
                    except TimeoutError:
                        process.kill()
                        await process.wait()
                logs[name].close()
            if sys.exc_info()[0] is not None:
                for name, home in homes.items():
                    print(f"--- {name} service log (tail)\n" + (home / "service.log").read_text()[-3000:], file=sys.stderr)
                    acp = work / (name + "-acp.jsonl")
                    if acp.exists():
                        print(f"--- {name} ACP fixture log (tail)\n" + acp.read_text()[-3000:], file=sys.stderr)
    await runner.cleanup()
    print("Owner message fixture passed")


if __name__ == "__main__":
    asyncio.run(main())
