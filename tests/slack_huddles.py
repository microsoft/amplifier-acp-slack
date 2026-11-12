"""Two actual Slack adapters + granted MCP + ordinary ACP + loopback Slack.

Run after cargo build: python3 tests/slack_huddles.py (requires aiohttp).
ACP_SLACK_TEST_BINARY selects an immutable candidate/release. Optional
SLACK_HUDDLE_RESULT_DIR retains logs, API payloads and a bounded result receipt.
This is deterministic protocol/behavior coverage, not live Slack UI, notification
preferences, or real-model judgment about which results deserve human attention.
"""

import asyncio
from contextlib import closing
import getpass
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import ssl
import sys
import tempfile
import uuid

from aiohttp import ClientSession, web
from process_integration import BINARY, ROOT, SlackFixture, private_json, rows, until


class Fixture(SlackFixture):
    def __init__(self):
        super().__init__()
        self.revoked = set()
        self.lost = set()
        self.reads = []

    async def api(self, request):
        method = request.match_info["method"]
        body = dict(request.query) if request.method == "GET" else await request.json()
        token = request.headers.get("Authorization", "").removeprefix("Bearer ")
        assert token.startswith("test-")
        name = token.removeprefix("test-").removesuffix("-app")
        channel = body.get("channel")
        if method == "conversations.info":
            return web.json_response({"ok": True, "channel": {"id": channel, "name": channel, "is_im": channel.startswith("D"), "is_mpim": False, "is_private": channel.startswith("G"), "is_member": True, "user": "UHUMAN"}})
        if method == "conversations.members" and channel in self.revoked:
            return web.json_response({"ok": True, "members": ["Ualpha", "Ubeta"]})
        if method in {"conversations.history", "conversations.replies"}:
            self.reads.append({"agent": name, "method": method, **body})
        if method == "chat.getPermalink":
            return web.json_response({"ok": True, "permalink": "https://fixture.slack.com/archives/" + channel + "/p" + body.get("message_ts", "").replace(".", "")})
        if method == "chat.postMessage" and body.get("metadata", {}).get("event_type") != "acp_work_status":
            text = body["markdown_text"]
            lost_kind = "root" if channel == "Clostroot" and not body.get("thread_ts") and "HUDDLE_CASE" in text else "result" if channel == "Clostresult" and text.startswith("RESULT ") else None
            if lost_kind and lost_kind not in self.lost:
                self.lost.add(lost_kind)
                message_id = body["client_msg_id"]
                self.attempts[message_id] = self.attempts.get(message_id, 0) + 1
                self.posts.append({**body, "agent": name})
                message = {"type": "message", "channel": channel, "channel_type": "channel", "text": text, "user": "U" + name, "bot_id": "B" + name, "ts": self.timestamp(), "client_msg_id": message_id}
                if body.get("thread_ts"):
                    message["thread_ts"] = body["thread_ts"]
                if body.get("metadata"):
                    message["metadata"] = body["metadata"]
                self.messages.append(message)
                # Slack committed the message; both HTTP ACK and socket echo are
                # lost. Recovery must use original client_msg_id and history.
                return web.Response(status=503)
        return await super().api(request)


