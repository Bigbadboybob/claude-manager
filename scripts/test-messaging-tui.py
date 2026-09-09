#!/usr/bin/env python3
"""Isolated TUI smoke test; no live sessions or messages are touched.

Requires pyte and Pillow in the test interpreter. Build the release TUI/daemon
first, or set CM_CHAT_BIN_DIR to a debug build directory. Screenshots and logs are written under /tmp/cm-chat-B-* and
/tmp/cm-messaging-B-preview*. The test owns a temporary HOME and private sockets.
"""

import fcntl, json, os, pathlib, pty, select, socket, struct, subprocess, tempfile, termios, time, signal
import pyte
from PIL import Image, ImageDraw, ImageFont

root = pathlib.Path(__file__).resolve().parents[1]
release = pathlib.Path(os.environ.get("CM_CHAT_BIN_DIR", str(pathlib.Path.home() / ".cm/shared-target/release")))
with tempfile.TemporaryDirectory(prefix="cm-chat-B-preview-") as tmp:
    home = pathlib.Path(tmp)
    (home / ".cm").mkdir()
    (home / ".cm/operator-token").write_text("preview-token")
    (home / ".cm/daemon.toml").write_text(
        "mcp_server_path = "
        + json.dumps(str(root / "mcp_server/server.py"))
        + '\napi_url = "http://127.0.0.1:1"\n'
    )
    env = {k: v for k, v in os.environ.items() if not k.startswith("CM_")}
    env.update(
        HOME=tmp,
        CM_DAEMON_SOCKET=str(home / "d.sock"),
        CM_TUI_SOCKET=str(home / "t.sock"),
        CM_OPERATOR_TOKEN="preview-token",
        CM_MCP_SERVER=str(root / "mcp_server/server.py"),
        CM_API_URL="http://127.0.0.1:1",
        CM_API_TOKEN="test",
        TERM="xterm-256color",
    )
    env.pop("NO_COLOR", None)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    screen = pyte.Screen(80, 24)
    stream = pyte.ByteStream(screen)

    def rpc(method, params={}, *, session=None, raw=False):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(8)
            s.connect(str(home / "d.sock"))
            body = json.dumps(
                {
                    "id": "preview",
                    "caller": {"session_uid": session} if session else {"token_id": "preview-token"},
                    "method": method if raw else "messaging." + method,
                    "params": params,
                }
            ).encode()
            s.sendall(struct.pack(">I", len(body)) + body)

            def exact(n):
                b = b""
                while len(b) < n:
                    c = s.recv(n - len(b))
                    assert c
                    b += c
                return b

            r = json.loads(exact(struct.unpack(">I", exact(4))[0]))
            assert r["ok"], r
            return r["result"]

    def drain(seconds=0.3):
        data = b""
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if select.select([master], [], [], max(0, until - time.monotonic()))[0]:
                try:
                    c = os.read(master, 262144)
                except OSError:
                    break
                stream.feed(c)
                data += c
        return data

    def key(seq, seconds=0.3):
        os.write(master, seq)
        return drain(seconds)

    def visible():
        return "\n".join(screen.display)

    def wait_text(text, timeout=8):
        until = time.monotonic() + timeout
        while text not in visible() and time.monotonic() < until:
            drain(0.1)
        assert text in visible(), visible()

    def shot(name):
        font = ImageFont.truetype(
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 16
        )
        image = Image.new("RGB", (screen.columns * 10, screen.lines * 20), "#101520")
        d = ImageDraw.Draw(image)
        colors = {
            "default": "#dde5f2",
            "white": "#ffffff",
            "black": "#000000",
            "red": "#ff5555",
            "green": "#55ff55",
            "yellow": "#ffff55",
            "cyan": "#55ffff",
            "magenta": "#ff55ff",
            "blue": "#5555ff",
            "brown": "#aaaa00",
        }

        def color(s, bg=False):
            return (
                "#101520"
                if s == "default" and bg
                else colors.get(s, "#" + s if len(s) == 6 else "#dde5f2")
            )

        for y in range(screen.lines):
            for x in range(screen.columns):
                c = screen.buffer[y][x]
                d.rectangle(
                    (x * 10, y * 20, x * 10 + 9, y * 20 + 19), fill=color(c.bg, True)
                )
                d.text((x * 10, y * 20), c.data, font=font, fill=color(c.fg))
        image.save("/tmp/" + name + ".png")

    daemon = tui = None
    with open("/tmp/cm-messaging-B-preview-daemon.log", "wb") as log:
        try:
            daemon = subprocess.Popen(
                [str(release / "cm-daemon")],
                env=env,
                cwd=tmp,
                stdout=log,
                stderr=log,
                start_new_session=True,
            )
            until = time.monotonic() + 10
            while not (home / "d.sock").exists():
                assert daemon.poll() is None and time.monotonic() < until
                time.sleep(0.05)
            initial = rpc("norms", {"action": "read"})
            initial_text = initial["text"]
            # Registered fixture identities only: no model/PTY sessions are spawned.
            fixtures = [{"uid": uid, "label": name} for uid, name in [("alpha", "Alpha"), ("beta", "Beta")]]
            rpc("tui.update_sessions_snapshot", {"sessions": fixtures}, raw=True)
            people = {p["session_uid"]: p["id"] for p in rpc("people")["items"] if p.get("session_uid")}
            rpc("send", {"channel": "general", "name": "Alpha", "body": "Oldest full message.\nSecond paragraph stays visible.", "request_id": "timeline-a"}, session="alpha")
            rpc("send", {"channel": "general", "name": "Beta", "body": "Newest full message.", "mentions": ["owner"], "request_id": "timeline-b"}, session="beta")
            tui = subprocess.Popen(
                [str(release / "claude-manager-tui")],
                env=env,
                cwd=tmp,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                start_new_session=True,
            )
            os.close(slave)
            drain(2)
            key(b"\x1bm", 1)
            assert "Messages" in screen.display[0]
            key(b"\t", 0.4)
            wait_text("Second paragraph stays visible.")
            assert visible().index("Oldest full message.") < visible().index("Newest full message.")
            assert "● @you" in visible()
            assert not any("Alpha" in row[:24] for row in screen.display[1:-3])
            shot("cm-chat-conversation-timeline")
            # Owner creates a channel with restricted editing, then manages pins
            # and changes its display name/description without changing its address.
            key(b"nrelease-notes\tRelease room\tUpdates shared by agents.\t\t\r", 0.8)
            wait_text("#Release room")
            channel = rpc("channels", {"action": "get", "path": "release-notes"})
            assert channel["created_by"] == "owner" and channel["allow_agent_edits"] is False, channel
            key(b"cPinned reference for the release.\x13", 0.8)
            wait_text("Pinned reference for the release.")
            key(b"p", 0.8)
            wait_text("pinned")
            pins = rpc("pins", {"channel": "release-notes"})
            assert len(pins["items"]) == 1 and pins["items"][0]["body"] == "Pinned reference for the release.", pins
            key(b"P", 0.8)
            wait_text("Pinned messages")
            shot("cm-chat-channel-pins")
            key(b"P", 0.5)
            key(b"S", 0.3)
            wait_text("Channel settings")
            shot("cm-chat-channel-settings")
            key(b"\x7f" * len("Release room") + b"Release review\t" + b"\x7f" * len("Updates shared by agents.") + b"Agent coordination for release review.\t" + b"\x7f" * 2 + b"yes\tAlpha\r", 0.8)
            wait_text("#Release review")
            wait_text("Agent coordination for release review.")
            changed = rpc("channels", {"action": "get", "path": "release-notes"})
            assert changed["id"] == channel["id"] and changed["allow_agent_edits"] is True and people["alpha"] in changed["admins"], changed
            assert rpc("pins", {"channel": "release-notes"})["items"][0]["id"] == pins["items"][0]["id"]
            key(b"p", 0.8)
            assert rpc("pins", {"channel": "release-notes"})["items"] == []
            key(b"P", 0.6)
            assert "Pinned reference for the release." not in visible()
            key(b"P", 0.6)
            wait_text("Pinned reference for the release.")
            shot("cm-chat-channel-renamed")
            # Channel norms use the same editor, with their own history and scope.
            key(b"N", 0.6)
            wait_text("#release-notes norms")
            wait_text("No channel norms yet")
            key(b"\r", 0.4)
            key(b"pUse short release notes.\x13", 0.4)
            wait_text("Explain norms change")
            key(b"Define channel conventions\r", 0.3)
            wait_text("Review norms change")
            key(b"\x13", 0.7)
            local_norms = rpc("norms", {"channel": "release-notes"})
            assert local_norms["text"] == "Use short release notes.", local_norms
            assert rpc("norms")["text"] == initial_text
            key(b"d", 0.5)
            wait_text("+Use short release notes.")
            shot("cm-chat-channel-norms")
            key(b"b", 0.5)
            wait_text("#Release review")
            key(b"bgeneral\r", 0.7)
            wait_text("Second paragraph stays visible.")
            # Public channel preview does not join; Owner joins/leaves explicitly.
            rpc("tui.update_sessions_snapshot", {"sessions": fixtures}, raw=True)
            rpc("channels", {"action": "create", "path": "membership-test", "description": "Membership and mentions", "request_id": "member-create"}, session="alpha")
            rpc("channels", {"action": "join", "path": "membership-test", "request_id": "beta-join"}, session="beta")
            key(b"g", 0.6)
            key(b"bmembership\r", 0.6)
            wait_text("Preview")
            assert rpc("channels", {"action": "get", "path": "membership-test"})["joined"] is False
            key(b"c", 0.3)
            assert "Join before posting" in visible(), visible()
            key(b"J", 0.8)
            wait_text("Joined")
            assert rpc("channels", {"action": "get", "path": "membership-test"})["joined"] is True
            key(b"u", 0.5)
            wait_text("Channel members")
            assert "Alpha" in visible() and "Beta" in visible() and "Owner" in visible(), visible()
            key(b"\x1b", 0.3)
            key(b"c@Al", 0.4)
            wait_text("@Alpha")
            shot("cm-chat-mention-completion")
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 16, 48, 0, 0))
            screen.resize(16, 48)
            os.kill(tui.pid, signal.SIGWINCH)
            drain(0.4)
            wait_text("@Alpha")
            shot("cm-chat-mention-narrow")
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            screen.resize(24, 80)
            os.kill(tui.pid, signal.SIGWINCH)
            drain(0.4)
            key(b"\rplease review.\x13", 0.8)
            wait_text("1 messages")
            direct = rpc("read", {"channel": "membership-test"})["items"][-1]
            assert direct["data"]["mentions"] == [people["alpha"]], direct
            key(b"c@here", 0.4)
            wait_text("all joined")
            key(b"\rready for review.\x13", 0.8)
            wait_text("2 messages")
            broadcast = rpc("read", {"channel": "membership-test"})["items"][-1]
            assert broadcast["data"]["mention_here"] is True, broadcast
            assert set(broadcast["data"]["mention_recipients"]) == {"owner", people["alpha"], people["beta"]}
            # Editing the visible mention removes its notification target.
            key(b"c@Al\r\x7f\x7f\x13", 0.8)
            wait_text("3 messages")
            edited = rpc("read", {"channel": "membership-test"})["items"][-1]
            assert edited["data"]["mentions"] == [], edited
            key(b"L", 0.8)
            wait_text("Preview")
            assert rpc("channels", {"action": "get", "path": "membership-test"})["joined"] is False
            shot("cm-chat-channel-preview")
            key(b"bmembership", 0.4)
            shot("cm-chat-channel-browser")
            key(b"\x1b", 0.2)
            key(b"bgeneral\r", 0.7)
            wait_text("Second paragraph stays visible.")
            # Go beyond a read page, navigate with k, and keep live updates
            # while reading earlier history without jumping to the newest post.
            for i in range(25):
                rpc("send", {"channel": "general", "body": f"History item {i:02}", "request_id": f"history-{i}"})
            key(b"g", 0.8)
            key(b"k" * 27, 1.0)
            wait_text("Oldest full message.")
            for i in range(25):
                rpc("send", {"channel": "general", "body": f"Burst item {i:02}", "request_id": f"burst-{i}"})
            rpc("send", {"channel": "general", "body": "Arrived while reading history", "request_id": "while-reading"})
            drain(3.5)
            assert "Oldest full message." in visible(), "Refresh moved the selected message"
            wait_text("53 messages")
            key(b"j" * 60, 0.8)
            wait_text("Arrived while reading history")
            key(b"dAlpha\t", 0.6)
            key(b"\x7f" * 5 + b"Beta\t", 0.6)
            wait_text("Recipients: Alpha, Beta")
            shot("cm-chat-conversation-picker")
            key(b"\r", 0.8)
            assert rpc("dms")["items"] == [], "Opening a group draft created a DM"
            key(b"Group kickoff.\x13", 0.8)
            wait_text("Group kickoff.")
            groups = rpc("dms")["items"]
            assert len(groups) == 1 and groups[0]["group"] and len(groups[0]["members"]) == 3, groups
            rpc("tui.update_sessions_snapshot", {"sessions": fixtures}, raw=True)
            rpc("send", {"conversation": groups[0]["id"], "body": "Group reply to Owner.", "request_id": "group-reply"}, session="alpha")
            key(b"g", 0.8)
            wait_text("● DM")
            shot("cm-chat-conversation-group")
            key(b"\t" + b"j" * 30 + b"k\r", 0.6)
            assert not any("Alpha" in row[:24] for row in screen.display[1:-3]), visible()
            key(b"\r", 0.3)
            assert any("Alpha" in row[:24] for row in screen.display[1:-3]), visible()
            key(b"k" * 30 + b"jj\r", 0.8)
            assert "Shared norms" in visible()
            for _ in range(28):
                key(b"j", 0.04)
            key(b"\r", 0.6)
            assert not rpc("open")["context_status"]["changed"], (
                "complete Owner acknowledgement failed"
            )
            key(b"p")
            wait_text("Norms draft")
            key(b"\x1b[F")
            key(b"\rSandbox convention.")
            key(b"\x13")
            wait_text("Explain norms change")
            key(b"Explain sandbox convention.\r")
            wait_text("Review norms change")
            shot("cm-chat-B-norms-preview")
            key(b"\x13", 0.8)
            changed = rpc("norms", {"action": "read"})
            assert "Sandbox convention." in changed["text"], visible()
            key(b"h", 0.6)
            key(b"jv", 0.8)
            key(b"\r")
            wait_text("Review norms change")
            key(b"\x13", 0.8)
            reverted = rpc("norms", {"action": "read"})
            assert (
                reverted["text"] == initial_text
                and reverted["revision"] != initial["revision"]
            ), "revert failed"
            key(b"p")
            wait_text("Norms draft")
            key(b"\x1b[F")
            key(b"\rDraft stays.")
            key(b"\x13")
            wait_text("Explain norms change")
            key(b"Owner draft\r")
            rpc(
                "norms",
                {
                    "action": "publish",
                    "text": initial_text + "Competing convention.\n",
                    "summary": "Concurrent edit",
                    "expected_revision": reverted["revision"],
                    "request_id": "conflict",
                },
            )
            key(b"\x13", 0.8)
            wait_text("Concurrent norms edit")
            shot("cm-chat-B-norms-conflict")
            saved = json.loads((home / ".cm/messages-owner-ui.json").read_text())
            assert (
                "Draft stays."
                in next(iter(saved["management"]["norms"].values()))["text"]
            )
            key(b"b", 0.8)
            assert "Norms draft" in visible()
            key(b"\x1b")
            key(b"D")
            assert not json.loads((home / ".cm/messages-owner-ui.json").read_text())[
                "management"
            ]["norms"]
            key(b"\t")
            key(b"j\r", 0.5)
            assert "Monitors" in visible()
            key(b"n")
            key(b"\x7f" * 3 + b"#general\t")
            key(b"\x7f" * 4 + b"continuous\t\t\t")
            key(b"\x7f" * 2 + b"yes\t")
            shot("cm-chat-B-monitor-form")
            key(b"\r", 0.8)
            monitors = rpc("monitors")["items"]
            assert len(monitors) == 1 and monitors[0]["mode"] == "continuous", monitors
            rpc(
                "send",
                {
                    "channel": "general",
                    "body": "Sandbox monitor hit",
                    "request_id": "hit",
                },
            )
            drain(3.5)
            key(b"\r", 0.6)
            wait_text("Sandbox monitor hit")
            shot("cm-chat-B-monitor-results")
            key(b"a", 0.6)
            assert (
                rpc("monitors", {"action": "get", "monitor_id": monitors[0]["id"]})[
                    "monitor"
                ]["unacknowledged"]
                == 0
            )
            key(b"x", 0.6)
            key(b"D", 0.6)
            assert rpc("monitors")["items"] == []
            key(b"\t")
            key(b"j\r", 0.5)
            assert "preferences" in visible()
            key(b"e")
            key(b"\x7f" * 3 + b"#general\t")
            key(b"\x7f" * 2 + b"yes")
            key(b"\r", 0.8)
            prefs = rpc("follow", {"scope": {"channel": "general"}})
            assert prefs["effective"]["inbox"], prefs
            key(b"B", 0.5)
            assert rpc("follow")["bell"]
            key(b"D", 0.5)
            assert rpc("follow")["dnd"]
            shot("cm-chat-B-preferences")
            disabled = key(b"\x1bM")
            assert b"\x1b[?1000l" in disabled
            enabled = key(b"\x1b[109;4u")
            assert b"\x1b[?1000h" in enabled
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 16, 48, 0, 0))
            screen.resize(16, 48)
            os.kill(tui.pid, signal.SIGWINCH)
            drain(0.6)
            shot("cm-chat-B-narrow")
            assert tui.poll() is None
            key(b"\x1bm")
            key(b"\x1bq")
            tui.wait(timeout=5)
            assert tui.returncode == 0
            print(
                "PASS: channel browser/preview/join/leave, direct and @here completion/recipients, edited mentions no longer notify; channel creation/settings/admins, stable-name edits, pin/unpin and pins view; timeline full messages/order/unread, searchable group creation/reply/collapse, real TUI norms acknowledge/publish/revert/conflict/rebase, persistent draft archive, monitor create/results/ack/cancel/dismiss, follow/bell/DND, 80x24 and 48x16, chat/mouse shortcuts.",
                flush=True,
            )
        except Exception:
            shot("cm-chat-B-failure")
            pathlib.Path("/tmp/cm-chat-B-failure.txt").write_text(visible())
            raise
        finally:
            if tui and tui.poll() is None:
                tui.terminate()
                tui.wait(timeout=5)
            if daemon and daemon.poll() is None:
                daemon.terminate()
                daemon.wait(timeout=5)
            os.close(master)
