"""No real Codex, user settings, or live sockets: temp cache + fake frontend."""
import asyncio
from concurrent.futures import ThreadPoolExecutor
import fcntl
import json
import os
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from mcp_server import codex_startup as startup, native_codex


@pytest.fixture
def home(tmp_path, monkeypatch):
    monkeypatch.setenv("CODEX_HOME", str(tmp_path))
    return tmp_path


def cache(home, **extra):
    value = {"latest_version":"0.999.0", "last_checked_at":"2026-10-31T09:00:00Z",
             "dismissed_version":None, **extra}
    (home / "version.json").write_text(json.dumps(value))
    return value


def cached_menu_visible(home, *, check_for_update_on_startup):
    # Regression model of the incident: the switch controls fetching, not the
    # already-cached notice. No network or native process is involved.
    value = json.loads((home / "version.json").read_text())
    return value["latest_version"] != value.get("dismissed_version")


def test_startup90_cached_menu_survives_false_then_is_narrowly_dismissed(home):
    old = cache(home, future={"nested":[1,"secret preserved but never logged"]})
    assert cached_menu_visible(home, check_for_update_on_startup=False)
    assert startup.dismiss_cached_update() == "dismissed"
    assert not cached_menu_visible(home, check_for_update_on_startup=False)
    assert json.loads((home/"version.json").read_text()) == {**old,"dismissed_version":"0.999.0"}
    before = (home/"version.json").stat()
    assert startup.dismiss_cached_update() == "already_dismissed"
    assert (home/"version.json").stat().st_mtime_ns == before.st_mtime_ns


@pytest.mark.parametrize("raw", [b"", b"  ", b"{}", b'{"last_checked_at":"yesterday"}',
                                  b'{"latest_version":null}', b'{"latest_version":""}'])
def test_startup90_empty_or_no_notice_cache_starts_without_rewrite(home,raw):
    path=home/"version.json";path.write_bytes(raw)
    assert startup.dismiss_cached_update() in {"empty","no_notice"}
    assert path.read_bytes()==raw


def test_startup90_missing_empty_no_notice_and_dismissed_need_no_write_permission(home):
    path=home/"version.json"
    try:
        for content in [None,"",'{}','{"latest_version":"x","dismissed_version":"x"}']:
            if content is not None:path.write_text(content)
            home.chmod(0o500)
            assert startup.dismiss_cached_update() in {"missing","empty","no_notice","already_dismissed"}
            assert not (home/".cm-version.lock").exists()
            home.chmod(0o700)
            if path.exists():path.unlink()
    finally:
        home.chmod(0o700)


def test_startup90_default_custom_empty_and_missing_home(tmp_path, monkeypatch):
    monkeypatch.delenv("CODEX_HOME", raising=False)
    with patch.object(startup.Path,"home",return_value=tmp_path):
        assert startup.dismiss_cached_update() == "missing"
        default=tmp_path/".codex";default.mkdir();cache(default)
        monkeypatch.setenv("CODEX_HOME","")
        assert startup.dismiss_cached_update() == "dismissed"
        custom=tmp_path/"custom";custom.mkdir();cache(custom)
        monkeypatch.setenv("CODEX_HOME",str(custom))
        assert startup.dismiss_cached_update() == "dismissed"
        assert not (tmp_path/"config.toml").exists()
    assert not list(tmp_path.rglob(".cm-version-*"))


@pytest.mark.parametrize("raw",[b"{",b"[]",b'{"latest_version":42}',
    b'{"latest_version":"x","latest_version":"y"}', b'{"latest_version":"x","other":NaN}',
    b"x"*(startup.MAX_CACHE_BYTES+1)])
def test_startup90_bad_cache_refuses_without_replacement_or_content_leak(home,raw):
    path=home/"version.json";path.write_bytes(raw)
    with pytest.raises(startup.StartupSuppressionError) as exc:
        startup.dismiss_cached_update()
    assert path.read_bytes()==raw
    assert "version.json" in str(exc.value) and "dismissed_version" in str(exc.value)
    assert not list(home.glob(".cm-version-*"))


def test_startup90_missing_regular_and_suspicious_entries(home):
    assert startup.dismiss_cached_update()=="missing"
    path=home/"version.json"
    path.symlink_to(home/"absent")
    with pytest.raises(startup.StartupSuppressionError):startup.dismiss_cached_update()
    path.unlink();os.mkfifo(path)
    with pytest.raises(startup.StartupSuppressionError):startup.dismiss_cached_update()
    path.unlink();path.mkdir()
    with pytest.raises(startup.StartupSuppressionError):startup.dismiss_cached_update()


