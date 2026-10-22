"""Gate receipts: validation, parsing and the verdict, free of any I/O
(doc/gate-receipts.md).

`dispatch.gate_db` loads a board's profile, the item's binding and its
grants under the board-row lock and calls these functions; nothing here
touches the database, so every verdict rule is unit-tested directly.

A verdict is computed, never taken from the poster: PASS needs every
required step present, run and passing (or covered by a verified grant),
every required input bound by content, and the run inside its grant.
"""

from __future__ import annotations

import hashlib
import json
import os
import re

from dispatch.items_rules import ItemsError

VERDICTS = ("PASS", "FAIL", "VOID", "INCOMPLETE", "STALE")
# Precedence of the levels a reason carries (STALE is applied by the caller).
LEVELS = ("fail", "void", "incomplete")
STEP_STATES = ("ran", "skipped", "not_run", "void")
STEP_KINDS = ("pytest", "mypy", "ruff", "rc")
NODE_OUTCOMES = ("passed", "failed", "error", "skipped", "xfailed", "xpassed", "deselected")
RAN_OUTCOMES = frozenset({"passed", "failed", "error", "xfailed", "xpassed"})
INPUT_KINDS = ("file", "dir", "gcs", "artifact", "db")
INPUT_STATES = ("bound", "missing", "unbound")
ATTESTATIONS = ("cm-gate", "self")
GRANT_KINDS = ("waiver", "variance")
SCOPES = ("paths", "tree")
# Content-id prefixes each input kind accepts.
CONTENT_PREFIXES = {
    "file": ("sha256:",),
    "dir": ("tree:",),
    "gcs": ("gcs:",),
    "artifact": ("sha256:", "tree:", "gcs:"),
    "db": ("snapshot:", "rows:"),
}
# v1 hashes these kinds itself when cm-gate runs (EP, 2026-10-22: every
# receipt says which inputs are hashed at run time and which are not).
CM_HASHED_KINDS = frozenset({"file", "dir", "gcs", "artifact"})
DEFAULT_MF_EXC = "AssertionError"
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
MAX_STEPS = 40
MAX_INPUTS = 100
MAX_WAIVERS = 20
MAX_NODES = 20000
MAX_CLOSURE_PATHS = 20000
MAX_SUMMARY = 2000
MAX_RECEIPT_BYTES = 4 * 1024 * 1024
GATE_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9_.-]{0,39}$")

# Flag kinds the gate layer raises (board engine never clears them).
GATE_FLAG_KINDS = frozenset({"receipt_stale", "runner_void", "input_void"})
# Kinds that only ever reach the item's holders, never the orchestrator.
HOLDER_ONLY_FLAGS = frozenset({"receipt_stale"})
VOID_REPEAT = 2
VOID_WINDOW_S = 24 * 3600
UNCHECKED_AFTER_S = 900


class GateError(ItemsError):
    """A refused gate write; the API maps it like every ItemsError."""


def bad(message: str, field: str | None = None) -> GateError:
    extra = {"field": field} if field else {}
    return GateError(422, "invalid_field", message, **extra)


# ---- small helpers ----------------------------------------------------------

def _str(value, name: str, limit: int, *, required: bool = False) -> str | None:
    if value is None or value == "":
        if required:
            raise bad(f"{name} is required", name)
        return None
    if not isinstance(value, str):
        raise bad(f"{name} must be a string", name)
    if len(value) > limit:
        raise bad(f"{name} is longer than {limit} characters", name)
    return value


def _sha(value, name: str, *, required: bool = False) -> str | None:
    text = _str(value, name, 40, required=required)
    if text is None:
        return None
    text = text.lower()
    if not SHA_RE.match(text):
        raise bad(f"{name} must be a full 40-hex commit SHA", name)
    return text


def _bool(value, name: str) -> bool:
    if not isinstance(value, bool):
        raise bad(f"{name} must be a boolean", name)
    return value


def _str_list(value, name: str, limit: int, item_limit: int = 500) -> list[str]:
    if value is None:
        return []
    if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
        raise bad(f"{name} must be a list of strings", name)
    if len(value) > limit or any(len(v) > item_limit for v in value):
        raise bad(f"{name}: at most {limit} entries of at most {item_limit} characters", name)
    return list(value)


def _int(value, name: str, *, minimum: int = 0) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise bad(f"{name} must be an integer ≥ {minimum}", name)
    return value


def cmd_digest(effective_cmd: dict | None) -> str:
    """The digest a variance grant binds: argv and env, not the host's cwd."""
    cmd = effective_cmd or {}
    canon = json.dumps({"argv": list(cmd.get("argv") or []), "env": dict(cmd.get("env") or {})},
                       sort_keys=True, separators=(",", ":"))
    return "sha256:" + hashlib.sha256(canon.encode()).hexdigest()


def closure_digest(paths: list[str]) -> str:
    return "sha256:" + hashlib.sha256("\n".join(sorted(set(paths))).encode()).hexdigest()


def receipt_label(slug: str, n: int, seq: int) -> str:
    return f"r-{slug}-{n}-{seq}"


def grant_label(slug: str, n: int, seq: int) -> str:
    return f"g-{slug}-{n}-{seq}"


def short(sha: str | None) -> str:
    return (sha or "?")[:7]


# ---- profiles --------------------------------------------------------------

_PROFILE_KEYS = {"steps", "waivable", "require", "base", "inputs", "allowed_env", "closure_cmd",
                 "runner_ref", "version", "class"}
_STEP_KEYS = {"name", "kind", "required", "argv", "at", "nodes", "expect_fail", "min_passed",
              "max_failed", "xfail_counts_as_pass", "must_run", "skip_policy", "max_errors",
              "max_violations", "void_on_fail", "timeout_s", "cwd"}


