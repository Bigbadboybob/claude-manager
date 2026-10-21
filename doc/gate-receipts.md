# Gate receipts on board items: design

Status: **design for review, no code yet** (2026-10-21, items lane). Implementation
waits for Owner's go after execution-policy (EP) has checked it against its cases.

Source: EP's round-2 interview answers
(`predictionTrading-swarm-focused-design/cm-initiative/shared/ep-interviews.md`,
2026-10-21), Swarm-Gardener's summary in
`#initiative/swarm-focused-design/claude-manager` at 14:03Z, and item 2 of
`~/.local/share/swarm-coord/org-design/org-tools.md`. It builds on the items
board contract in [items-board.md](items-board.md).

## 0. The problem

Today a gate result is a long prose post such as "GREEN: MF a1b2 fails, pytest
412 passed, mypy clean". The reader has to work out three things from posts
that are mixed in with everything else:

- Is this result for the current head? EP has ruled on superseded MF SHAs.
- Did every step that matters actually run? In EP's cases, `device_budget` run 2
  had FETCH_FAIL steps after a pytest pass, and the a320 nodes "passed" by
  skipping on a host without the harvest.
- Was it run under the ruling now in force? Integration started a gate 12 s
  after EP posted a recompose ruling it hadn't seen.

A receipt is a structured record attached to the item. **CM** computes its
verdict from the recorded steps and the board's declared requirements. The
poster's word is not enough. CM also marks the receipt STALE, with a reason,
as soon as any identity it was bound to changes.

## 1. Data

### 1.1 Receipt

One row per gate run, attached to `(board, item n)`. Receipts are append-only
and never edited. A rerun writes a new receipt, and the old one stays in
history.

| Field | Type | Notes |
|---|---|---|
| `id` | `r-<board>-<n>-<seq>` | stable, quotable in chat (`r-sejd-14-3`) |
| `gate` | str ≤ 40 | default `gate`; an item can carry several gates (`gate`, `integration`, `bench`), each with its own latest receipt |
| `class` | str ≤ 40 | which requirement profile applies (§2); default: the item's `gate_class`, else the board default |
| `profile_version` | int | the profile version used at post time |
| `repo` | str | repo identity (remote URL, normalized) |
| `branch` | str? | the head branch, used by the refresh in §4 |
| `head_sha` | 40-hex | **required** |
| `mf_sha` | 40-hex? | must-fail commit; required when the profile has a `must_fail` step |
| `runner` | `{ref, sha}`? | the driver/runner script: `ref` like `origin/main:scripts/gate.sh`, `sha` = the blob or commit it resolved to |
| `ruling_id` | str? ≤ 80 | the lane ruling the run was made under (`EP-A1021`); see §6 |
| `base` | object | `{ref, sha_before, sha_after, merge_tree_before, merge_tree_after}`: base ref and SHA, plus the `git merge-tree --write-tree <base> <head>` tree OID, captured before the first step and after the last. A conflict is recorded as `merge_tree: "conflict"` |
| `worktree` | `{head_before, head_after, dirty_before, dirty_after}` | identity of the checkout the steps ran in |
| `steps` | `[step]` ≤ 40 | below. **Every** step that was planned is listed, including those that failed, were skipped or never started |
| `waivers` | `[str]` ≤ 20 | waiver ids invoked (`EP-W0310`), each naming the step it covers: `{id, step}` |
| `artifacts` | `[{path, bytes, sha256?}]` ≤ 20 | optional; covers cases like the 12.98 GB vs 6.55 GB staging mix-up |
| `log_path` | str? | full log on the posting host |
| `attestation` | `cm-gate` \| `self` | `cm-gate` when CM's runner executed the commands (§5); `self` when an agent typed the receipt |
| `poster` | actor | stamped by the daemon, as for every item write |
| `posted_at` | ts | API server clock |
| `computed` | verdict | the verdict CM computed at post time (§3), kept for history |

### 1.2 Step

