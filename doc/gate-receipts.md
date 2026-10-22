# Gate receipts on board items: design

**Status:** design revision 2 (2026-10-22, items lane); the §11 v1 cut is
implemented on `items/gate-receipts`. §13 lists where v1 differs in detail
from the text above. Not deployed.

**Sources:**
- EP's round-2 and round-3 interview answers
  (`predictionTrading-swarm-focused-design/cm-initiative/shared/ep-interviews.md`);
- EP's review of revision 1 (`0e64d4a`), posted in
  `#initiative/swarm-focused-design/claude-manager` at 2026-10-21 14:07Z;
- Swarm-Gardener's posts in that channel (2026-10-21 14:03Z, 2026-10-22 14:03Z);
- item 2 of `~/.local/share/swarm-coord/org-design/org-tools.md`.

It builds on the items board contract in [items-board.md](items-board.md).

**Changes since revision 1 (`0e64d4a`):**
- **Waivers.** A waiver is now a grant: the grantor attaches it, and it links
  the grantor's message (§4). A free-text or unattached waiver never satisfies
  a step.
- **VOID.** VOID is its own verdict, separate from INCOMPLETE. Repeated VOIDs
  raise a flag (§3, §7).
- **Grant vs executed.** The receipt records the effective command and the set
  of test nodes that actually ran, and compares them with what was granted. A
  weakened run is RED until the grantor acknowledges it (§5).
- **Inputs.** External inputs and resolved roots are bound by content hash.
  Derived artifacts also bind the receipt that produced them (§6).
- **Must-fail.** Each MF node expects a declared exception type. Explicit
  `skips` and `deselected` lists are required, and a missing list means
  INCOMPLETE (§1, §2).
- **Profiles.** Steward removed from the profile editors. Every profile edit is
  logged, and every edit restales that class's receipts (§2).
- **Base scope.** Defaults to `paths` over the gate's closure; `tree` is a
  per-profile override (§8).
- **Attestation.** A profile can require the cm-gate runner; this is off by
  default (§2).
- **New §11, v1 cut.**

## 0. The problem

Today a gate result is a long prose post such as "GREEN: MF a1b2 fails, pytest
412 passed, mypy clean". From posts mixed in with everything else, the reader
has to work out four things:

- Is this result for the current head?
- Did every required step actually run, on the intended inputs?
- Was it the granted command and test selection, or a narrowed one?
- Was it run under the ruling now in force?

EP has ruled on superseded MF SHAs. It has seen FETCH_FAIL after a pytest pass,
and a320 nodes "pass" by skipping on a host without the harvest. And a gate
started 12 s after a recompose ruling that its runner hadn't seen.

A receipt is a structured record attached to the item. **CM** computes its
verdict from:

- the recorded steps;
- the board's declared requirements;
- the grants that cover the run.

The poster's word is never enough. CM marks the receipt STALE, with a reason,
as soon as any identity or input it was bound to changes.

## 1. Data

### 1.1 Receipt

One row per gate run, attached to `(board, item n)`. Receipts are append-only.
A rerun writes a new receipt, and the old one stays in history.

| Field | Type | Notes |
|---|---|---|
| `id` | `r-<board>-<n>-<seq>` | stable and quotable (`r-sejd-14-3`) |
| `gate` | str ≤ 40 | default `gate`; an item can carry several gates, each with its own latest receipt |
| `class`, `profile_version` | str, int | the requirement profile (§2) and the version evaluated at post time |
| `repo`, `branch` | str | `branch` drives the refresh (§8) |
| `head_sha` | 40-hex | **required** |
| `mf_sha` | 40-hex? | required when the profile has a must-fail step |
| `runner` | `{ref, sha}` | runner/driver script, e.g. `origin/main:scripts/gate.sh` and the SHA it resolved to |
| `ruling_id` | str? ≤ 80 | the lane ruling the run was made under (§10) |
| `base` | object | `{ref, sha_before, sha_after, merge_tree_before, merge_tree_after}`; merge tree = `git merge-tree --write-tree <base> <head>` OID, or `"conflict"` |
| `closure` | `{paths_digest, count, cmd}` | the gate's closure file set (identity-closure walker plus the gate's file list), digested; the paths themselves go to the log |
| `worktree` | `{head_before, head_after, dirty_before, dirty_after}` | identity of the checkout the run used |
| `inputs` | `[input]` ≤ 100 | §6 |
| `steps` | `[step]` ≤ 40 | **every** planned step, including failed, skipped and never-started ones |
| `waivers` | `[grant id]` ≤ 20 | grants the runner **cites** (§4); citing proves nothing by itself |
| `log_path` | str? | the full log on the posting host |
| `attestation` | `self` | always `self` in v1: never taken from the poster (§13) |
| `poster`, `posted_at` | actor, ts | stamped by the daemon and the API clock |
| `computed` | `{verdict, reasons}` | the verdict at post time, kept for history |

### 1.2 Step