def test_startup90_dangling_configured_home_or_ancestor_fails(home,monkeypatch):
    link=home/"dangling";link.symlink_to(home/"absent")
    for path in [link,link/"nested"/"codex"]:
        monkeypatch.setenv("CODEX_HOME",str(path))
        with pytest.raises(startup.StartupSuppressionError):startup.dismiss_cached_update()


def test_startup90_readonly_io_failure_and_busy_lock_refuse(home):
    cache(home);path=home/"version.json";before=path.read_bytes()
    path.chmod(0o400)
    with pytest.raises(startup.StartupSuppressionError):startup.dismiss_cached_update()
    assert path.read_bytes()==before
    path.chmod(0o600)
    with patch.object(startup.os,"replace",side_effect=PermissionError("PRIVATE")):
        with pytest.raises(startup.StartupSuppressionError,match="PermissionError") as exc:startup.dismiss_cached_update()
    assert "PRIVATE" not in str(exc.value) and path.read_bytes()==before
    assert not list(home.glob(".cm-version-*"))
    with (home/".cm-version.lock").open("a") as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        with patch.object(startup,"LOCK_WAIT",0.01):
            with pytest.raises(startup.StartupSuppressionError,match="busy"):startup.dismiss_cached_update()


def test_startup90_concurrent_cm_launches_preserve_other_keys(home):
    old=cache(home,unknown={"keep":True})
    with ThreadPoolExecutor(max_workers=4) as pool:
        results=list(pool.map(lambda _:startup.dismiss_cached_update(),range(8)))
    assert results.count("dismissed")==1
    assert results.count("already_dismissed")==7
    assert json.loads((home/"version.json").read_text())=={**old,"dismissed_version":"0.999.0"}


@pytest.mark.parametrize("stage", [1, 2, 3])
def test_startup90_detected_upstream_rewrite_is_not_overwritten(home,stage):
    cache(home)
    real_stat=startup.os.stat;calls=0
    def race(path,*args,**kwargs):
        nonlocal calls
        if path=="version.json":
            calls+=1
            if calls==stage:cache(home,latest_version="new",new_unknown="keep")
        return real_stat(path,*args,**kwargs)
    with patch.object(startup.os,"stat",side_effect=race):
        if stage==1:
            # A preflight race is re-read under the shared CM lock, preserving
            # the new fields; races during the protected attempt must refuse.
            assert startup.dismiss_cached_update()=="dismissed"
        else:
            with pytest.raises(startup.StartupSuppressionError,match="changed"):
                startup.dismiss_cached_update()
    value=json.loads((home/"version.json").read_text())
    assert value["new_unknown"]=="keep" and value["latest_version"]=="new"
    assert value["dismissed_version"] == ("new" if stage==1 else None)


@pytest.mark.parametrize("mode",[[],["resume","saved"],["fork","saved"]])
def test_startup90_every_frontend_including_relaunch_is_gated(home,mode):
    cache(home)
    incoming=([mode[0]] if mode else [])+["-c","check_for_update_on_startup=true"]+([mode[1]] if mode else [])
    backend,frontend=native_codex.split_args(incoming)
    assert backend[-2:]==["-c",startup.UPDATE_OFF]
    assert startup.UPDATE_OFF in frontend
    if mode: assert frontend[-2:]==mode
    async def fake_exec(*argv,**kwargs):
        assert not cached_menu_visible(home,check_for_update_on_startup=False)
        assert "--remote" in argv and startup.UPDATE_OFF in argv
        assert not kwargs # Inherits PTY as before; no menu input plumbing.
        return "child"
    with patch.object(native_codex.asyncio,"create_subprocess_exec",side_effect=fake_exec) as spawn:
        assert asyncio.run(native_codex.spawn_frontend("fake",frontend,"/tmp/fake.sock"))=="child"
        cache(home,latest_version="0.1000.0")
        assert asyncio.run(native_codex.spawn_frontend("fake",frontend,"/tmp/fake.sock"))=="child"
        assert spawn.call_count==2
        (home/"version.json").write_text("bad")
        with pytest.raises(startup.StartupSuppressionError):
            asyncio.run(native_codex.spawn_frontend("fake",frontend,"/tmp/fake.sock"))
        assert spawn.call_count==2,"failed suppression must never spawn a frontend"