def validate_profile(raw) -> dict:
    """A normalized profile, or GateError. `version` is assigned by the caller."""
    if not isinstance(raw, dict):
        raise bad("profile must be an object", "profile")
    unknown = set(raw) - _PROFILE_KEYS
    if unknown:
        raise bad(f"unknown profile keys: {sorted(unknown)}", "profile")
    steps_raw = raw.get("steps")
    if not isinstance(steps_raw, list) or not steps_raw or len(steps_raw) > MAX_STEPS:
        raise bad(f"profile.steps must be a list of 1-{MAX_STEPS} steps", "steps")
    steps, names = [], set()
    for s in steps_raw:
        if not isinstance(s, dict):
            raise bad("each step must be an object", "steps")
        extra = set(s) - _STEP_KEYS
        if extra:
            raise bad(f"unknown step keys: {sorted(extra)}", "steps")
        name = _str(s.get("name"), "steps[].name", 40, required=True)
        if not GATE_NAME_RE.match(name) or name in names:
            raise bad(f"step name {name!r} must be unique, lowercase, [a-z0-9_.-]", "steps")
        names.add(name)
        kind = s.get("kind", "rc")
        if kind not in STEP_KINDS:
            raise bad(f"step {name}: kind must be one of {', '.join(STEP_KINDS)}", "steps")
        step = {"name": name, "kind": kind, "required": _bool(s.get("required", True), "required")}
        if "argv" in s:
            step["argv"] = _str_list(s["argv"], f"{name}.argv", 200, 2000)
        at = s.get("at", "head")
        if at not in ("head", "mf"):
            raise bad(f"step {name}: at must be head or mf", "steps")
        step["at"] = at
        if "nodes" in s:
            step["nodes"] = _str_list(s["nodes"], f"{name}.nodes", MAX_NODES, 1000)
        if "expect_fail" in s:
            ef = s["expect_fail"]
            if kind != "pytest" or not isinstance(ef, dict):
                raise bad(f"step {name}: expect_fail is a pytest object", "steps")
            norm = {}
            for key, val in ef.items():
                if key == "default":
                    norm["default"] = _str(val, "expect_fail.default", 200, required=True)
                else:
                    vals = [val] if isinstance(val, str) else val
                    norm[key] = _str_list(vals, f"expect_fail[{key}]", 20, 200)
            norm.setdefault("default", DEFAULT_MF_EXC)
            step["expect_fail"] = norm
        elif at == "mf":
            step["expect_fail"] = {"default": DEFAULT_MF_EXC}
        for key in ("min_passed", "max_failed", "max_errors", "max_violations", "timeout_s"):
            if key in s:
                step[key] = _int(s[key], f"{name}.{key}")
        for key in ("xfail_counts_as_pass", "void_on_fail"):
            if key in s:
                step[key] = _bool(s[key], f"{name}.{key}")
        if "must_run" in s:
            step["must_run"] = _str_list(s["must_run"], f"{name}.must_run", 500, 1000)
        if "skip_policy" in s:
            sp = s["skip_policy"]
            if not isinstance(sp, dict) or set(sp) - {"allow_reasons", "otherwise"}:
                raise bad(f"step {name}: skip_policy takes allow_reasons and otherwise", "steps")
            allow = _str_list(sp.get("allow_reasons"), "allow_reasons", 50, 500)
            for pat in allow:
                try:
                    re.compile(pat)
                except re.error as exc:
                    raise bad(f"step {name}: bad allow_reasons regex {pat!r}: {exc}",
                              "steps") from None
            otherwise = sp.get("otherwise", "incomplete")
            if otherwise not in ("incomplete", "fail"):
                raise bad("skip_policy.otherwise must be incomplete or fail", "steps")
            step["skip_policy"] = {"allow_reasons": allow, "otherwise": otherwise}
        if "cwd" in s:
            step["cwd"] = _str(s["cwd"], f"{name}.cwd", 500)
        steps.append(step)
    waivable = _str_list(raw.get("waivable"), "waivable", MAX_STEPS, 40)
    if set(waivable) - names:
        raise bad(f"waivable names unknown steps: {sorted(set(waivable) - names)}", "waivable")
    req = raw.get("require") or {}
    if not isinstance(req, dict) or set(req) - {"mf_sha", "runner", "ruling_id", "attestation"}:
        raise bad("require takes mf_sha, runner, ruling_id, attestation", "require")
    require = {k: _bool(req.get(k, False), f"require.{k}") for k in ("mf_sha", "runner", "ruling_id")}
    if any(s["at"] == "mf" for s in steps):
        require["mf_sha"] = True
    att = req.get("attestation")
    if att not in (None, "cm-gate"):
        raise bad("require.attestation is null or cm-gate", "require")
    require["attestation"] = att
    base_raw = raw.get("base")
    base = None
    if base_raw is not None:
        if not isinstance(base_raw, dict) or set(base_raw) - {"ref", "scope"}:
            raise bad("base takes ref and scope", "base")
        scope = base_raw.get("scope", "paths")
        if scope not in SCOPES:
            raise bad("base.scope is paths or tree", "base")
        base = {"ref": _str(base_raw.get("ref"), "base.ref", 200, required=True), "scope": scope}
    inputs = []
    seen_inputs = set()
    for inp in raw.get("inputs") or []:
        if not isinstance(inp, dict) or set(inp) - {"name", "kind", "root_env", "path",
                                                    "locator", "required"}:
            raise bad("inputs[] take name, kind, root_env, path, locator, required", "inputs")
        name = _str(inp.get("name"), "inputs[].name", 80, required=True)
        if name in seen_inputs:
            raise bad(f"duplicate input {name}", "inputs")
        seen_inputs.add(name)
        kind = inp.get("kind")
        if kind not in INPUT_KINDS:
            raise bad(f"input {name}: kind must be one of {', '.join(INPUT_KINDS)}", "inputs")
        entry = {"name": name, "kind": kind, "required": _bool(inp.get("required", True), "required")}
        for key in ("root_env", "path", "locator"):
            if inp.get(key) is not None:
                entry[key] = _str(inp[key], f"inputs[].{key}", 1000)
        inputs.append(entry)
    if len(inputs) > MAX_INPUTS:
        raise bad(f"at most {MAX_INPUTS} inputs", "inputs")
    out = {
        "steps": steps, "waivable": waivable, "require": require, "base": base,
        "inputs": inputs,
        "allowed_env": _str_list(raw.get("allowed_env"), "allowed_env", 100, 200),
    }
    if raw.get("closure_cmd") is not None:
        out["closure_cmd"] = _str_list(raw["closure_cmd"], "closure_cmd", 100, 2000)
    if raw.get("runner_ref") is not None:
        out["runner_ref"] = _str(raw["runner_ref"], "runner_ref", 500)
    return out


def profile_step(profile: dict | None, name: str) -> dict | None:
    for s in (profile or {}).get("steps", []):
        if s["name"] == name:
            return s
    return None


# ---- receipt normalization --------------------------------------------------

def _node(value) -> dict:
    if not isinstance(value, dict):
        raise bad("nodes[] must be objects {id, outcome, exc_type?}", "steps")
    node_id = _str(value.get("id"), "nodes[].id", 1000, required=True)
    outcome = value.get("outcome")
    if outcome not in NODE_OUTCOMES:
        raise bad(f"node {node_id}: outcome must be one of {', '.join(NODE_OUTCOMES)}", "steps")
    out = {"id": node_id, "outcome": outcome}
    if value.get("exc_type") is not None:
        out["exc_type"] = _str(value["exc_type"], "nodes[].exc_type", 200)
    if value.get("phase") is not None:
        out["phase"] = _str(value["phase"], "nodes[].phase", 20)
    return out


def _reasoned(value, name: str) -> list[dict] | None:
    """`skips` / `deselected`: absent stays None (INCOMPLETE), [] is explicit."""
    if value is None:
        return None
    if not isinstance(value, list):
        raise bad(f"{name} must be a list of {{node, reason}}", name)
    out = []
    for v in value:
        if not isinstance(v, dict):
            raise bad(f"{name}[] must be objects {{node, reason}}", name)
        out.append({"node": _str(v.get("node"), f"{name}[].node", 1000, required=True),
                    "reason": _str(v.get("reason"), f"{name}[].reason", 500) or ""})
    return out