async def main():
    binary = BINARY.resolve(strict=True)
    before = hashlib.sha256(binary.read_bytes()).hexdigest()
    scripts = {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in (Path(__file__).resolve(), ROOT / "tests/huddles_acp_fixture.py", ROOT / "tests/process_integration.py")}
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
    checks, processes, logs, homes = [], {}, {}, {}
    result_dir = Path(os.environ["SLACK_HUDDLE_RESULT_DIR"]).resolve() if os.environ.get("SLACK_HUDDLE_RESULT_DIR") else None
    success = False

    def passed(message):
        checks.append(message)
        print("PASS " + message, flush=True)

    with tempfile.TemporaryDirectory(prefix="slack-huddles-") as temporary:
        temporary = Path(temporary)
        work = temporary / "work"
        work.mkdir()

        async def start(name):
            home = homes[name]
            logs[name] = (home / "service.log").open("ab", buffering=0)
            processes[name] = await asyncio.create_subprocess_exec(str(binary), "serve", "--home", str(home), stdout=logs[name], stderr=logs[name])
            await until(lambda: name in fixture.sockets or processes[name].returncode is not None, name + " connected")
            assert processes[name].returncode is None, (home / "service.log").read_text()

        async def stop(name):
            process = processes.get(name)
            if process and process.returncode is None:
                process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), 10)
                except TimeoutError:
                    process.kill()
                    await process.wait()
            if name in logs:
                logs[name].close()
            await until(lambda: name not in fixture.sockets, name + " disconnected")

        async def api(name, method, path, body=None):
            home = homes[name]
            endpoint = json.loads((home / "endpoints.json").read_text())["base_url"]
            trust = ssl.create_default_context(cafile=str(home / "tls/root-ca.crt"))
            token = (home / "secrets/control-token").read_text().strip()
            async with ClientSession() as client:
                async with client.request(method, endpoint + "/api/v1/" + path, json=body, headers={"Authorization": "Bearer " + token}, ssl=trust) as response:
                    value = await response.json()
                    assert response.status < 300, (response.status, value)
                    return value

        def marker(case, phase):
            return work / (case + "-" + phase + ".json")

        def case_posts(case):
            return [post for post in fixture.posts if post["channel"] == channel_id(case)]

        def channel_id(case):
            return "C" + case.replace("_", "")

        async def begin(case, thread="", channel=None):
            return await fixture.send(channel or channel_id(case), "<@Ualpha> HUDDLE_START " + case + ": review together and bring the meaningful result back here.", thread=thread)

        async def finish(case, original):
            await until(lambda: marker(case, "result").exists(), case + " meaningful-result tool", timeout=65)
            await until(lambda: len([p for p in case_posts(case) if p["markdown_text"].startswith("RESULT ")]) == 1, case + " exact original destination result", timeout=45)
            descriptor = json.loads(marker(case, "ready").read_text())["descriptor"]
            root_ts = descriptor["root_ts"]
            assert descriptor["inspect_url"] == "https://fixture.slack.com/archives/" + original["channel"] + "/p" + root_ts.replace(".", "")
            assert root_ts != original.get("thread_ts") and root_ts != original["ts"]
            posts = case_posts(case)
            roots = [p for p in posts if "HUDDLE_CASE " + case in p["markdown_text"]]
            work_posts = [p for p in posts if p["markdown_text"].startswith("HUDDLE_WORK ")]
            peer = [p for p in posts if p["markdown_text"].startswith("PEER_DONE ")]
            results = [p for p in posts if p["markdown_text"].startswith("RESULT ")]
            assert len(roots) == len(work_posts) == len(peer) == len(results) == 1, posts
            assert not roots[0].get("thread_ts")
            assert "mention <@Ualpha> in this thread" in roots[0]["markdown_text"]
            assert roots[0]["metadata"] == {"event_type": "acp_coordination_thread", "event_payload": {"version": 1, "id": roots[0]["client_msg_id"]}}
            assert work_posts[0].get("thread_ts") == peer[0].get("thread_ts") == root_ts
            assert results[0].get("thread_ts", "") == original.get("thread_ts", "")
            assert work_posts[0]["reply_broadcast"] is False and peer[0]["reply_broadcast"] is False
            assert all(not p.get("reply_broadcast") and "<!channel>" not in p["markdown_text"] and "<@UHUMAN>" not in p["markdown_text"] for p in posts)
            assert all(fixture.attempts.get(p["client_msg_id"]) == 1 for p in posts)
            assert descriptor["origin"]["channel"] == original["channel"]
            assert descriptor["origin"]["thread"] == original.get("thread_ts", "")
            assert descriptor["origin"]["source_ts"] == original["ts"]
            assert descriptor["work_authority"] == "unchanged" and descriptor["completion"] == "not_established"
            # Separate identities exercise separate actual ACP sessions; beta's
            # message comes through the second adapter, never direct fixture post.
            assert peer[0]["agent"] == "beta" and results[0]["agent"] == "alpha"
            await until(lambda: bool(rows(homes["alpha"], "SELECT 1 FROM jobs WHERE json_extract(trigger,'$.ts')=? AND status='pass'", (original["ts"],))), case + " quiet terminal after explicit delivery")
            assert not rows(homes["alpha"], "SELECT 1 FROM audit WHERE kind='direct_reply_reconsidered'")
            observed_prompts = [json.loads(line) for line in (work / "alpha-acp.jsonl").read_text().splitlines()]
            assert len([row for row in observed_prompts if row["kind"] == "prompt" and row["trigger"]["ts"] == original["ts"]]) == 1
            return descriptor

        try:
            for name in ("alpha", "beta"):
                home = temporary / name
                home.mkdir(mode=0o700)
                (home / "secrets").mkdir(mode=0o700)
                homes[name] = home
                with socket.socket() as reservation:
                    reservation.bind(("127.0.0.1", 0))
                    port = reservation.getsockname()[1]
                config = {"schema": 1, "id": str(uuid.uuid4()), "label": name, "revision": 1, "enabled": True, "setup_complete": True, "participation": "mentions", "acp": {"command": sys.executable, "args": [str(ROOT / "tests/huddles_acp_fixture.py"), str(work), name], "cwd": str(work), "timeout_seconds": 120, "client": {"approvals_required": False}}, "slack": {"team_id": "Ttest", "app_id": "A" + name, "bot_user_id": "U" + name, "bot_id": "B" + name, "channels": [], "allow_dms": True, "api_base": f"http://127.0.0.1:{fixture.port}/api/", "loopback_test": True}, "context": {"enabled": True, "owner_slack_user_id": "UHUMAN"}, "attention": {"settle_ms": 10, "minimum_reply_interval_seconds": 0, "max_replies_per_context_per_hour": 100, "max_replies_per_agent_per_hour": 100, "max_decisions_per_agent_per_hour": 100, "max_bot_replies_without_human": 20}, "server": {"bind": "127.0.0.1", "port": port, "owner": getpass.getuser(), "pam_service": "acp-slack-adapter", "public_url": f"https://127.0.0.1:{port}", "embed_origins": []}}
                private_json(home / "adapter.json", config)
                private_json(home / "secrets/slack.json", {"app_token": "test-" + name + "-app", "bot_token": "test-" + name})
                await start(name)

            original = await begin("happy")
            await until(lambda: marker("happy", "inspected").exists(), "quiet coordination and inspectable transcript", timeout=65)
            descriptor = json.loads(marker("happy", "ready").read_text())["descriptor"]
            # This case asks about a later human join. Accepted MCP work can
            # still be awaiting Slack delivery when the earlier peer reply is
            # already readable. Observe its actual delivery and the peer's
            # settled thread before introducing a distinct human stimulus.
            await until(lambda: any(p["markdown_text"].startswith("HUDDLE_WORK happy") for p in case_posts("happy")), "routine coordination actually posted before later human join")
            work_post = next(p for p in case_posts("happy") if p["markdown_text"].startswith("HUDDLE_WORK happy"))
            await until(lambda: bool(rows(homes["alpha"], "SELECT 1 FROM public_speech WHERE id=? AND status='sent'", (work_post["client_msg_id"],))), "routine post has a confirmed delivery receipt")

            def peer_settled():
                observed = rows(homes["beta"], "SELECT 1 FROM messages WHERE channel='Chappy' AND json_extract(body,'$.thread')=? AND json_extract(body,'$.text') LIKE 'HUDDLE_WORK happy%'", (descriptor["root_ts"],))
                terminal = rows(homes["beta"], "SELECT 1 FROM jobs WHERE json_extract(trigger,'$.channel')='Chappy' AND json_extract(trigger,'$.thread')=? AND status IN ('speak','pass')", (descriptor["root_ts"],))
                active = rows(homes["beta"], "SELECT 1 FROM jobs WHERE json_extract(trigger,'$.channel')='Chappy' AND json_extract(trigger,'$.thread')=? AND status IN ('pending','running')", (descriptor["root_ts"],))
                return observed and terminal and not active

            await until(peer_settled, "peer observed routine coordination and its side-thread turn is terminal")
            marker("happy", "before_human_join").write_text(json.dumps({"work_delivery_id": work_post["client_msg_id"], "work_receipt": rows(homes["alpha"], "SELECT status,slack_ts FROM public_speech WHERE id=?", (work_post["client_msg_id"],)), "peer_jobs": rows(homes["beta"], "SELECT id,status,json_extract(trigger,'$.ts') FROM jobs WHERE json_extract(trigger,'$.channel')='Chappy' AND json_extract(trigger,'$.thread')=?", (descriptor["root_ts"],))}))
            snapshot = len(fixture.posts)
            await asyncio.sleep(0.3)
            assert len(fixture.posts) == snapshot and not any(p["markdown_text"].startswith("RESULT ") for p in case_posts("happy"))
            assert any(r["method"] == "conversations.replies" and r.get("ts") == descriptor["root_ts"] for r in fixture.reads)
            passed("Inspect uses the existing read path and publishes nothing; coordination stays in one bot-root thread")
            await fixture.send("Chappy", "<@Ubeta> HUMAN_JOIN happy: I can inspect and join this conversation.", thread=descriptor["root_ts"])
            (work / "happy-join-release").touch()
            await until(lambda: marker("happy", "peer_reuse").exists(), "peer reuses side thread for a later human contribution")
            reused = json.loads(marker("happy", "peer_reuse").read_text())["descriptor"]
            assert reused["reused"] and reused["status"] == "existing_thread" and reused["root_ts"] == descriptor["root_ts"], reused
            await finish("happy", original)
            assert marker("happy", "joined").exists()
            assert len([p for p in case_posts("happy") if p["markdown_text"].startswith("Coordination thread:")]) == 1
            passed("Two actual adapters coordinate quietly; a human joins; one meaningful result returns to the original channel")
            passed("A peer reuses the same side thread when a human joins; no nested or duplicate room is created")

            original_parent = await fixture.send(channel_id("original_thread"), "A human's original discussion, before the task.")
            original = await begin("original_thread", thread=original_parent["ts"])
            await finish("original_thread", original)
            passed("A separate bot-root side thread returns its result to the exact original human thread")

            original_parent = await fixture.send(channel_id("deferred"), "The original request lives in this human thread.")
            original = await begin("deferred", thread=original_parent["ts"])
            await until(lambda: marker("deferred", "awaiting_side").exists(), "original source creates a side thread then ends quietly")
            await until(lambda: bool(rows(homes["alpha"], "SELECT 1 FROM jobs WHERE json_extract(trigger,'$.ts')=? AND status='pass'", (original["ts"],))), "original source has a completed terminal before peer response")
            await until(lambda: len([p for p in case_posts("deferred") if p["markdown_text"].startswith("HUDDLE_WORK ")]) == 1, "initial coordination was sent")
            await stop("alpha")
            await start("alpha")
            (work / "deferred-peer-release").touch()
            await finish("deferred", original)
            initial = json.loads(marker("deferred", "awaiting_side").read_text())
            final = json.loads(marker("deferred", "result").read_text())
            assert initial["original_session"] != final["side_session"]
            passed("After the original turn ends and the adapter restarts, a peer triggers a distinct side-thread ACP session and one result returns to the saved human thread")

            for case in ("lost_root", "lost_result"):
                original = await begin(case)
                await finish(case, original)
                lost_post = next(p for p in case_posts(case) if ("HUDDLE_CASE" in p["markdown_text"] if case == "lost_root" else p["markdown_text"].startswith("RESULT ")))
                await until(lambda: bool(rows(homes["alpha"], "SELECT 1 FROM public_speech WHERE id=? AND status='sent' AND note LIKE '%history%'", (lost_post["client_msg_id"],))), case + " exact committed send reconciled from history", timeout=45)
                passed(case.replace("_", " ") + " committed HTTP/socket ACK loss reconciles original delivery without a second post")
            assert fixture.lost == {"root", "result"}

            await begin("private", channel="Dprivate")
            await until(lambda: marker("private", "denied").exists(), "private source refusal")
            assert not any(p["channel"] == "Dprivate" for p in fixture.posts)
            passed("A private source cannot silently become a room-visible side conversation")

            for case in ("edited", "revoked", "stopped"):
                original = await begin(case)
                await until(lambda: marker(case, "ready").exists(), case + " root before authority change", timeout=45)
                if case == "edited":
                    await fixture.edit(original, "The original request is superseded; do not publish the old result.")
                    # An edit need not interrupt an active ACP turn. Its exact
                    # source revision must be observed before any new delivery.
                    await until(lambda: bool(rows(homes["alpha"], "SELECT 1 FROM messages WHERE channel=? AND ts=? AND json_extract(body,'$.revision')!=?", (original["channel"], original["ts"], original["ts"]))), "edited source revision retained")
                elif case == "revoked":
                    fixture.revoked.add("C" + case)
                else:
                    await api("alpha", "POST", "lifecycle", {"action": "stop"})
                    await until(lambda: "alpha" not in fixture.sockets, "Stop closes socket")
                (work / (case + "-release")).touch()
                if case in {"edited", "revoked"}:
                    await until(lambda: marker(case, "denied").exists(), case + " source rejects result")
                await asyncio.sleep(0.5)
                assert not any("MUST_NOT_SEND" in p["markdown_text"] for p in fixture.posts)
                passed(case + " authority change prevents a stale result; no automatic replacement or replay")
                if case == "stopped":
                    await api("alpha", "POST", "lifecycle", {"action": "start"})
                    await until(lambda: "alpha" in fixture.sockets, "explicit test restart")

            # Installation-owned thread-only policy is changed only while this
            # disposable adapter is stopped, never through a fabricated MCP grant.
            await stop("alpha")
            config = json.loads((homes["alpha"] / "adapter.json").read_text())
            config["context"]["thread_only_grants"] = True
            config["revision"] += 1
            private_json(homes["alpha"] / "adapter.json", config)
            await start("alpha")
            parent = await fixture.send(channel_id("thread_only"), "Existing thread-only discussion.")
            await begin("thread_only", thread=parent["ts"])
            await until(lambda: marker("thread_only", "denied").exists(), "thread-only scope refusal")
            assert not case_posts("thread_only")
            passed("Thread-only source authority refuses a new parent-channel root")
            assert hashlib.sha256(binary.read_bytes()).hexdigest() == before, "Candidate binary changed during the run"
            assert all(hashlib.sha256(Path(path).read_bytes()).hexdigest() == digest for path, digest in scripts.items()), "Fixture source changed during the run"
            success = True
        except BaseException:
            for name, home in homes.items():
                print("\n" + name + " service log:\n" + (home / "service.log").read_text(), file=sys.stderr)
                if (home / "state.sqlite3").exists():
                    print(rows(home, "SELECT kind,data FROM audit ORDER BY seq DESC LIMIT 12"), file=sys.stderr)
            for log in work.glob("*-acp.jsonl"):
                print(log.name + "\n" + "\n".join(log.read_text().splitlines()[-10:]), file=sys.stderr)
            raise
        finally:
            for name in list(processes):
                await stop(name)
            await runner.cleanup()
            if result_dir:
                result_dir.mkdir(parents=True, exist_ok=True)
                shutil.copytree(work, result_dir / "fixture", dirs_exist_ok=True)
                for name, home in homes.items():
                    shutil.copy2(home / "service.log", result_dir / (name + "-service.log"))
                    if (home / "state.sqlite3").exists():
                        # A main-file copy loses still-uncheckpointed WAL pages
                        # held by read observers. Backup reads one coherent DB.
                        source_uri = (home / "state.sqlite3").as_uri() + "?mode=ro"
                        with closing(sqlite3.connect(source_uri, uri=True)) as source:
                            with closing(sqlite3.connect(result_dir / (name + "-state.sqlite3"))) as destination:
                                source.backup(destination)
                                assert destination.execute("PRAGMA integrity_check").fetchone() == ("ok",)
                (result_dir / "slack.json").write_text(json.dumps({"posts": fixture.posts, "messages": fixture.messages, "attempts": fixture.attempts, "reads": fixture.reads}, indent=2))
                (result_dir / "completion.json").write_text(json.dumps({"passed": success, "binary": str(binary), "binary_sha256": before, "fixture_sha256": scripts, "cases": checks, "limits": ["Slack API and ACP reasoning are deterministic loopback fixtures", "Actual adapter processes, granted MCP, SQLite journals and send loops execute", "No live Slack, real model, notification/UI, or social-judgment claim"]}, indent=2))


if __name__ == "__main__":
    asyncio.run(main())