| Field | Type | Notes |
|---|---|---|
| `name` | str ≤ 40 | matches a profile step (`must_fail`, `pytest`, `mypy`, `ruff`, `closure`, `identity`, `fetch`, …) |
| `effective_cmd` | `{argv, env, cwd}` | exactly what ran, after every override and expansion; `env` includes only the keys the run set or overrode |
| `at_sha` | 40-hex? | defaults to `head_sha`; must-fail runs at `mf_sha` |
| `state` | `ran` \| `skipped` \| `not_run` \| `void` | `void` = no result because of infra trouble or a missing input (FETCH_FAIL, missing harvest, timeout, OOM) |
| `rc` | int? | the step's own exit code (`ran` only) |
| `started_at`, `ended_at` | ts? | |
| `summary` | str ≤ 2000 | the tool's own summary lines, verbatim |
| `nodes` | `[{id, outcome, exc_type?}]` | pytest steps only, and **required** for them: every collected node with its outcome (`passed`, `failed`, `error`, `skipped`, `xfailed`, `xpassed`, `deselected`), plus the exception type for failures. cm-gate takes this from JUnit XML |
| `skips` | `[{node, reason}]` | **required** on pytest steps; `[]` = no skips. A missing list is INCOMPLETE |
| `deselected` | `[{node, reason}]` | **required** on pytest steps; `[]` = nothing deselected. A missing list is INCOMPLETE |
| `parsed` | object | filled by **CM**, never by the poster. pytest counts are derived from `nodes` and checked against `summary`; mypy `{ok, errors, files}`; ruff `{violations}` |
| `inputs_used` | `[input name]` | which of the receipt's inputs this step read |
| `reason` | str? ≤ 300 | why it was skipped, not run or void |

CM checks three things against each other: `summary`, `rc` and `nodes`. Any
disagreement marks the step `inconsistent`, and an inconsistent step never
satisfies a requirement. Examples:
- rc 0 while some nodes failed;
- a count in `summary` that doesn't match `nodes`;
- mypy reporting "Success" with rc 1.

If a step type the profile needs parsed can't be parsed, the step is
`unparsed`, which is also never a pass.

## 2. Requirements: gate profiles

A board stores **gate profiles**, keyed by `class`. The board also names a
default class, and an item can take `gate_class`. A profile lists everything a
valid run must contain. It is also the **grant**: the commands, the test
selection and the pass predicates that were approved (§5).

```json
{"class": "py-lane", "version": 4,
 "steps": [
   {"name": "must_fail", "kind": "pytest", "at": "mf", "required": true,
    "argv": ["pytest", "-p", "no:cacheprovider", "{mf_nodes}"],
    "nodes": ["tests/sejd/test_fuse.py::test_new_path"],
    "expect_fail": {"default": "AssertionError",
                    "tests/sejd/test_fuse.py::test_new_path": ["AttributeError"]}},
   {"name": "pytest", "kind": "pytest", "required": true,
    "argv": ["pytest", "-n", "2", "tests/sejd", "tests/a320"],
    "min_passed": 1, "max_failed": 0, "xfail_counts_as_pass": false,
    "must_run": ["tests/a320/test_nodes.py"],
    "skip_policy": {"allow_reasons": ["^requires CUDA$"]}},
   {"name": "mypy", "kind": "mypy", "required": true, "max_errors": 0},
   {"name": "ruff", "kind": "ruff", "required": true, "max_violations": 0},
   {"name": "closure", "kind": "rc", "required": true},
   {"name": "identity", "kind": "rc", "required": true}
 ],
 "inputs": [{"name": "harvest_a320", "kind": "dir", "root_env": "A320_HARVEST",
             "required": true}],
 "require": {"mf_sha": true, "runner": true, "ruling_id": false,
             "attestation": null},
 "base": {"ref": "origin/main", "scope": "paths"}}
```

- **Kinds.** `kind` picks the parser and the pass test: `pytest`, `mypy`,
  `ruff`, or `rc` (exit code only).
- **Must-fail.** A must-fail step passes only if both conditions hold:
  - it **ran** at `mf_sha`;
  - every listed node **failed** with its expected exception type. The
    default is `AssertionError`; per-node alternatives are declared by node
    id, e.g. `AttributeError` for a new symbol, or pytest's `Failed` for "DID
    NOT RAISE".

  Collection errors and import errors never count. Neither does a
  missing-data `pytest.fail`: a node that can't run for lack of data must be
  declared in `deselected` with a reason.
- **`must_run`.** Lists node ids or path prefixes that must appear in `nodes`
  as run. If one is skipped, deselected or absent, the step is INCOMPLETE.
- **`skip_policy`.** A skip whose reason doesn't match `allow_reasons` makes
  the step INCOMPLETE. A skip with no reason is never allowed.
- **Pass predicates** (`max_failed`, `xfail_counts_as_pass`, `max_errors`, …)
  come only from the profile. A run can't change them (§5).
- **`require.attestation: "cm-gate"`** needs CM-verified run evidence, which
  v1 does not have: such a profile computes INCOMPLETE until it does (§13).
  It is off by default; EP wants it on for deploy-gate profiles first.

**Who edits profiles.** Owner, the board's orchestrator, and the pids in the
board setting `gate_editors` (intended for EP). Nobody else, the steward
included; anyone else gets `403`. Every edit:
- needs a `reason`;
- is written as an `item_events` row `gate_profile_changed` with who, the old
  profile, the new profile and the reason;
- bumps the version;
- **restales every receipt of that class**, with reason
  `profile_changed v3 → v4`, whether or not the verdict would change.

A board with no profile for a class has nothing declared, so its receipts
compute INCOMPLETE ("no gate profile for class X"). An empty profile can't
mean everything passed.

## 3. Verdict

CM evaluates the verdict on post and again whenever a binding, grant or
profile changes. Values, in order of precedence:

1. **STALE.** A binding changed (§8). The earlier verdict is still shown:
   `STALE (was PASS): head moved a1b2c3d → 9f8e7d6`.
2. **FAIL** (EP's RED). Any of:
   - a required step **ran** and missed its pass test;
   - an MF node passed, or failed with an undeclared exception type;
   - a step is `inconsistent`;
   - the run is **weakened** relative to the grant and the grantor hasn't
     acknowledged it (§5). A weakened run is RED until acknowledged.
3. **VOID.** A required step is `void` (FETCH_FAIL, timeout, OOM), or a
   required input is missing or couldn't be fetched (§6). VOID never counts as
   RED and doesn't use up a one-run budget. CM counts VOIDs per runner SHA and
   per input and raises a flag on repeats (§7).
4. **INCOMPLETE.** The run didn't establish the result, though nothing was
   red or void. Any of:
   - a required step missing, `skipped`, `not_run` or `unparsed`;
   - a pytest step with no `nodes`, `skips` or `deselected` list;
   - a vacuous pytest result (`min_passed` not met, or a `must_run` node not
     run);
   - a skip that `skip_policy` doesn't allow;
   - a required identity missing (`mf_sha`, `runner`, `closure`);
   - a required input present but not bound by content (§6);
   - a `self` receipt where the profile requires `cm-gate`;
   - code that moved during the run (`worktree.head_before ≠ head_after`, a
     dirty worktree, or `merge_tree_before ≠ merge_tree_after`);
   - a cited waiver that is unverified (§4) and is the only thing standing in
     for its step.
5. **PASS.** Every required step is present, `ran` and met its pass test, or
   is covered by a **verified** waiver. In addition, every required input is
   bound by content, and the run is within its grant.

Precedence is STALE > FAIL > VOID > INCOMPLETE > PASS. Each verdict lists
**all** of its reasons, not just the winning one, e.g.
`VOID: fetch void (FETCH_FAIL); also INCOMPLETE: a320 nodes skipped "harvest missing"`.
Non-required steps are shown but don't affect the verdict.

The verdict counts runs per `(item, gate, head)`: `FAIL ×1, VOID ×2`.
Enforcing EP's one-run budget is EP's policy; CM just keeps the count visible.

## 4. Waivers are grants

A waiver stands in for a required step, or some of its nodes, on one exact run.
The grantor (EP, or Owner for Owner-only items) posts it as a chat message
naming:
- the exact HEAD SHA, or base→head;
- the exact check skipped (node ids or a step name);
- the reason and the real risk.

The **grantor** then attaches it:

```
gate_waive(n, head, step, nodes=None, base=None, reason, risk, message_id,
           gate="gate", board=None) -> {grant: {id, verified, problems}}
```

The tool creates a grant `g-<board>-<n>-<seq>` with `kind: waiver`. The
daemon stamps the caller as the grantor. **Only the grantor attaches**; a
runner can only cite the grant id in a receipt's `waivers`.

A grant is **verified** only if every check below holds. The daemon does the
message checks against its own message store before forwarding; the API does
the rest:

1. The grantor is in the board's `waiver_grantors`: Owner always, plus the
   listed pids (EP).
2. `message_id` exists, its sender is the grantor, and its body contains the
   grant's full 40-character head SHA as a token (and the base SHA when one
   is given), the step and the kind of grant (§13). A message the daemon can't read yet stays `verified: false` with
   `problems: ["message not found"]` and is rechecked on the next read.
3. `reason` and `risk` are both non-empty. `step` names a step in the profile,
   and any `nodes` belong to that step's grant. A generic "the gate" waiver is
   refused.
4. The grant is not revoked. `gate_waive(..., revoke=g-…)` revokes one,
   grantor only.

A cited grant **covers** a receipt only if:
- it is verified;
- its head equals the receipt's `head_sha`, and its base matches when one is
  given;
- its step and nodes include what the receipt is missing.

Otherwise it shows as `waiver g-sejd-14-2: unverified (head 9f8e… ≠ grant
a1b2…)` and satisfies nothing. A wrong-SHA, generic or unattached waiver
therefore can never yield PASS.

Grants are also how the grantor acknowledges a weakened run (§5, `kind:
variance`), so there is one mechanism. When ruling records arrive (§10),
grants become ruling records of type `waiver` / `variance`.

## 5. Grant vs executed

The profile's steps are the granted commands and the granted test selection.
Every step records `effective_cmd` and, for pytest, the executed `nodes`. CM
diffs the run against the grant in two ways.

**Node set.** cm-gate collects the granted selection at the run's SHA
(`pytest --collect-only` with the granted argv) and stores it as
`granted_nodes`. The executed set is every node in `nodes` that wasn't
deselected. The rules:
- A granted node that is missing from the executed set, or deselected, is
  **shrinkage**.
- Executed nodes outside the granted set are **additive**. They are allowed
  within the grant's paths.

For a `self` receipt with no `granted_nodes`, CM can't verify the selection.
The step is INCOMPLETE, unless the profile lists nodes explicitly and the run
covers all of them.

**Command.** `effective_cmd.argv` and `env` are compared with the profile
`argv`. These differences are **allowed** without review:

| Allowed | Recognized as |
|---|---|
| worker count ≤ 2 | `-n N` / `--numprocesses N` with N ≤ 2 |
| timeout, memory cap, CPU pinning | `--timeout`, `timeout N` prefix, `systemd-run … -p MemoryMax=`, `taskset`/`numactl` prefixes |
| env needed to run | keys in the profile's `allowed_env` (`PATH`-like roots, `JAX_PLATFORMS`, fixture roots); their values are bound as inputs (§6) |
| additive nodes/files | extra positional paths or node ids |

**Review is forced** by any other difference, specifically:
- a granted node or file removed or deselected;
- a `-k`/`-m` filter narrower than granted (any added filter counts);
- a changed MF SHA, base or comparison mode (e.g. HEAD_ONLY logic);
- skipping the mypy, closure or identity step;
- a changed pass predicate (XFAIL counted as pass, rc ignored, a
  `|| true`-shaped wrapper);
- any argv token CM doesn't recognize.

A run needing review is **weakened**. Its verdict is FAIL with reason
`weakened: <diff>` until the grantor attaches a `variance` grant:

```
gate_waive(n, head, step, kind="variance", cmd_digest=<sha256 of effective_cmd>,
           reason, risk, message_id)
```

The grant is bound to that exact head and that exact effective command. A
variance grant lets the step be judged on its own pass test; it doesn't make a
red step green. **Runner attestation never substitutes:** a `cm-gate` receipt
of a weakened run is FAIL until acknowledged, exactly like a `self` receipt.

Lane-local override files (cm-gate `--override`) are simply a source of
effective-command differences. They go through the same diff.

## 6. Inputs bound by content

A run's result depends on data that can change while the code SHA stays the
same. Every external input the run reads is listed in `inputs`. Examples:
- warm files and pin maps;
- fixtures and tapes outside git;
- ECFG and the release manifest;
- captured roots;
- harvested outputs from another run;
- DB data.

| Field | Notes |
|---|---|
| `name` | matches the profile's `inputs[].name`, or is free for an undeclared input |
| `kind` | `file`, `dir`, `gcs`, `artifact`, `db` |
| `locator` | how it was found: path, `gs://…`, or query + window |
| `root_env`, `resolved_root` | for env-derived roots: the variable and the path it resolved to. The resolved path is bound along with its hash |
| `content_id` | `sha256:<hex>` for a file; `tree:<sha256 of a sorted (path, size, sha256) manifest>` for a dir; `gcs:<generation>/<md5 or crc32c>`; for `db`, see below |
| `bytes` | total size, to catch size mix-ups like 12.98 vs 6.55 GB |
| `produced_by` | `artifact` only: the receipt id that produced it |
| `state` | `bound`, `missing` (not there, or the fetch failed), `unbound` (present, but without a content id) |

- **Missing or failed fetch.** A `missing` required input makes the run
  **VOID**, never PASS. This covers FETCH_FAIL and a host without the harvest.
- **Unbound.** An `unbound` required input makes the run INCOMPLETE. The run
  happened, but CM can't say what it ran against.
- **Changed.** If an input's content id differs from a later observation, the
  receipt goes **STALE** with `input_changed <name>`. Observations come from a
  later receipt on the same board binding the same locator, or from the
  refresh in §8.
- **Derived artifacts** carry `produced_by`. If the producing receipt is STALE,
  FAIL or VOID, every receipt that consumed the artifact goes STALE with
  `input_receipt_invalid r-…`. The consumer binds the id, so this cascade
  happens in the database, with no rehashing.
- **DB inputs** need an immutable identity. A query plus a time window is only
  a `locator`: rows inside the window can still change, so on its own it is
  `unbound`. Acceptable `content_id`s:
  - `snapshot:<id>`, an immutable snapshot or export the profile names as
    immutable, such as a dump file, recorded with its sha256;
  - `rows:<sha256>`, a digest the step computes over the ordered result set
    it actually read. The step records the digest query in `locator`.

## 7. Flags

These are new board flag kinds. They use the same lifecycle and `flag_pushed`
dedupe as the existing flags in items-board.md §4.

| Kind | Raised when | Goes to |
|---|---|---|
| `receipt_stale` | an open item's latest receipt for a gate went STALE | holders only; never escalated |
| `runner_void` | ≥ 2 VOIDs on the same runner SHA within 24 h, on any item on the board (detail: `runner VOID ×N`, runner SHA, receipts) | holders and orchestrator |
| `input_void` | ≥ 2 VOIDs on the same input locator within 24 h (detail: `input <name> VOID ×N`) | holders and orchestrator |

`receipt_stale` resolves when a new receipt for that gate is posted or the
item closes. The VOID flags resolve on the next non-VOID receipt that uses
that runner or input. FAIL and INCOMPLETE don't raise flags, because the
holder already knows; they show up in the board read.

## 8. Staleness

| Binding | Detected when | Reason |
|---|---|---|
| head SHA | a newer receipt names another head, `item_set(n, head=…)`, or the refresh sees `branch` move past `head_sha` | `head_moved` |
| MF SHA | a newer receipt or `item_set(n, mf=…)` | `mf_moved` |
| runner SHA | the refresh resolves `runner.ref` to another SHA | `runner_changed` |
| ruling ID | the item's `gate_binding.ruling_id` differs from the receipt's (§10) | `ruling_superseded` |
| base | the base moved, as defined by the profile's `base.scope` (below) | `base_moved` |
| profile | any edit of the receipt's class | `profile_changed` |
| input | a changed content id, or an invalid producing receipt (§6) | `input_changed` / `input_receipt_invalid` |
| grant | a cited grant was revoked | `grant_revoked` |