def normalize_step(raw) -> dict:
    if not isinstance(raw, dict):
        raise bad("each step must be an object", "steps")
    name = _str(raw.get("name"), "steps[].name", 40, required=True)
    state = raw.get("state")
    if state not in STEP_STATES:
        raise bad(f"step {name}: state must be one of {', '.join(STEP_STATES)}", "steps")
    step: dict = {"name": name, "state": state}
    cmd = raw.get("effective_cmd")
    if cmd is not None:
        if not isinstance(cmd, dict):
            raise bad(f"step {name}: effective_cmd is {{argv, env, cwd}}", "steps")
        env = cmd.get("env") or {}
        if not isinstance(env, dict) or not all(isinstance(k, str) and isinstance(v, str)
                                                for k, v in env.items()):
            raise bad(f"step {name}: effective_cmd.env maps strings to strings", "steps")
        step["effective_cmd"] = {
            "argv": _str_list(cmd.get("argv"), f"{name}.argv", 500, 4000),
            "env": env,
            "cwd": _str(cmd.get("cwd"), f"{name}.cwd", 1000),
        }
        if cmd.get("injected") is not None:
            step["effective_cmd"]["injected"] = _str_list(cmd["injected"], "injected", 20, 1000)
    step["at_sha"] = _sha(raw.get("at_sha"), f"{name}.at_sha")
    rc = raw.get("rc")
    if rc is not None and (not isinstance(rc, int) or isinstance(rc, bool)):
        raise bad(f"step {name}: rc must be an integer", "steps")
    step["rc"] = rc
    for key in ("started_at", "ended_at"):
        step[key] = _str(raw.get(key), key, 64)
    summary = raw.get("summary") or ""
    if not isinstance(summary, str):
        raise bad(f"step {name}: summary must be a string", "steps")
    step["summary"] = summary[-MAX_SUMMARY:]
    if raw.get("nodes") is not None:
        if not isinstance(raw["nodes"], list) or len(raw["nodes"]) > MAX_NODES:
            raise bad(f"step {name}: nodes is a list of at most {MAX_NODES}", "steps")
        step["nodes"] = [_node(v) for v in raw["nodes"]]
    else:
        step["nodes"] = None
    if raw.get("granted_nodes") is not None:
        step["granted_nodes"] = _str_list(raw["granted_nodes"], f"{name}.granted_nodes",
                                          MAX_NODES, 1000)
    step["skips"] = _reasoned(raw.get("skips"), "skips")
    step["deselected"] = _reasoned(raw.get("deselected"), "deselected")
    step["inputs_used"] = _str_list(raw.get("inputs_used"), "inputs_used", MAX_INPUTS, 80)
    step["reason"] = _str(raw.get("reason"), "reason", 300)
    return step


def normalize_input(raw, attestation: str) -> dict:
    if not isinstance(raw, dict):
        raise bad("inputs[] must be objects", "inputs")
    name = _str(raw.get("name"), "inputs[].name", 80, required=True)
    kind = raw.get("kind")
    if kind not in INPUT_KINDS:
        raise bad(f"input {name}: kind must be one of {', '.join(INPUT_KINDS)}", "inputs")
    state = raw.get("state", "bound")
    if state not in INPUT_STATES:
        raise bad(f"input {name}: state must be one of {', '.join(INPUT_STATES)}", "inputs")
    content_id = _str(raw.get("content_id"), "content_id", 300)
    if state == "bound" and not content_id:
        state = "unbound"
    produced_by = raw.get("produced_by")
    if produced_by is not None and not isinstance(produced_by, str):
        raise bad(f"input {name}: produced_by is a receipt id string", "inputs")
    size = raw.get("bytes")
    if size is not None:
        size = _int(size, "inputs[].bytes")
    out = {
        "name": name, "kind": kind, "state": state,
        "locator": _str(raw.get("locator"), "locator", 1000) or "",
        "root_env": _str(raw.get("root_env"), "root_env", 200),
        "resolved_root": _str(raw.get("resolved_root"), "resolved_root", 1000),
        "content_id": content_id,
        "bytes": size,
        "produced_by": produced_by,
    }
    # Computed here, never taken from the poster.
    # cm-gate marks the inputs it hashed itself; anything supplied on its
    # command line, and every db identity, stays self-supplied.
    out["verification"] = ("hashed_at_run"
                           if attestation == "cm-gate" and raw.get("hashed_by") == "cm-gate"
                           and kind in CM_HASHED_KINDS and state == "bound"
                           else "self_supplied_unverified")
    return out


def normalize_receipt(raw: dict) -> dict:
    """The stored shape of a posted receipt, or GateError for a bad wire shape.

    Only the wire shape is checked here: a missing step or identity is a
    verdict reason, not a refusal, so the evidence is kept."""
    if not isinstance(raw, dict):
        raise bad("receipt must be an object")
    att = raw.get("attestation", "self")
    if att not in ATTESTATIONS:
        raise bad("attestation is cm-gate or self", "attestation")
    gate = raw.get("gate") or "gate"
    if not isinstance(gate, str) or not GATE_NAME_RE.match(gate):
        raise bad("gate must be lowercase [a-z0-9_.-], at most 40 characters", "gate")
    steps_raw = raw.get("steps")
    if not isinstance(steps_raw, list) or len(steps_raw) > MAX_STEPS:
        raise bad(f"steps must be a list of at most {MAX_STEPS}", "steps")
    steps = [normalize_step(s) for s in steps_raw]
    if len({s["name"] for s in steps}) != len(steps):
        raise bad("step names must be unique", "steps")
    inputs_raw = raw.get("inputs") or []
    if not isinstance(inputs_raw, list) or len(inputs_raw) > MAX_INPUTS:
        raise bad(f"inputs must be a list of at most {MAX_INPUTS}", "inputs")
    inputs = [normalize_input(i, att) for i in inputs_raw]
    if len({i["name"] for i in inputs}) != len(inputs):
        raise bad("input names must be unique", "inputs")
    runner = raw.get("runner")
    if runner is not None:
        if not isinstance(runner, dict):
            raise bad("runner is {ref, sha}", "runner")
        runner = {"ref": _str(runner.get("ref"), "runner.ref", 500, required=True),
                  "sha": _sha(runner.get("sha"), "runner.sha", required=True)}
    base = raw.get("base") or {}
    if not isinstance(base, dict):
        raise bad("base is an object", "base")
    base = {
        "ref": _str(base.get("ref"), "base.ref", 200),
        "sha_before": _sha(base.get("sha_before"), "base.sha_before"),
        "sha_after": _sha(base.get("sha_after"), "base.sha_after"),
        "merge_tree_before": _tree(base.get("merge_tree_before"), "base.merge_tree_before"),
        "merge_tree_after": _tree(base.get("merge_tree_after"), "base.merge_tree_after"),
    }
    wt = raw.get("worktree") or {}
    if not isinstance(wt, dict):
        raise bad("worktree is an object", "worktree")
    worktree = {
        "head_before": _sha(wt.get("head_before"), "worktree.head_before"),
        "head_after": _sha(wt.get("head_after"), "worktree.head_after"),
        "dirty_before": wt.get("dirty_before") if isinstance(wt.get("dirty_before"), bool) else None,
        "dirty_after": wt.get("dirty_after") if isinstance(wt.get("dirty_after"), bool) else None,
        "path": _str(wt.get("path"), "worktree.path", 1000),
    }
    closure = raw.get("closure")
    if closure is not None:
        if not isinstance(closure, dict):
            raise bad("closure is {paths, cmd}", "closure")
        paths = _str_list(closure.get("paths"), "closure.paths", MAX_CLOSURE_PATHS, 1000)
        closure = {"paths": sorted(set(paths)), "count": len(set(paths)),
                   "digest": closure_digest(paths),
                   "cmd": _str_list(closure.get("cmd"), "closure.cmd", 100, 2000)}
    waivers = _str_list(raw.get("waivers"), "waivers", MAX_WAIVERS, 120)
    out = {
        "gate": gate,
        "class": _str(raw.get("class") or raw.get("klass"), "class", 40),
        "repo": _str(raw.get("repo"), "repo", 500),
        "branch": _str(raw.get("branch"), "branch", 250),
        "head_sha": _sha(raw.get("head_sha") or raw.get("head"), "head", required=True),
        "mf_sha": _sha(raw.get("mf_sha") or raw.get("mf"), "mf"),
        "runner": runner,
        "ruling_id": _str(raw.get("ruling_id") or raw.get("ruling"), "ruling", 80),
        "base": base,
        "closure": closure,
        "worktree": worktree,
        "inputs": inputs,
        "steps": steps,
        "waivers": waivers,
        "log_path": _str(raw.get("log_path"), "log_path", 1000),
        "attestation": att,
    }
    if len(json.dumps(out, default=str)) > MAX_RECEIPT_BYTES:
        raise bad(f"receipt is larger than {MAX_RECEIPT_BYTES // 1024} KiB; put logs at log_path")
    return out


