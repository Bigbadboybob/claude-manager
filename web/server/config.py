"""Explicit production configuration; no anonymous/dev-auth fallback."""
from dataclasses import dataclass
import os
from pathlib import Path
import re
from urllib.parse import urlsplit


@dataclass(frozen=True)
class Settings:
    team_domain: str
    audience: str
    owner_email: str
    origin: str = "https://board.latentoracle.xyz"
    socket_path: Path = Path.home() / ".cm/daemon.sock"
    token_path: Path = Path.home() / ".cm/board-token"
    push_path: Path = Path.home() / ".cm/web-push"
    static_path: Path = Path(__file__).resolve().parents[1] / "static"

    def __post_init__(self):
        if not re.fullmatch(r"[a-zA-Z0-9-]+\.cloudflareaccess\.com", self.team_domain):
            raise ValueError("CF_ACCESS_TEAM_DOMAIN must be the team's cloudflareaccess.com hostname")
        if not self.audience or len(self.audience) > 256:
            raise ValueError("CF_ACCESS_AUD is required")
        if not re.fullmatch(r"[^\s@]+@[^\s@]+\.[^\s@]+", self.owner_email):
            raise ValueError("CF_ACCESS_EMAIL must identify Owner")
        parsed = urlsplit(self.origin)
        if (parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password
                or parsed.path or parsed.query or parsed.fragment or parsed.port == 0):
            raise ValueError("BOARD_ORIGIN must be an HTTPS origin without a path")
        if not self.socket_path.is_absolute() or not self.token_path.is_absolute():
            raise ValueError("Daemon socket/token paths must be absolute local paths")

    @classmethod
    def from_env(cls):
        paths = {field: Path(os.environ[key]).expanduser() for field, key in {
            "socket_path": "BOARD_DAEMON_SOCKET", "token_path": "BOARD_TOKEN_FILE",
            "push_path": "BOARD_PUSH_PATH", "static_path": "BOARD_STATIC_PATH",
        }.items() if key in os.environ}
        return cls(team_domain=os.environ.get("CF_ACCESS_TEAM_DOMAIN", ""),
                   audience=os.environ.get("CF_ACCESS_AUD", ""),
                   owner_email=os.environ.get("CF_ACCESS_EMAIL", "").lower(),
                   origin=os.environ.get("BOARD_ORIGIN", "https://board.latentoracle.xyz"), **paths)