- **Base scope.** The default is `paths`: the receipt goes stale only when the
  base change since `base.sha_after` touches the gate's closure (the
  identity-closure walker's file set plus the gate's file list, recomputed at
  the receipt's head). EP chose this because `tree` would restale every gate on
  every unrelated landing. Compose and deploy gates set `scope: "tree"`, which
  restales on any base move that changes the merge tree, because those gates
  must be exact on the final SHA.
- **Stale on arrival.** A receipt posted against a head, MF or ruling that the
  item has already replaced is STALE at once. This catches a gate started
  12 s after a recompose ruling.
- **Refresh.** The API server has no checkouts, so the posting host's daemon
  checks the moving bindings.
  - For each repo, it runs one `git fetch` of the base and runner refs every
    5 minutes, covering every latest, non-stale receipt on an open item.
  - It runs the closure/merge-tree check and posts what it observed.
  - The API decides staleness.

  If no check has landed for 15 minutes, the receipt keeps its verdict but
  shows `base unchecked 40m`. CM doesn't call it STALE without evidence, and
  doesn't hide the gap either.
- **One-way.** A stale receipt never becomes valid again; a rerun posts a new
  one.
- Each transition is an `item_events` row (`receipt_stale`, with reason, prev
  and new), so it moves the board version.

## 9. Tools

**MCP** (daemon-routed like the other item tools; the daemon stamps `actor`):

```
gate_report(n, steps, head, mf=None, runner=None, ruling=None, base=None,
            closure=None, worktree=None, inputs=None, waivers=None,
            gate="gate", klass=None, log_path=None, board=None, request_id=None)
  -> {receipt: {id, verdict, reasons, stale?}, item}

gate_receipts(n, gate=None, history=False, board=None)
  -> {latest: {<gate>: receipt}, history?, grants}

gate_waive(n, head, step, reason, risk, message_id, kind="waiver",
           nodes=None, base=None, cmd_digest=None, revoke=None, gate="gate",
           board=None)
  -> {grant: {id, verified, problems}}

gate_profile(klass, profile=None, reason=None, default=False, board=None)
  -> {profile}       # read when profile is None; write: editors only, reason required
```

- `item_set` gains `head`, `mf`, `ruling` and `gate_class`. Setting `head`,
  `mf` or `ruling` moves a binding without a rerun, and restales the current
  receipts.
- `board(view="slim")` shows the latest receipt per gate on the item line:
  `#14 active "fuse SEJD" … ✓PASS 9f8e7d6 r-sejd-14-3` or
  `⟲STALE head_moved (was PASS)`. `view="full"` and `gate_receipts` show the
  steps.
- `request_id` makes a retried `gate_report` return the first receipt.

**`scripts/cm-gate`**, the intended way to post:

```
cm-gate run --item 14 [--gate gate] [--override lane.toml]
```

It reads the item's profile, then:
- runs each step's `argv` at the right SHA, with JUnit XML for pytest;
- collects the granted node set;
- runs the profile's `closure_cmd`;
- hashes the declared inputs;
- captures the worktree and merge-tree identity before and after;
- posts through the caller's daemon, which rehashes local inputs (§13).

Every planned step is posted, so a step that never started appears as
`not_run` rather than being left out.

**Planning API** (`api/items.py`; logic in `dispatch/gate_db.py`):

| Method + path | Body | Returns |
|---|---|---|
| `POST /boards/{ref}/items/{n}/receipts` | `{actor, receipt, request_id?}` | `{board, n, receipt}` (the computed verdict, `runs`, `stale?`); a repeated `request_id` replays the first receipt |
| `GET /boards/{ref}/items/{n}/receipts` | `?gate=&history=false` | `{n, class, binding, latest: {gate: receipt}, grants, history?}` |
| `POST /boards/{ref}/items/{n}/grants` | `{actor, kind, gate, head, base?, step, nodes?, cmd_digest?, reason, risk, message_id, message_check}` | `{grant}`; 403 unless the actor is a grantor, 422 for a generic or mis-scoped grant |
| `PATCH /boards/{ref}/items/{n}/grants/{id}` | `{actor, revoke: true}` or `{actor, message_check}` | `{grant}` |
| `GET /boards/{ref}/gate-profiles` | `?class=&n=` | `{class, profile, classes, default_class, gate_editors, waiver_grantors, binding?}` |
| `PUT /boards/{ref}/gate-profiles/{class}` | `{actor, profile \| null, reason, default}` | `{profile, version, restaled}` |
| `POST /receipts/refresh` | `{daemon_id, checks: [{receipt_id, branch_head?, runner_sha?, base_sha?, changed_paths?, changed_paths_truncated?, merge_tree?, error?}]}` | `{stale: [id], watch: [{receipt_id, path, branch, head, base_ref, base_sha_after, runner_ref, scope}]}` |
| `PATCH /boards/{ref}` | `{gate_default_class?, gate_editors?, waiver_grantors?}` | board header; the two lists are Owner-only |

`board()` reads carry `gate: {<gate>: {id, verdict, was, headline, head, stale,
unchecked_s?, reasons, steps}}` on each item.