def _tree(value, name: str) -> str | None:
    text = _str(value, name, 64)
    if text is None or text == "conflict":
        return text
    if not re.match(r"^[0-9a-f]{40}([0-9a-f]{24})?$", text.lower()):
        raise bad(f"{name} must be a tree OID or \"conflict\"", name)
    return text.lower()


# ---- tool summary parsers ---------------------------------------------------

_PYTEST_PART = re.compile(r"(\d+) (passed|failed|errors?|skipped|xfailed|xpassed|deselected|"
                          r"warnings?|rerun)")


def parse_pytest(summary: str) -> dict | None:
    """Counts from pytest's final summary line, or None if there is none."""
    for line in reversed((summary or "").splitlines()):
        text = line.strip().strip("=").strip()
        if not text:
            continue
        if re.search(r"\bno tests ran\b", text):
            return {"passed": 0, "failed": 0, "errors": 0, "skipped": 0, "xfailed": 0,
                    "xpassed": 0, "deselected": 0}
        if not re.search(r"\bin [\d.]+s\b", text):
            continue
        parts = _PYTEST_PART.findall(text)
        if not parts:
            continue
        out = {"passed": 0, "failed": 0, "errors": 0, "skipped": 0, "xfailed": 0,
               "xpassed": 0, "deselected": 0}
        for count, word in parts:
            key = "errors" if word.startswith("error") else word
            if key in out:
                out[key] = int(count)
        return out
    return None


def parse_mypy(summary: str) -> dict | None:
    for line in reversed((summary or "").splitlines()):
        m = re.search(r"Success: no issues found in (\d+) source files?", line)
        if m:
            return {"ok": True, "errors": 0, "files": int(m.group(1))}
        m = re.search(r"Found (\d+) errors? in (\d+) files?", line)
        if m:
            return {"ok": False, "errors": int(m.group(1)), "files": int(m.group(2))}
    return None


def parse_ruff(summary: str) -> dict | None:
    for line in reversed((summary or "").splitlines()):
        if "All checks passed!" in line:
            return {"violations": 0}
        m = re.search(r"Found (\d+) errors?", line)
        if m:
            return {"violations": int(m.group(1))}
    return None


def node_counts(nodes: list[dict]) -> dict:
    out = {"passed": 0, "failed": 0, "errors": 0, "skipped": 0, "xfailed": 0, "xpassed": 0,
           "deselected": 0}
    for n in nodes:
        key = "errors" if n["outcome"] == "error" else n["outcome"]
        out[key] += 1
    return out


# ---- grant vs executed (§5) ---------------------------------------------------

# pytest options that take a value (as `-n 2` or `--numprocesses=2`).
_PYTEST_VALUE_OPTS = {
    "-n", "--numprocesses", "-k", "-m", "-p", "--timeout", "--deselect", "--ignore",
    "--ignore-glob", "-c", "--rootdir", "--junitxml", "--junit-xml", "-o", "--override-ini",
    "--maxfail", "--tb", "-W", "--basetemp", "--durations", "-r", "--dist", "--color",
    "--import-mode", "--confcutdir", "--log-level", "--timeout-method",
}
# Output-only options: never change what runs or what passes.
_OUTPUT_OPTS = {"-q", "-v", "-vv", "-vvv", "-s", "--tb", "-r", "--durations", "--junitxml",
                "--junit-xml", "--color", "--no-header", "--quiet", "--verbose", "-ra", "-rA",
                "-rs", "-rfE", "--basetemp", "-l", "--showlocals", "--log-level"}
_FILTER_OPTS = {"-k", "-m", "--deselect", "--ignore", "--ignore-glob", "--lf", "--last-failed",
                "--ff", "-x", "--exitfirst", "--maxfail", "--sw", "--stepwise"}
_WRAPPERS = {"timeout", "taskset", "numactl", "systemd-run", "nice", "ionice", "env",
             "chrt", "prlimit"}


def _split_opts(tokens: list[str], value_opts: set[str]) -> tuple[list[tuple[str, str | None]], list[str]]:
    opts: list[tuple[str, str | None]] = []
    positional: list[str] = []
    i = 0
    while i < len(tokens):
        t = tokens[i]
        if t.startswith("--") and "=" in t:
            k, v = t.split("=", 1)
            opts.append((k, v))
        elif t in value_opts and i + 1 < len(tokens):
            opts.append((t, tokens[i + 1]))
            i += 1
        elif t.startswith("-") and len(t) > 2 and t[:2] in value_opts and not t.startswith("--"):
            opts.append((t[:2], t[2:]))  # -n2, -kfoo
        elif t.startswith("-"):
            opts.append((t, None))
        else:
            positional.append(t)
        i += 1
    return opts, positional