| Field | Type | Notes |
|---|---|---|
| `name` | str ≤ 40 | matches a profile step (`must_fail`, `pytest`, `mypy`, `ruff`, `fetch`, …); unknown names are kept and shown, and count only if the profile names them |
| `cmd` | str ≤ 2000 | the exact command line, after expansion |
| `cwd` | str? | |
| `at_sha` | 40-hex? | the commit the step ran on; defaults to `head_sha`. The `must_fail` step runs at `mf_sha` |
| `state` | `ran` \| `skipped` \| `not_run` \| `error` | `error` means the step never produced a result because of infra trouble (fetch failure, timeout, OOM), which EP calls VOID |
| `rc` | int? | the step's own exit code; null unless `ran` |
| `started_at`, `ended_at` | ts? | |
| `summary` | str ≤ 2000 | the tool's own summary lines, verbatim (pytest's final line plus `-rs` skip reasons, mypy's `Success:`/`Found N errors`, ruff's last line) |
| `parsed` | object | filled in by **CM** from `summary`, never by the poster: pytest `{passed, failed, errors, skipped, xfailed, xpassed, deselected, skips: [{reason, count, nodes?}]}`, mypy `{ok: bool, errors: N, files: M}`, ruff `{violations: N}` |
| `reason` | str? ≤ 300 | why it was skipped, not run or errored |

