"""Shell panes are controllable terminals, not agent workers."""


def is_shell(session: dict) -> bool:
    return session.get("kind") == "shell" or any(
        session.get(key) == "bash" for key in ("type", "session_type", "engine")
    )


def shell_refusal(feature: str) -> dict:
    return {
        "error": "agent_only",
        "kind": "shell",
        "message": f"{feature} requires an agent session; bash is a shell with no "
                   "agent turns, transcript or completion reports. Use send_input "
                   "for shell commands and redirect output to a file.",
    }