def _strip_wrappers(argv: list[str], program: str, allowed_env: set[str]) -> tuple[list[str], list[str]]:
    """(problems, argv from the program on). Known resource wrappers before the
    program are allowed; `env K=V` only for allowed keys."""
    prog = os.path.basename(program)
    idx = next((i for i, t in enumerate(argv) if os.path.basename(t) == prog), None)
    if idx is None:
        return [f"runs {argv[0] if argv else '(nothing)'} instead of {program}"], []
    problems = []
    prefix = argv[:idx]
    i = 0
    while i < len(prefix):
        t = prefix[i]
        base = os.path.basename(t)
        if base not in _WRAPPERS:
            problems.append(f"unrecognized command prefix {t!r}")
            break
        i += 1
        # Consume the wrapper's own arguments: options, KEY=VALUE pairs,
        # numbers/masks/durations, until the next wrapper.
        while i < len(prefix) and os.path.basename(prefix[i]) not in _WRAPPERS:
            arg = prefix[i]
            if base == "env" and "=" in arg and not arg.startswith("-"):
                key = arg.split("=", 1)[0]
                if key not in allowed_env:
                    problems.append(f"env {key} is not an allowed variable")
            i += 1
    return problems, argv[idx:]


def _pytest_diff(granted: list[str], effective: list[str]) -> list[str]:
    problems = []
    g_opts, g_pos = _split_opts(granted[1:], _PYTEST_VALUE_OPTS)
    e_opts, e_pos = _split_opts(effective[1:], _PYTEST_VALUE_OPTS)
    g_set = {(k, v) for k, v in g_opts}
    for k, v in e_opts:
        if (k, v) in g_set:
            continue
        if k in ("-n", "--numprocesses"):
            try:
                if int(v or "") <= 2:
                    continue
            except ValueError:
                pass
            problems.append(f"workers {v} (at most 2 without review)")
        elif k in ("--timeout", "--timeout-method") or k in _OUTPUT_OPTS:
            continue
        elif k == "-p" and (v or "").startswith("no:cacheprovider"):
            continue
        elif k in _FILTER_OPTS:
            problems.append(f"filter {k}{' ' + v if v else ''} narrows the granted selection")
        else:
            problems.append(f"option {k}{'=' + v if v else ''} is not in the grant")
    e_keys = {(k, v) for k, v in e_opts}
    for k, v in g_opts:
        if (k, v) in e_keys or k in _OUTPUT_OPTS or k in ("-n", "--numprocesses", "--timeout"):
            continue
        if k in _FILTER_OPTS and any(ek == k for ek, _ in e_opts):
            problems.append(f"filter {k} changed from {v!r}")
        else:
            problems.append(f"granted option {k}{'=' + v if v else ''} was dropped")
    for p in g_pos:
        if p not in e_pos:
            problems.append(f"granted path {p} was removed")
    for p in e_pos:
        if p in g_pos:
            continue
        # Additive nodes/files are allowed within the grant's paths.
        if not any(p == g or p.startswith(g.rstrip("/") + "/") or p.startswith(g + "::")
                   for g in g_pos):
            problems.append(f"added path {p} is outside the granted paths")
    return problems


def command_diff(step_profile: dict, step: dict, allowed_env: set[str]) -> list[str]:
    """Why the executed command needs grantor review ([] = within the grant)."""
    granted = step_profile.get("argv")
    if not granted:
        return []
    cmd = step.get("effective_cmd")
    if not cmd or not cmd.get("argv"):
        return ["no effective command recorded"]
    problems, effective = _strip_wrappers(cmd["argv"], granted[0], allowed_env)
    if effective:
        if step_profile["kind"] == "pytest":
            problems += _pytest_diff(granted, effective)
        else:
            g = [t for t in granted[1:]]
            e = [t for t in effective[1:] if t not in _OUTPUT_OPTS]
            g = [t for t in g if t not in _OUTPUT_OPTS]
            if e != g:
                problems.append(f"arguments {' '.join(e)!r} differ from the grant {' '.join(g)!r}")
    for key in sorted((cmd.get("env") or {})):
        if key not in allowed_env:
            problems.append(f"env {key} is not an allowed variable")
    return problems


def node_diff(step_profile: dict, step: dict, covered: set[str] = frozenset()) -> tuple[list[str], list[str]]:
    """(shrinkage problems, incomplete reasons) of the executed node set.
    Nodes a verified waiver covers are not counted as missing."""
    nodes = step.get("nodes")
    if nodes is None:
        return [], []
    executed = {n["id"] for n in nodes if n["outcome"] != "deselected"}
    granted = step.get("granted_nodes")
    if granted is None and step_profile.get("nodes"):
        granted = step_profile["nodes"]
    if granted is None:
        return [], ["granted node set was not collected, so the selection cannot be checked"]
    missing = [g for g in granted if g not in executed and g not in covered]
    if not missing:
        return [], []
    sample = ", ".join(missing[:3]) + (f" (+{len(missing) - 3})" if len(missing) > 3 else "")
    return [f"{len(missing)} granted node(s) not run: {sample}"], []


# ---- grants (§4) -------------------------------------------------------------

def grant_problems(grant: dict, profile: dict | None, grantors: set[str]) -> list[str]:
    """Why a grant is not verified ([] = verified)."""
    problems = []
    if grant.get("revoked_at"):
        problems.append("revoked")
    if grant["grantor_pid"] not in grantors:
        problems.append(f"{grant['grantor_pid']} is not a waiver grantor on this board")
    check = grant.get("message_check") or {}
    if not check.get("found"):
        problems.append("message not found")
    else:
        if check.get("sender") != grant["grantor_pid"]:
            problems.append("message was not sent by the grantor")
        if not check.get("contains_head"):
            problems.append("message does not name the head SHA")
        if grant.get("base_sha") and not check.get("contains_base"):
            problems.append("message does not name the base SHA")
    if not (grant.get("reason") or "").strip() or not (grant.get("risk") or "").strip():
        problems.append("reason and risk are required")
    step = profile_step(profile, grant["step"])
    if step is None:
        problems.append(f"step {grant['step']} is not in the profile")
    elif grant["kind"] == "waiver" and grant["step"] not in (profile or {}).get("waivable", []):
        problems.append(f"step {grant['step']} is not waivable")
    if grant["kind"] == "variance" and not grant.get("cmd_digest"):
        problems.append("a variance grant binds a command digest")
    if step is not None and grant.get("nodes") and step.get("nodes"):
        outside = [n for n in grant["nodes"] if n not in step["nodes"]]
        if outside:
            problems.append(f"nodes outside the step's grant: {', '.join(outside[:3])}")
    return problems