**TUI (Alt+B).** A gate cell after the status badge: `✓PASS` green, `✗FAIL`
red, `⊘VOID` magenta, `◐INC` amber, `⟲STALE` dim. The detail pane gets a
**Gate** section with one block per gate:
- the verdict and every reason;
- the bound SHAs, with the stale reason highlighted;
- one line per step, e.g. `✓ pytest 412 passed · 3 skipped (requires CUDA ×3)`
  or `⊘ fetch void FETCH_FAIL`;
- the inputs, with size and short content id;
- the grants, with their verified state;
- the receipt id and the run counts (`FAIL ×1 · VOID ×2 on 9f8e7d6`).

The banner adds `N stale receipts` and any `runner VOID ×N`.

## 10. Storage (migration `023_item_receipts.sql`, number claimed at implementation)

Uses the items style: rerunnable DDL only, no row UPDATEs, every write inside
the board-row lock, and every change an `item_events` row. The DDL is
`sql/023_item_receipts.sql`:

- `item_receipts`: one row per run, with the identities, steps, inputs,
  `computed` (the verdict at post time) and `verdict` (the latest
  evaluation; a grant attached later can change it), `stale_at` /
  `stale_reason`, `checked_at`, and the poster's pid and daemon id (the
  refresh watches only its own host's receipts).
- `item_receipt_inputs`: `(receipt, name) → locator, content_id, state,
  produced_by`, for `input_changed`, `input_void` and the cascade.
- `item_grants`: waiver and variance grants with the daemon's
  `message_check`.
- `items.gate_class`, `items.gate_binding {head, mf, ruling_id}`;
  `boards.gate_profiles {class: profile}`, `gate_default_class`,
  `gate_editors`, `waiver_grantors`.

The verdict logic is a pure function in `dispatch/gate_rules.py`:
`evaluate(receipt, profile, grants) -> {verdict, reasons}`, with the summary
parsers, the grant-vs-executed diff and the staleness rules beside it. It
can be tested without a database.

A receipt is at most 4 MiB serialized (a large suite's `nodes` list is the
bulk). Full logs stay at `log_path`. Archiving an item archives its receipts and grants.

**Later: question/ruling records (not this batch).** EP's companion ask is a
record of question, asker, decision owner, status (open / answered /
superseded), answer, ruling id with timestamp, and bound SHAs/items. How it
connects to receipts:
- **Until it exists**, `ruling_id` is free text, compared only with the item's
  `gate_binding.ruling_id` (`item_set(n, ruling="EP-A1021")`).
- **Once it exists**, the current ruling for an item is the newest answered,
  non-superseded ruling that binds the item, its group or its lane.
  - Answering a superseding ruling restales every receipt bound to the old
    one.
  - A receipt citing an unknown or still-open ruling computes INCOMPLETE.
- **Grants become rulings** of type `waiver` / `variance`. The ruling record
  replaces `message_id` as the link, and the head-SHA and step binding rules
  of §4 carry over unchanged.

## 11. v1 cut

The goal: EP gets a trustworthy PASS / FAIL / INCOMPLETE / VOID / STALE on
real sejd items with the least machinery. **Trustworthy** means a PASS in v1
is never weaker than the full design's. Where v1 can't check something, the
answer is INCOMPLETE or an explicit "unchecked" note, never a silent PASS.

**In v1:**

| Area | v1 scope |
|---|---|
| Storage | migration 023 as in §10: receipts, receipt inputs, grants, and the item and board columns |
| Profiles | `gate_profile` read/write by Owner, orchestrator and `gate_editors`; edit log; restale-all-of-class on edit; `require.attestation` |
| Evaluator | the full §3 rules, in a pure function with unit tests for every EP case in §12 |
| pytest evidence | per-node outcomes from JUnit XML (cm-gate), MF expected exception types, explicit `skips`/`deselected` with absent = INCOMPLETE, `must_run`, `skip_policy` |
| mypy, ruff, rc steps | summary parsing plus rc cross-check |
| Grant vs executed | granted node collection plus node-set diff; argv/env diff against the allowlist; any unrecognized token forces review; `variance` grants |
| Waivers | `gate_waive` with daemon message verification (sender, head SHA in body); `waiver_grantors`; revoke |
| Staleness | event-driven bindings (new receipt, `item_set` head/mf/ruling, profile edit, grant revoke, derived-artifact cascade); refresh from the posting host for base (`paths` and `tree`) and runner |
| Inputs | **partial, see below** |
| Flags | `receipt_stale`, `runner_void`, `input_void` |
| Tools | `gate_report`, `gate_receipts`, `gate_waive`, `gate_profile`, `item_set` bindings, the slim-board gate cell, `scripts/cm-gate run` |
| TUI | Alt+B gate cell, Gate detail section, banner counts |

**Inputs in v1 (partial content hashing, stated explicitly).**
- **Rehashed by the reporting daemon:** `file` (sha256), `dir` (manifest
  digest) and `artifact` inputs with a local path (hash plus `produced_by`).
  Only these are `hashed_at_run`. `gcs` identities (generation plus
  md5/crc32c, which cm-gate reads from object metadata) are self-supplied and
  unverified, and bind only where the profile's `accept_unverified` lists
  `gcs`.
- **`db` inputs:** v1 accepts only `snapshot:` or `rows:` content ids that the
  step itself supplies. cm-gate doesn't compute them, and CM doesn't verify
  them, so they bind only where `accept_unverified` lists `db`. A bare
  query/window is `unbound`, so the run is INCOMPLETE.
