#!/usr/bin/env python3
"""Live-agent e2e for existing-session binding (runs ON cm-manager).

Proves role_sessions binding end-to-end with real Claude/Codex agents:

  Run A (fresh): give the worker a codeword held ONLY in its conversation
                 (never written to disk), let it take its turn, then STOP
                 the run (-> Detached, so the worker is idle + bindable).
  Run B (bind):  bind that warm worker via role_sessions={worker: uidA},
                 goal = "write the codeword I gave you to CODEWORD.md".

Pass iff: run B's worker is the SAME session (uid + transcript sid) as run
A's worker (adopted, not fresh-spawned) AND CODEWORD.md contains the
codeword (only the warm conversation could know it).
"""
import json, os, socket, struct, sys, time, subprocess

SOCK = os.path.expanduser("~/.cm/daemon.sock")
WT = "/tmp/cm-e2e-bind"
WS = "ws-e2e-bind"
CODEWORD = "PURPLE-OTTER-42"


def rpc(method, params=None, timeout=30.0):
    req = {"id": os.urandom(8).hex(), "caller": {"token_id": "e2e"},
           "method": method, "params": params or {}}
    body = json.dumps(req).encode()
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(SOCK)
    try:
        s.sendall(struct.pack(">I", len(body)) + body)
        (n,) = struct.unpack(">I", _recv(s, 4))
        resp = json.loads(_recv(s, n).decode())
    finally:
        s.close()
    if resp.get("ok"):
        r = resp.get("result")
        return r if r is not None else {}
    raise RuntimeError(f"{method} -> {resp.get('error')}")


def _recv(s, n):
    buf = b""
    while len(buf) < n:
        c = s.recv(n - len(buf))
        if not c:
            raise RuntimeError("EOF")
        buf += c
    return buf


def log(*a):
    print(*a, flush=True)


def setup_worktree():
    subprocess.run(f"rm -rf {WT}", shell=True, check=True)
    os.makedirs(WT)
    for c in ["git init -q", "git config user.email e2e@x", "git config user.name e2e",
              "echo seed > README.md", "git add -A", "git commit -qm seed"]:
        subprocess.run(c, shell=True, cwd=WT, check=True)
    log(f"[setup] git worktree ready at {WT}")


def workers_for_run(run_id):
    return [s for s in rpc("list_sessions")
            if s.get("workflow_run_id") == run_id and s.get("workflow_role") == "worker"]


def state(run_id):
    return rpc("get_workflow_state", {"run_id": run_id})


def wait_until(run_id, pred, timeout, what):
    t0 = time.time()
    last = None
    while time.time() - t0 < timeout:
        st = state(run_id)
        last = st
        if pred(st):
            return st
        time.sleep(5)
    raise TimeoutError(f"timed out waiting for {what}; last status="
                       f"{last.get('status')} iter={last.get('iteration')} "
                       f"active={last.get('active_role')}")


def main():
    setup_worktree()

    # ---- Run A: fresh, plant a codeword in the worker's conversation only ----
    goalA = (f"Reply with exactly: ACK. Also, please keep this codeword in mind "
             f"in case I ask you for it later in our conversation: {CODEWORD}.")
    runA = rpc("start_workflow", {"worktree": WT, "workspace_id": WS,
                                  "workflow_name": "feedback", "goal": goalA})["run_id"]
    log(f"[runA] started {runA} (fresh)")

    # Wait until the worker has actually taken its turn (handed off => iter>=2),
    # so the codeword is durably in its transcript/context.
    wait_until(runA, lambda s: (s.get("iteration") or 0) >= 2 or s.get("status") == "done",
               240, "runA worker to take its first turn")
    wks = workers_for_run(runA)
    if not wks:
        raise RuntimeError("runA: no worker session found")
    uidA = wks[0]["session_uid"]
    sidA = state(runA)["role_sessions"]["worker"].get("current_transcript_id")
    log(f"[runA] worker uid={uidA} sid={sidA}")

    # Stop run A so it is terminal (Detached) and the worker becomes bindable.
    rpc("stop_workflow", {"run_id": runA})
    log(f"[runA] stopped (detached); worker {uidA} now idle + bindable")
    time.sleep(3)

    # ---- Run B: BIND the warm worker; ask it to recall the codeword ----
    goalB = ("Write to a NEW file named CODEWORD.md the codeword I gave you "
             "earlier in our conversation — just the codeword on one line, "
             "nothing else. Then you are done.")
    runB = rpc("start_workflow", {"worktree": WT, "workspace_id": WS,
                                  "workflow_name": "feedback", "goal": goalB,
                                  "role_sessions": {"worker": uidA}})["run_id"]
    log(f"[runB] started {runB} with role_sessions={{worker: {uidA}}}")

    # Immediately verify ADOPTION (not fresh spawn):
    time.sleep(2)
    stB = state(runB)
    sidB = stB["role_sessions"]["worker"].get("current_transcript_id")
    wksB = workers_for_run(runB)
    uidsB = [w["session_uid"] for w in wksB]
    log(f"[runB] worker sid={sidB}; worker session uids tagged to runB={uidsB}")
    adopt_sid = (sidB == sidA and sidA is not None)
    adopt_uid = (uidA in uidsB)
    # No NEW worker session should have been spawned for runB:
    no_fresh = (uidsB == [uidA])
    log(f"[check] adopt_sid={adopt_sid} adopt_uid={adopt_uid} no_fresh_worker={no_fresh}")

    # Wait for run B to finish.
    stB = wait_until(runB, lambda s: s.get("status") == "done", 600, "runB to reach done")
    log(f"[runB] status={stB.get('status')} iter={stB.get('iteration')}")
    log(f"[runB] done_reason: {(stB.get('done_reason') or '')[:400]}")

    # Disk-level proof: CODEWORD.md must contain the codeword.
    cw_path = os.path.join(WT, "CODEWORD.md")
    cw = ""
    if os.path.exists(cw_path):
        cw = open(cw_path).read().strip()
    log(f"[check] CODEWORD.md = {cw!r}")
    codeword_ok = CODEWORD in cw

    log("\n==== VERDICT ====")
    log(f"  adopted same sid (no respawn) : {adopt_sid}")
    log(f"  worker uid reused (bound)     : {adopt_uid}")
    log(f"  no fresh worker spawned       : {no_fresh}")
    log(f"  runB reached done             : {stB.get('status') == 'done'}")
    log(f"  warm codeword recalled to disk: {codeword_ok}  (only the bound conversation knew {CODEWORD})")
    ok = adopt_sid and adopt_uid and stB.get("status") == "done" and codeword_ok
    log(f"  RESULT: {'PASS' if ok else 'FAIL'}")

    # Best-effort cleanup of e2e sessions (leave nothing running).
    try:
        for s in rpc("list_sessions"):
            if s.get("workspace_id") == WS:
                try:
                    rpc("kill_session", {"session_uid": s["session_uid"]})
                except Exception:
                    pass
        log("[cleanup] killed e2e sessions")
    except Exception as e:
        log(f"[cleanup] {e}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