def _covering(grants: list[dict], receipt: dict, step_name: str, kind: str) -> tuple[list[dict], list[str]]:
    """(verified grants covering this step on this run, notes on cited grants that don't)."""
    good, notes = [], []
    cited = set(receipt.get("waivers") or [])
    for g in grants:
        if g["step"] != step_name or g["kind"] != kind or g["gate"] != receipt["gate"]:
            continue
        is_cited = g["label"] in cited
        if kind == "waiver" and not is_cited:
            continue  # the runner cites waivers; variance grants apply on their own
        why = list(g.get("problems") or [])
        if g["head_sha"] != receipt["head_sha"]:
            why.append(f"head {short(receipt['head_sha'])} ≠ grant {short(g['head_sha'])}")
        if g.get("base_sha") and g["base_sha"] != (receipt.get("base") or {}).get("sha_before"):
            why.append("base differs from the grant's")
        if why:
            if is_cited or kind == "variance":
                notes.append(f"{kind} {g['label']}: unverified ({'; '.join(why)})")
            continue
        good.append(g)
    return good, notes


# ---- the verdict (§3) ----------------------------------------------------------

def _r(level: str, text: str, step: str | None = None) -> dict:
    out = {"level": level, "text": text}
    if step:
        out["step"] = step
    return out


def _eval_pytest(sp: dict, st: dict, receipt: dict, covered_nodes: set[str]) -> list[dict]:
    name = sp["name"]
    out: list[dict] = []
    nodes = st.get("nodes")
    if nodes is None or st.get("skips") is None or st.get("deselected") is None:
        missing = [k for k in ("nodes", "skips", "deselected") if st.get(k) is None]
        out.append(_r("incomplete", f"{name}: no {'/'.join(missing)} list "
                      "(an empty list is explicit; absent is not)", name))
        return out
    counts = node_counts(nodes)
    parsed = parse_pytest(st.get("summary", ""))
    st["parsed"] = parsed or counts
    rc = st.get("rc")
    is_mf = sp.get("at") == "mf"
    # Consistency: the summary, the node list and the exit code agree.
    if parsed is None:
        out.append(_r("incomplete", f"{name}: summary has no pytest result line (unparsed)", name))
    else:
        for key in ("passed", "failed", "errors", "skipped", "xfailed", "xpassed"):
            if parsed[key] != counts[key]:
                out.append(_r("fail", f"{name}: inconsistent: summary says {parsed[key]} {key}, "
                              f"nodes say {counts[key]}", name))
                break
    bad_count = counts["failed"] + counts["errors"]
    if rc is None:
        out.append(_r("incomplete", f"{name}: no exit code", name))
    elif bad_count and rc == 0:
        out.append(_r("fail", f"{name}: inconsistent: rc 0 with {bad_count} failing node(s)", name))
    elif not bad_count and rc not in (0, 5):
        out.append(_r("fail", f"{name}: inconsistent: rc {rc} with no failing node", name))
    skipped_ids = sorted(n["id"] for n in nodes if n["outcome"] == "skipped")
    if sorted(s["node"] for s in st["skips"]) != skipped_ids:
        out.append(_r("fail", f"{name}: inconsistent: skips list does not match the skipped nodes",
                      name))
    deselected_ids = sorted(n["id"] for n in nodes if n["outcome"] == "deselected")
    if sorted(d["node"] for d in st["deselected"]) != deselected_ids:
        out.append(_r("fail", f"{name}: inconsistent: deselected list does not match the nodes",
                      name))
    if is_mf and not st.get("at_sha"):
        out.append(_r("incomplete", f"{name}: no at_sha, so it is not shown to have run at the MF",
                      name))
    elif is_mf and st["at_sha"] != receipt.get("mf_sha"):
        out.append(_r("fail", f"{name}: ran at {short(st['at_sha'])}, not the MF "
                      f"{short(receipt.get('mf_sha'))}", name))
    if not is_mf and st.get("at_sha") and st["at_sha"] != receipt["head_sha"]:
        out.append(_r("fail", f"{name}: ran at {short(st['at_sha'])}, not the head", name))
    if is_mf:
        out += _eval_mf(sp, st, covered_nodes)
        return out
    passed = counts["passed"] + (counts["xfailed"] if sp.get("xfail_counts_as_pass") else 0)
    if bad_count > sp.get("max_failed", 0):
        failing = [n["id"] for n in nodes if n["outcome"] in ("failed", "error")]
        out.append(_r("fail", f"{name}: {counts['failed']} failed, {counts['errors']} errors "
                      f"({', '.join(failing[:3])})", name))
    if passed < sp.get("min_passed", 1):
        out.append(_r("incomplete", f"{name}: vacuous: {passed} passed "
                      f"(needs {sp.get('min_passed', 1)})", name))
    by_id = {n["id"]: n for n in nodes}
    for entry in sp.get("must_run", []):
        hits = [n for i, n in by_id.items()
                if (i == entry or i.startswith(entry.rstrip("/") + "/") or i.startswith(entry + "::"))
                and i not in covered_nodes]
        if not hits:
            if not any(i == entry or i.startswith(entry) for i in covered_nodes):
                out.append(_r("incomplete", f"{name}: must_run {entry} did not run (absent)", name))
            continue
        not_run = [n for n in hits if n["outcome"] not in RAN_OUTCOMES]
        if not_run:
            reasons = {s["node"]: s["reason"] for s in st["skips"] + st["deselected"]}
            why = reasons.get(not_run[0]["id"]) or not_run[0]["outcome"]
            out.append(_r("incomplete", f"{name}: must_run {entry}: {len(not_run)} node(s) "
                          f"{not_run[0]['outcome']} \"{why}\"", name))
    policy = sp.get("skip_policy") or {"allow_reasons": [], "otherwise": "incomplete"}
    for s in st["skips"]:
        if s["node"] in covered_nodes:
            continue
        if not s["reason"]:
            out.append(_r("incomplete", f"{name}: skip without a reason ({s['node']})", name))
        elif not any(re.search(p, s["reason"]) for p in policy["allow_reasons"]):
            out.append(_r(policy["otherwise"], f"{name}: skip not allowed: \"{s['reason']}\" "
                          f"({s['node']})", name))
    return out


def _eval_mf(sp: dict, st: dict, covered_nodes: set[str]) -> list[dict]:
    """Must-fail: every listed node failed, with its expected exception type."""
    name = sp["name"]
    out = []
    expect = sp.get("expect_fail") or {"default": DEFAULT_MF_EXC}
    by_id = {n["id"]: n for n in st["nodes"]}
    wanted = sp.get("nodes") or [n["id"] for n in st["nodes"] if n["outcome"] != "deselected"]
    if not wanted:
        out.append(_r("incomplete", f"{name}: no must-fail node ran", name))
    for node_id in wanted:
        if node_id in covered_nodes:
            continue
        node = by_id.get(node_id)
        allowed = expect.get(node_id) or [expect.get("default", DEFAULT_MF_EXC)]
        if node is None:
            out.append(_r("incomplete", f"{name}: MF node {node_id} did not run (absent)", name))
        elif node["outcome"] == "error":
            out.append(_r("fail", f"{name}: MF node {node_id} errored "
                          f"({node.get('exc_type') or 'collection/setup error'}); "
                          "collection and import errors never count", name))
        elif node["outcome"] in ("passed", "xpassed"):
            out.append(_r("fail", f"{name}: MF node {node_id} passed at the MF commit", name))
        elif node["outcome"] in ("skipped", "deselected"):
            out.append(_r("incomplete", f"{name}: MF node {node_id} {node['outcome']}", name))
        elif node["outcome"] == "xfailed":
            out.append(_r("fail", f"{name}: MF node {node_id} xfailed (not a real failure)", name))
        elif node.get("exc_type") not in allowed:
            out.append(_r("fail", f"{name}: MF node {node_id} failed with "
                          f"{node.get('exc_type') or 'an unknown type'}, expected "
                          f"{' or '.join(allowed)}", name))
    return out