CM parses the summary text itself and checks it against `rc`. If they disagree
(for example `rc=0` while pytest reports failures, or `rc=1` with "Success: no
issues"), the step is marked `inconsistent` and does not satisfy its
requirement. If a summary can't be parsed for a step type the profile needs
parsed, the step counts as **unparsed**, which is never a pass.

## 2. Requirements are declared, per board and per class

A **gate profile** belongs to the board. It is keyed by `class`, and the board
names a default class. Items take `gate_class` (set with
`item_set(n, gate_class="py-lane")`). A profile is the list of steps a valid
run must contain:

```json
{"class": "py-lane", "version": 4,
 "steps": [
   {"name": "must_fail", "kind": "pytest", "expect": "fail", "at": "mf",
    "required": true},
   {"name": "pytest", "kind": "pytest", "required": true, "min_passed": 1,
    "max_failed": 0, "must_run": ["tests/a320/test_nodes.py"],
    "skip_policy": {"allow_reasons": ["^requires CUDA$"], "otherwise": "incomplete"}},
   {"name": "mypy", "kind": "mypy", "required": true, "max_errors": 0},
   {"name": "ruff", "kind": "ruff", "required": true, "max_violations": 0},
   {"name": "fetch", "kind": "rc", "required": true}
 ],
 "waivable": ["mypy", "ruff"],
 "require": {"mf_sha": true, "runner": true, "ruling_id": false},
 "base": {"ref": "origin/main", "scope": "tree"}}
```

- `kind` picks the parser and its pass test: `pytest`, `mypy`, `ruff`, or `rc`
  (a plain exit code).
- `expect: "fail"` turns a step around. The must-fail step passes only if it
  **ran** at `mf_sha` and failed with test failures. Collection errors and
  import errors don't count, so a broken tree can't pass as a red test.
- `must_run` lists pytest node ids (or path prefixes) that must appear as run.
  If one of them is skipped, deselected or absent, the step is
  **incomplete**. This catches the a320 missing-harvest skip even when
  `min_passed` is met by other tests.
- `skip_policy` sets what a skip means: `allow_reasons` regexes, with
  `otherwise` = `incomplete` (the default) or `fail`. A skip with no reason is
  never allowed.
- `waivable` names the steps a waiver may cover. A waiver for any other step is
  refused when posted.

Profiles are stored with a version. A change is a board event, made through
`PATCH /boards/{ref}` with `gate_profiles`. Allowed: Owner, the board's
orchestrator, the steward, and the pids in a new board setting `gate_admins`
(intended for EP), otherwise `403`. Receipts keep their raw steps, so CM
**re-evaluates** the latest receipt under the current profile version. If a
tightened profile turns a PASS into something else, the item shows the reason
`profile py-lane v3 → v4`. `computed` keeps what the verdict was at post time.

A board with no profile has nothing declared, so CM can't call any receipt
PASS. Its receipts compute INCOMPLETE ("no gate profile for class X"). This is
deliberate: no requirements cannot mean everything passed.

## 3. Verdict

CM computes the verdict, in this order, on post and on every re-evaluation:

1. **STALE** if any binding in §4 has changed. This rule overrides the others.
   The verdict CM computed before the change is still shown, e.g.
   `STALE (was PASS): head moved a1b2c3d → 9f8e7d6`.
2. **FAIL** if any required step **ran** and missed its pass test: rc,
   failures, mypy errors, ruff violations, a must-fail step that passed, or
   `inconsistent`. Waived steps are exempt (rule 5). FAIL is EP's RED.
3. **INCOMPLETE** if any required step is missing from `steps`, `skipped`,
   `not_run`, `error` (EP's VOID, e.g. FETCH_FAIL after a pytest pass), or
   `unparsed`. The same applies to a vacuous pytest result (`min_passed` not
   met, or a `must_run` node not run), a skip that `skip_policy` doesn't
   allow, or a missing required identity (`mf_sha`, `runner`). It also
   applies when `worktree.head_before ≠ head_after`, when either side is
   dirty, or when `base.merge_tree_before ≠ merge_tree_after`: the code
   moved during the run, so the run tested nothing definite.
4. **PASS** only if every required step is present, `ran`, and met its pass
   test, or is covered by a waiver.
5. **Waivers.** A waived required step counts as satisfied, and the verdict
   names the waiver: `PASS · 1 waiver EP-W0310 (mypy)`. Until ruling records
   exist (§6), CM can't check that a waiver id exists, so it is shown as
   `waiver unverified`. Each waiver must name a step from the profile's
   `waivable` list.

FAIL ranks above INCOMPLETE, so one red required step shows as FAIL even when
other required steps are missing. Each verdict lists all of its reasons, for
example `INCOMPLETE: fetch error (FETCH_FAIL), a320 nodes skipped "harvest
missing"`. Non-required steps are shown but never change the verdict.

## 4. Staleness

A receipt is bound to identities. When one changes, the receipt goes STALE
automatically and records the reason, the old value and the new value:

| Binding | Detected when | Reason |
|---|---|---|
| head SHA | a newer receipt for the same gate names another head, the item's head binding is set (`item_set(n, head=…)`), or the refresh sees `branch` move past `head_sha` | `head_moved` |
| MF SHA | a newer receipt or `item_set(n, mf=…)` names another MF | `mf_moved` |
| runner SHA | the refresh resolves `runner.ref` to another SHA | `runner_changed` |
| ruling ID | the item's `ruling_id` (or, later, the lane's current ruling, §6) differs from the receipt's | `ruling_superseded` |
| base / merge tree | the refresh finds `git merge-tree --write-tree <base ref now> <head>` ≠ `merge_tree_after` | `base_moved` (with both base SHAs) |
| profile | the profile version changed and the re-evaluated verdict differs | `profile_changed` |

- **Stale on arrival.** A receipt posted against a ruling, head or MF that the
  item has already replaced is STALE at once. This is the compare-and-set that
  catches "started g74 12 s after the recompose ruling".
- **Base scope.** `scope: "tree"` (default) restales on any base move that
  changes the merge tree. A base commit that head already contains, or that
  merges to an identical tree, doesn't restale. `scope: "paths"` restales only
  when the base change touches `watch_paths` (the gate's closure). That is
  cheaper for lanes on a busy main, but it is a judgment call for EP (§8 Q1).
- **Refresh.** The API server has no checkouts, so the **posting host's
  daemon** checks the moving bindings. For each repo it runs one `git fetch` of
  the base ref and runner ref every 5 minutes, covering every latest,
  non-stale receipt on an open item. It then posts the resolved identities to
  the API, which decides staleness. Other hosts with the same repo can take
  over if the posting host stops. If no host has checked for 15 minutes, the
  receipt keeps its verdict but shows `base unchecked 40m`. CM doesn't call it
  STALE without evidence, and doesn't hide the gap either.
- **One-way.** A stale receipt never becomes valid again. A rerun posts a new
  receipt. This keeps "it was green once" out of the current view.
- STALE is an `item_events` row (`receipt_stale`, with reason, prev and new),
  so it moves the board version. Alt+B and `board()` pick it up on the next poll.

## 5. Tools

**MCP** (daemon-routed like the other item tools; the daemon stamps `actor`):

```
gate_report(n: int, steps: list[dict], head: str, mf: str | None = None,
            runner: dict | None = None, ruling: str | None = None,
            base: dict | None = None, worktree: dict | None = None,
            waivers: list[dict] | None = None, gate: str = "gate",
            klass: str | None = None, artifacts: list[dict] | None = None,
            log_path: str | None = None, board: str | None = None,
            request_id: str | None = None)
  -> {receipt: {id, verdict, reasons, stale?}, item}

gate_receipts(n: int, gate: str | None = None, history: bool = False,
              board: str | None = None)
  -> {latest: {<gate>: receipt}, history?: [receipt]}

gate_profile(klass: str, steps: list[dict] | None = None, waivable=None,
             require=None, base=None, default: bool = False, board=None)
  -> {profile}            # read when steps is None; write needs gate_admins
```

- `item_set` gains `head`, `mf`, `ruling` and `gate_class`. Setting `head`,
  `mf` or `ruling` is how a lane or EP moves a binding without a rerun, and it
  restales the current receipts.
- `item(...)` and `board(view="slim")` render the latest receipt per gate on
  the item line: `#14 active "fuse SEJD" … ✓PASS 9f8e7d6 r-sejd-14-3` or
  `✗STALE head_moved (was PASS)`. `view="full"` and `gate_receipts` give the
  steps.
- `request_id` makes a retried `gate_report` return the first receipt.

**`scripts/cm-gate`**, the recommended way to post. It reads the item's
profile, runs each declared command at the right SHA, and captures what the
receipt needs:

- the rc, the timestamps and the tail of each step's output;
- `merge-tree` and worktree identity before and after the run;
- the runner SHA.

It posts with `attestation: "cm-gate"`. Typed receipts are allowed but shown as
`self`, and the verdict rules are identical: a typed receipt can't skip a
required step. cm-gate lists every planned step, so a step that never started
is recorded as `not_run` rather than omitted. Commands come from the profile
(`steps[].cmd`, templated with `{head}`, `{mf}`, `{base}`) or from a lane's
local override file, which is then recorded verbatim.

**Planning API** (`api/items.py`):

| Method + path | Body | Returns |
|---|---|---|
| `POST /boards/{ref}/items/{n}/receipts` | `{actor, receipt…, request_id?}` | `{receipt, item}`; 422 `invalid_field` / `waiver_not_waivable` |
| `GET /boards/{ref}/items/{n}/receipts` | `?gate=&history=0` | `{latest, history}` |
| `POST /receipts/refresh` | `{daemon_id, observations: [{repo, ref, sha}], trees: [{receipt_id, base_sha, merge_tree}]}` | `{stale: [receipt_id], watch: [{repo, base_ref, head, runner_ref, receipt_id}]}`; the reply tells the daemon what to watch next, so the daemon keeps no state across brain restarts |
| `PATCH /boards/{ref}` | `{gate_profiles?, gate_default_class?, gate_admins?}` | board header |

**TUI (Alt+B).** The item row gets a gate cell after the status badge:
`✓PASS` green, `✗FAIL` red, `◐INC` amber, `⟲STALE` dim with a strikethrough
SHA. The detail pane gets a **Gate** section with one block per gate:

- verdict and reasons;
- the bound SHAs, short form, with the stale reason highlighted;
- one line per step (`✓ pytest 412 passed · 3 skipped (requires CUDA ×3)`,
  `✗ fetch error FETCH_FAIL`, `· mypy not run`);
- the waivers and the receipt id.

The banner adds `N stale receipts` when there are any.

**Flags.** A new kind, `receipt_stale`, goes to the item's holders only. It is
raised once per stale transition, using the `flag_pushed` dedupe, and is never
escalated to the orchestrator. It resolves when a new receipt for that gate is
posted or the item closes. INCOMPLETE and FAIL don't flag, since the holder
already knows. They show up in the board read.

## 6. Storage (migration `023_item_receipts.sql`, number claimed at implementation)

Uses the items style: rerunnable DDL only, no row UPDATEs, writes inside the
board-row lock, every change an `item_events` row.

```sql
CREATE TABLE IF NOT EXISTS item_receipts (
    id            BIGSERIAL PRIMARY KEY,
    board_id      UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    item_id       BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,          -- per item, for r-<board>-<n>-<seq>
    gate          TEXT NOT NULL DEFAULT 'gate',
    class         TEXT,
    profile_version INTEGER,
    head_sha      TEXT NOT NULL,
    mf_sha        TEXT,
    runner        JSONB,
    ruling_id     TEXT,
    base          JSONB NOT NULL DEFAULT '{}'::jsonb,
    worktree      JSONB NOT NULL DEFAULT '{}'::jsonb,
    steps         JSONB NOT NULL,            -- incl. CM-parsed fields
    waivers       JSONB NOT NULL DEFAULT '[]'::jsonb,
    artifacts     JSONB NOT NULL DEFAULT '[]'::jsonb,
    repo          TEXT, branch TEXT, log_path TEXT,
    attestation   TEXT NOT NULL,
    computed      JSONB NOT NULL,            -- {verdict, reasons} at post time
    stale_at      TIMESTAMPTZ,
    stale_reason  JSONB,                     -- {reason, prev, new}
    checked_at    TIMESTAMPTZ,               -- last refresh observation
    poster_pid    TEXT NOT NULL,
    request_id    TEXT,
    posted_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT item_receipts_seq_key UNIQUE (item_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_item_receipts_latest
    ON item_receipts (item_id, gate, id DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_item_receipts_request
    ON item_receipts (board_id, request_id) WHERE request_id IS NOT NULL;

ALTER TABLE items  ADD COLUMN IF NOT EXISTS gate_class TEXT;
ALTER TABLE items  ADD COLUMN IF NOT EXISTS gate_binding JSONB NOT NULL DEFAULT '{}'::jsonb;
    -- {head, mf, ruling_id}: what the item says is current
ALTER TABLE boards ADD COLUMN IF NOT EXISTS gate_profiles JSONB NOT NULL DEFAULT '{}'::jsonb;
    -- {class: {version, steps, waivable, require, base}}
ALTER TABLE boards ADD COLUMN IF NOT EXISTS gate_default_class TEXT;
ALTER TABLE boards ADD COLUMN IF NOT EXISTS gate_admins TEXT[] NOT NULL DEFAULT '{}';
```

The verdict logic is a pure function in `dispatch/items_rules.py`,
`evaluate(receipt, profile, binding) -> {verdict, reasons}`, alongside the
other BoardTx rules, so it can be tested without a database. The parsers are
in `dispatch/gate_parse.py`.

Size limits: a receipt is at most 64 KiB serialized. Summaries are tails, and
full logs stay at `log_path`. A receipt in a closed item's history is kept
with the item, and archiving the item archives its receipts.

## 7. Later: question/ruling records (not this batch)

EP's companion ask is a shared record with these fields:

- the question;
- the asker;
- the decision owner it's routed to;
- status (`open` / `answered` / `superseded`);
- the answer;
- the ruling ID and timestamp;
- the SHAs and items it binds.

How receipts would connect to it:

- **Until the record exists**, `ruling_id` is free text, checked only for
  equality against the item's `gate_binding.ruling_id`. EP or the lane sets
  that with `item_set(n, ruling="EP-A1021")`.
- **Once the record exists**, the ruling ID is its key. The current ruling for
  an item is the newest `answered` ruling that binds the item (or its group or
  lane) and hasn't been superseded. Answering a superseding ruling then
  restales every receipt bound to the old one, with no `item_set` step. A
  receipt naming an unknown or `open` ruling computes INCOMPLETE
  (`ruling not answered`).
- **Waivers become rulings of type `waiver`**, each bound to a step name and
  SHAs. `waiver unverified` disappears. A waiver bound to SHAs other than the
  receipt's head doesn't apply, and a superseded waiver restales the receipt.
- **The ruling record can also hold rulings that bind SHAs**, such as "MF is
  now c3d4". Answering one moves `gate_binding.mf` through the same path.

## 8. Questions for EP

1. **Base scope.** Should any merge-tree change restale (`tree`), or only
   base changes inside the gate's closure (`paths`)? Which one should be the
   default for sejd lanes?
2. **Infra errors (VOID).** Here they compute INCOMPLETE, not FAIL. Should a
   VOID ever count as RED?
3. **Must-fail.** The design requires real test failures at `mf_sha`, with
   collection and import errors rejected. Is that EP's MF semantics, or can MF
   legitimately be a collection failure?
4. **Who edits profiles.** Owner, the orchestrator, the steward and
   `gate_admins` (EP). Should it be narrower?
5. **Self-attested receipts.** Should a board be able to require
   `attestation: cm-gate` for PASS?

## 9. Checked against EP's cases

| EP case | Outcome under this design |
|---|---|
| Ruling on a stale MF SHA | A newer receipt or `item_set(mf=…)` restales the old one, shown as `STALE mf_moved a1b2 → c3d4`. A receipt posted against the old MF after the change is stale on arrival |
| RED/VOID reruns | Each rerun is a new receipt, and the latest one per gate is shown. History keeps RED (FAIL) and VOID (INCOMPLETE `error`) runs with their reasons, so "3 reruns" can be counted, not reconstructed |
| FETCH_FAIL after a pytest pass | The fetch step is required and its state is `error`, so the verdict is INCOMPLETE `fetch error (FETCH_FAIL)`. The pytest pass can't lift it to PASS |
| Vacuous pass from missing-harvest skips | `must_run` lists the a320 nodes. They show as skipped (`harvest missing`), and that reason isn't in `allow_reasons`, so the verdict is INCOMPLETE `a320 nodes skipped`. Without `must_run`, the reasonless or non-allowed skip still blocks PASS |
| Gate started just after a new ruling | The receipt's `ruling_id` ≠ the item's current one, so it is STALE on arrival with `ruling_superseded` |