- **Undeclared inputs:** inputs the run reads but the profile doesn't declare
  aren't detected. Coverage is only as good as the profile's `inputs` list.
- **What a v1 PASS means for inputs:** the declared inputs had these content
  ids **when the run happened**. An input that changes after the post is
  caught only by a later receipt binding the same locator, or by the
  derived-artifact cascade. v1 doesn't rehash or re-poll inputs in the
  background. The Gate section prints this as `inputs checked at run time`.

**Deferred** (each can follow without changing the v1 data):
- background input re-verification in the §8 refresh (rehash dirs, poll GCS
  generations);
- cm-gate computing `rows:` digests for DB inputs, and CM verifying snapshot
  ids;
- detecting undeclared inputs (e.g. by tracing file opens);
- refresh takeover by another host with the same repo. In v1, if the posting
  host stops, the receipt shows `base unchecked Nm`;
- question/ruling records (end of §10), and with them `ruling_superseded`
  driven by the record instead of `item_set`;
- grants that link ruling records instead of chat messages.

**Order of work** (for the go):
1. Migration and evaluator with EP-case tests.
2. API routes and MCP tools.
3. `cm-gate run`.
4. Refresh and flags.
5. TUI.

Steps 1–3 give real receipts on sejd. Steps 4–5 add base/runner staleness and
the Alt+B view.

## 12. Checked against EP's cases

| EP case | Outcome |
|---|---|
| Ruling on a stale MF SHA | `item_set(mf=…)` or a newer receipt gives `STALE mf_moved a1b2 → c3d4`; a receipt posted against the old MF is stale on arrival |
| RED/VOID reruns | one receipt per run; history keeps each run as FAIL or VOID with its reasons, and the run counts show `FAIL ×1 · VOID ×2 on 9f8e7d6`. VOID doesn't use up the one-run budget |
| FETCH_FAIL ×2 on the same runner | each run is VOID, and the second raises `runner_void` (`runner VOID ×2`) to holders and the orchestrator |
| FETCH_FAIL after a pytest pass | the fetch step is `void`, so the run is VOID; the pytest pass can't lift it |
| Vacuous pass from missing-harvest skips | the required input `harvest_a320` is `missing`, so VOID; also `must_run` a320 nodes skipped, so INCOMPLETE is listed as well; and a non-allowed skip reason. Never PASS |
| MF failing by import error, or by a missing-data `pytest.fail` | the node's failure type isn't the declared one, so FAIL; a missing-data node must be listed in `deselected` |
| Lane override drops nodes or adds `-k` | node-set shrinkage or a narrower filter makes the run weakened, so FAIL until EP attaches a `variance` grant for that head and command digest |
| Worker count 2, a timeout and `JAX_PLATFORMS=cpu` | allowed differences; the value of `JAX_PLATFORMS` is recorded |
| Waiver naming another SHA, or "the gate" | unverified: it doesn't cover the run, so not PASS |
| Staging artifact 12.98 vs 6.55 GB | the input's `bytes` and `content_id` are bound; a consumer that read a different artifact binds a different id and `produced_by` |
| Gate started just after a new ruling | the receipt's `ruling_id` differs from the item's, so STALE on arrival with `ruling_superseded` |
| Unrelated landing on main | `paths` scope: no closure file touched, so the receipt stays valid; a deploy gate with `tree` scope restales |

## 13. v1 implementation notes

Where the code differs in detail from the sections above (including the
review of `4b6d422`):