def _eval_simple(sp: dict, st: dict) -> list[dict]:
    name, kind, rc = sp["name"], sp["kind"], st.get("rc")
    if rc is None:
        return [_r("incomplete", f"{name}: no exit code", name)]
    if kind == "rc":
        return [] if rc == 0 else [_r("fail", f"{name}: rc {rc}", name)]
    if kind == "mypy":
        parsed = parse_mypy(st.get("summary", ""))
        st["parsed"] = parsed
        if parsed is None:
            return [_r("incomplete", f"{name}: summary has no mypy result line (unparsed)", name)]
        if parsed["ok"] != (rc == 0):
            return [_r("fail", f"{name}: inconsistent: rc {rc} with "
                       f"{'Success' if parsed['ok'] else 'Found ' + str(parsed['errors'])}", name)]
        if parsed["errors"] > sp.get("max_errors", 0):
            return [_r("fail", f"{name}: Found {parsed['errors']} errors", name)]
        return []
    parsed = parse_ruff(st.get("summary", ""))
    st["parsed"] = parsed
    if parsed is None:
        return [_r("incomplete", f"{name}: summary has no ruff result line (unparsed)", name)]
    if (parsed["violations"] == 0) != (rc == 0):
        return [_r("fail", f"{name}: inconsistent: rc {rc} with {parsed['violations']} violations",
                   name)]
    if parsed["violations"] > sp.get("max_violations", 0):
        return [_r("fail", f"{name}: {parsed['violations']} violations", name)]
    return []


def _eval_inputs(receipt: dict, profile: dict) -> list[dict]:
    out = []
    have = {i["name"]: i for i in receipt["inputs"]}
    for decl in profile.get("inputs", []):
        inp = have.get(decl["name"])
        if not decl["required"]:
            continue
        if inp is None:
            out.append(_r("incomplete", f"input {decl['name']} not reported"))
            continue
        if inp["kind"] != decl["kind"]:
            out.append(_r("incomplete", f"input {decl['name']} is {inp['kind']}, "
                          f"declared {decl['kind']}"))
            continue
        if inp["state"] == "missing":
            out.append(_r("void", f"input {decl['name']} missing"
                          + (f" ({inp['locator']})" if inp["locator"] else "")))
        elif inp["state"] == "unbound" or not inp["content_id"]:
            what = ("a query/window is not an immutable identity; bind snapshot: or rows:"
                    if inp["kind"] == "db" else "no content id")
            out.append(_r("incomplete", f"input {decl['name']} unbound ({what})"))
        elif not inp["content_id"].startswith(CONTENT_PREFIXES[inp["kind"]]):
            out.append(_r("incomplete", f"input {decl['name']}: content id must start with "
                          f"{' or '.join(CONTENT_PREFIXES[inp['kind']])}"))
    return out


def evaluate(receipt: dict, profile: dict | None, grants: list[dict], *,
             klass: str | None = None) -> dict:
    """{verdict, reasons} for a normalized receipt (STALE is the caller's).

    `grants` are the item's grants with `label` and `problems` filled in
    (see `grant_problems`)."""
    reasons: list[dict] = []
    if profile is None:
        reasons.append(_r("incomplete", f"no gate profile for class {klass or '(default)'}"))
        return _combine(reasons)
    req = profile["require"]
    if req.get("attestation") == "cm-gate" and receipt["attestation"] != "cm-gate":
        reasons.append(_r("incomplete", "profile requires a cm-gate run; this receipt is self-attested"))
    if req.get("mf_sha") and not receipt.get("mf_sha"):
        reasons.append(_r("incomplete", "no MF SHA"))
    if req.get("runner") and not receipt.get("runner"):
        reasons.append(_r("incomplete", "no runner identity"))
    if req.get("ruling_id") and not receipt.get("ruling_id"):
        reasons.append(_r("incomplete", "no ruling id"))
    wt = receipt.get("worktree") or {}
    if not wt.get("head_before") or not wt.get("head_after"):
        reasons.append(_r("incomplete", "worktree identity before/after missing"))
    else:
        if wt["head_before"] != wt["head_after"]:
            reasons.append(_r("incomplete", f"code moved during the run: {short(wt['head_before'])}"
                              f" → {short(wt['head_after'])}"))
        if wt["head_before"] != receipt["head_sha"]:
            reasons.append(_r("incomplete", f"checkout was at {short(wt['head_before'])}, "
                              f"not the head {short(receipt['head_sha'])}"))
    if wt.get("dirty_before") or wt.get("dirty_after"):
        reasons.append(_r("incomplete", "worktree was dirty"))
    elif wt.get("dirty_before") is None or wt.get("dirty_after") is None:
        reasons.append(_r("incomplete", "worktree cleanliness not recorded"))
    base_cfg = profile.get("base")
    base = receipt.get("base") or {}
    if base_cfg:
        if base.get("ref") and base["ref"] != base_cfg["ref"]:
            reasons.append(_r("fail", f"weakened: base {base['ref']} instead of the granted "
                              f"{base_cfg['ref']}"))
        if not base.get("sha_after") or not base.get("merge_tree_after") \
                or not base.get("merge_tree_before"):
            reasons.append(_r("incomplete", "base / merge-tree identity missing"))
        elif base["merge_tree_before"] != base["merge_tree_after"]:
            reasons.append(_r("incomplete", "merge tree changed during the run"))
        elif base["merge_tree_after"] == "conflict":
            reasons.append(_r("incomplete", "head conflicts with the base"))
        if base_cfg["scope"] == "paths" and not (receipt.get("closure") or {}).get("paths"):
            reasons.append(_r("incomplete", "no closure file set (base scope paths needs one)"))
    allowed_env = set(profile.get("allowed_env", []))
    steps = {s["name"]: s for s in receipt["steps"]}
    for sp in profile["steps"]:
        name = sp["name"]
        st = steps.get(name)
        waivers, w_notes = _covering(grants, receipt, name, "waiver")
        whole = [g for g in waivers if not g.get("nodes")]
        covered_nodes = {n for g in waivers for n in (g.get("nodes") or [])}
        if whole:
            reasons.append(_r("waived", f"{name}: waived by {whole[0]['label']}", name))
            continue
        step_reasons: list[dict] = []
        if st is None:
            step_reasons.append(_r("incomplete", f"{name}: missing", name))
        elif st["state"] == "void":
            step_reasons.append(_r("void", f"{name}: void" + (f" ({st['reason']})" if st.get("reason") else ""), name))
        elif st["state"] in ("skipped", "not_run"):
            step_reasons.append(_r("incomplete", f"{name}: {st['state'].replace('_', ' ')}"
                                   + (f" ({st['reason']})" if st.get("reason") else ""), name))
        else:
            weak = command_diff(sp, st, allowed_env)
            if sp["kind"] == "pytest":
                shrink, unknown = node_diff(sp, st, covered_nodes)
                weak += shrink
                step_reasons += [_r("incomplete", f"{name}: {u}", name) for u in unknown]
            if weak:
                variance, v_notes = _covering(grants, receipt, name, "variance")
                digest = cmd_digest(st.get("effective_cmd"))
                if any(g.get("cmd_digest") == digest for g in variance):
                    reasons.append(_r("waived", f"{name}: variance accepted by "
                                      f"{next(g['label'] for g in variance if g.get('cmd_digest') == digest)}",
                                      name))
                else:
                    step_reasons.append(_r("fail", f"{name}: weakened: {'; '.join(weak)} "
                                           "(needs a variance grant)", name))
                    step_reasons += [_r("note", n, name) for n in v_notes]
            if sp["kind"] == "pytest":
                step_reasons += _eval_pytest(sp, st, receipt, covered_nodes)
            else:
                step_reasons += _eval_simple(sp, st)
        if not sp["required"]:
            reasons += [_r("note", r["text"], name) for r in step_reasons]
            continue
        if step_reasons and w_notes:
            step_reasons += [_r("note", n, name) for n in w_notes]
        reasons += step_reasons
    reasons += _eval_inputs(receipt, profile)
    return _combine(reasons)


def _combine(reasons: list[dict]) -> dict:
    levels = {r["level"] for r in reasons}
    verdict = "PASS"
    for level, v in (("fail", "FAIL"), ("void", "VOID"), ("incomplete", "INCOMPLETE")):
        if level in levels:
            verdict = v
            break
    order = {"fail": 0, "void": 1, "incomplete": 2, "waived": 3, "note": 4}
    reasons.sort(key=lambda r: order.get(r["level"], 9))
    return {"verdict": verdict, "reasons": reasons}


def headline(verdict: dict, stale: dict | None = None) -> str:
    """`STALE (was PASS): head_moved a1b2c3d → 9f8e7d6` / `VOID: fetch void; …`."""
    if stale:
        text = f"STALE (was {verdict['verdict']}): {stale.get('reason')}"
        if stale.get("prev") or stale.get("new"):
            text += f" {stale.get('prev') or '?'} → {stale.get('new') or '?'}"
        return text
    blocking = [r["text"] for r in verdict["reasons"] if r["level"] in LEVELS]
    waived = [r["text"] for r in verdict["reasons"] if r["level"] == "waived"]
    if not blocking:
        return verdict["verdict"] + (f" · {'; '.join(waived)}" if waived else "")
    return f"{verdict['verdict']}: " + "; ".join(blocking[:4]) + (
        f"; +{len(blocking) - 4} more" if len(blocking) > 4 else "")


# ---- staleness (§8) -------------------------------------------------------------

def arrival_stale(receipt: dict, binding: dict) -> dict | None:
    """Stale on arrival: the item already moved past this receipt's identity."""
    if binding.get("head") and binding["head"] != receipt["head_sha"]:
        return {"reason": "head_moved", "prev": short(receipt["head_sha"]),
                "new": short(binding["head"])}
    if binding.get("mf") and receipt.get("mf_sha") and binding["mf"] != receipt["mf_sha"]:
        return {"reason": "mf_moved", "prev": short(receipt["mf_sha"]), "new": short(binding["mf"])}
    if binding.get("ruling_id") and binding["ruling_id"] != receipt.get("ruling_id"):
        return {"reason": "ruling_superseded", "prev": receipt.get("ruling_id") or "(none)",
                "new": binding["ruling_id"]}
    return None


def superseded_by(old: dict, new: dict) -> dict | None:
    """Does a newer receipt for the same gate invalidate `old`?"""
    if old["head_sha"] != new["head_sha"]:
        return {"reason": "head_moved", "prev": short(old["head_sha"]), "new": short(new["head_sha"])}
    if old.get("mf_sha") and new.get("mf_sha") and old["mf_sha"] != new["mf_sha"]:
        return {"reason": "mf_moved", "prev": short(old["mf_sha"]), "new": short(new["mf_sha"])}
    return None


def binding_stale(receipt: dict, binding: dict, changed: set[str]) -> dict | None:
    """After item_set moved `changed` bindings, is `receipt` stale?"""
    if "head" in changed and binding.get("head") and binding["head"] != receipt["head_sha"]:
        return {"reason": "head_moved", "prev": short(receipt["head_sha"]),
                "new": short(binding["head"])}
    if "mf" in changed and binding.get("mf") and binding["mf"] != receipt.get("mf_sha"):
        return {"reason": "mf_moved", "prev": short(receipt.get("mf_sha")),
                "new": short(binding["mf"])}
    if "ruling_id" in changed and binding.get("ruling_id") \
            and binding["ruling_id"] != receipt.get("ruling_id"):
        return {"reason": "ruling_superseded", "prev": receipt.get("ruling_id") or "(none)",
                "new": binding["ruling_id"]}
    return None


def refresh_stale(receipt: dict, profile: dict | None, check: dict) -> dict | None:
    """Staleness from one refresh observation of the posting host."""
    if check.get("branch_head") and check["branch_head"] != receipt["head_sha"]:
        return {"reason": "head_moved", "prev": short(receipt["head_sha"]),
                "new": short(check["branch_head"])}
    runner = receipt.get("runner") or {}
    if check.get("runner_sha") and runner.get("sha") and check["runner_sha"] != runner["sha"]:
        return {"reason": "runner_changed", "prev": short(runner["sha"]),
                "new": short(check["runner_sha"])}
    base = receipt.get("base") or {}
    scope = ((profile or {}).get("base") or {}).get("scope", "paths")
    if check.get("base_sha") and base.get("sha_after") and check["base_sha"] != base["sha_after"]:
        if scope == "tree":
            tree = check.get("merge_tree")
            if tree and tree != base.get("merge_tree_after"):
                return {"reason": "base_moved", "prev": short(base["sha_after"]),
                        "new": short(check["base_sha"]), "detail": "merge tree changed"}
        else:
            closure = set((receipt.get("closure") or {}).get("paths") or [])
            changed = check.get("changed_paths") or []
            hit = [p for p in changed if p in closure]
            if check.get("changed_paths_truncated") or hit:
                return {"reason": "base_moved", "prev": short(base["sha_after"]),
                        "new": short(check["base_sha"]),
                        "detail": ("closure: " + ", ".join(hit[:3])) if hit
                        else "too many changed paths to check"}
    return None