- **Attestation is never the poster's.** The daemon drops any posted
  `attestation`, and the API stores every v1 receipt as `self`. v1 has no
  daemon-verified evidence that a run happened as recorded, so a profile
  with `require.attestation: "cm-gate"` always computes INCOMPLETE ("v1
  cannot attest"). That requirement becomes satisfiable only when CM itself
  verifies run evidence (deferred).
- **Input verification** (EP, 2026-10-22). The reporting daemon drops any
  posted `verification`, `hashed_by`, `daemon_hash` and `resolved_path`.
  After `authorize_edit` (so an unauthorised caller learns nothing about
  the host's files), it derives each declared file, dir and artifact
  input's path itself: the declared `root_env`'s value in the calling
  session's own process environment, else the caller's registered
  worktree, joined with the declared relative `path`. The result must stay
  inside that root after canonicalisation (no `..`, no absolute path, no
  symlink escape); if it cannot be derived (unset root, no registered
  worktree, an escape) the input is `self_supplied_unverified` and
  INCOMPLETE. The daemon then rehashes it (`sha256:` for a file; for a dir `tree:` = sha256 of the
  `relpath\tsize\tsha256` lines, files of a directory before its
  subdirectories, symlinks skipped; cm-gate uses the same manifest). Only
  those inputs are `hashed_at_run`; a typed id that differs is kept as
  `poster_content_id`, and a path that is not there is `missing` (VOID)
  whatever id was typed. Hashing is capped per report at 64 GiB, 500 000
  files and 300 s (the RPC allows 900 s); hitting a cap leaves the input
  unverifiable, so INCOMPLETE. The receipt's `worktree.path` is likewise the
  caller's registered worktree, not the poster's. Everything else (gcs, db, inputs without a local
  path) is `self_supplied_unverified` and binds only when the profile's
  `accept_unverified` lists its kind; otherwise it is INCOMPLETE.
- **root_env + path.** The daemon's derivation (above) decides; cm-gate
  joins and resolves the same way for its own display and log. A missing
  sub-path under a derivable root is `missing` (VOID); an unset root is
  unverifiable (INCOMPLETE). The stored locator is the derived path, never
  `$ENV`.
- **Input keys.** `input_changed` and `input_void` are keyed across items
  only by a daemon-derived path. Every other input (a db query, a gcs URL, a
  poster's locator) is keyed per item (`item:<id>:<name>`), so a poster can
  never restale another item's receipts.
- **Granted nodes.** The profile's `nodes` are the grant and are never
  overridden. When a profile lists none, only a verified `selection` grant
  (a grantor-attached grant naming the nodes for that head) supplies the
  set; otherwise the step is INCOMPLETE ("no granted node set"). The
  runner's own collection (`granted_nodes`) is kept as evidence and never
  grants anything. Profiles for pytest classes therefore list their nodes,
  or rely on a selection grant per head.
- **Dirty tree.** Modified, staged and untracked files count. Gitignored
  files do not, by design (build outputs, caches, `.venv`), but they can
  still change a run (a local `.env`, generated fixtures): bind such files
  as declared inputs if the gate depends on them.
- **Wrappers.** Only bounded forms may precede the granted program:
  `timeout [-s SIG] [-k D] D`, `nice [-n N]`, `ionice -c N [-n N]`,
  `taskset [-c LIST | MASK]`, `numactl` with CPU/node/memory binding,
  `systemd-run --user --scope [-q] [-p MemoryMax|MemoryHigh|CPUQuota|…=V] [--]`,
  `env K=V` for `allowed_env` keys. A shell (`sh -c`, `bash -c`), any other
  tool or option, or a bad value makes the run weakened (RED without a
  grant).
- **Variance scope.** A variance grant accepts exactly the argv/env change
  whose digest it binds. Granted nodes that did not run still need a waiver
  naming those nodes.
- **Program.** The executed program must be the granted token itself, or
  the same basename resolving to the caller's worktree venv
  (`<worktree>/.venv/bin/<name>`, `venv/bin`) or to the registered runner's
  path in that worktree. Any other spelling (`/tmp/x/pytest`) is weakened.
- **The class is the item's.** A receipt naming another class is refused
  (`class_mismatch`). Changing an item's `gate_class`, the board's
  `gate_default_class`, or setting a profile as the default restales the
  receipts of the items that use it (`class_changed`).
- **Grant message checks.** The grantor's daemon reads the linked message by
  id through its store's event index and requires: the grantor as sender;
  the full 40-character head SHA as a token (and the base SHA for a
  base→head grant); the step as a token; and the kind (`waive…` or
  `variance`). Messages in the CM store are immutable (there are no edit or
  retraction events), so the create event is the latest version. A message
  not yet replicated to that host leaves the grant unverified, and
  `gate_receipts` rechecks it on read.
- **Refs and paths.** Base, runner and branch refs are validated (plain
  characters, no leading `-`, no `..`, `git check-ref-format`) by the API,
  cm-gate and the daemon, and every git call passes `--end-of-options`. The
  refresh runs git only in a registered CM worktree (a workspace, TUI
  session or recently exited session on that host) and skips anything else.
- **Bounded reads.** Receipt reads select only the latest row per gate in
  SQL; history is at most 50 rows without steps, inputs or closure. Board
  reads use a small stored `brief` (step name, state, rc, parsed counts) and
  never load steps or closures.
- **Closure.** The receipt stores the closure paths (up to 20 000). The
  refresh sends the base's changed paths and the API intersects them.
  Without a `closure_cmd`, cm-gate uses the tracked files under the steps'
  paths plus the files the head changed against the base.
- **pytest evidence** comes from CM's plugin `scripts/cm_gate_pytest.py`
  (`-p cm_gate_pytest`), not JUnit XML: exact node ids, each failure's
  exception type (forwarded from xdist workers), skip reasons, deselections
  and collection errors. The plugin writes its report to the path in
  `CM_GATE_REPORT` and removes that variable from its own environment, so a
  nested pytest the suite starts cannot overwrite the report. A suite that
  sets `CM_GATE_REPORT` itself, or another plugin that writes to that path,
  still could; the receipt is only as good as that file, which is one
  reason v1 receipts stay self-attested.
- **cm-gate output.** stdout and stderr are captured separately; a step's
  summary is stdout's tail (both streams for plain rc steps), so stderr
  noise cannot push pytest's result line out. Full output goes to the log.
- **Void flags.** `runner_void` counts VOID receipts on the board with the
  same runner SHA in 24 h; `input_void` counts receipts with the same input
  key missing. Both resolve on the item's next non-VOID receipt. Neither
  escalates to Owner; `receipt_stale` never reaches the orchestrator.
- **Timeouts.** `gate.report` allows 900 s, since the daemon rehashes large
  input directories before forwarding.
- **cm-gate exit status**: 0 PASS, 1 FAIL, 2 VOID, 3 INCOMPLETE, 4 STALE,
  5 runner error. Logs go to `~/.local/share/cm-gate/logs/` (or
  `CM_GATE_LOG_DIR`) and are pruned after 14 days.
- **Lane overrides** (`--override lane.toml|json`): `[steps.<name>]` with
  `prefix`, `extra_args`, `env`, `deselect = [{node, reason}]`, `skip` /
  `reason`. Each becomes part of the effective command and goes through the
  §5 diff.
