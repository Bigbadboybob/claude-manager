# Questions and rulings on boards: design

**Status:** design revision 1 (2026-10-22, items lane), for EP's review. No
code yet; implementation waits for Swarm-Coord's go.

**Sources:**
- EP's round-2 answer 4 and round-3 answers
  (`predictionTrading-swarm-focused-design/cm-initiative/shared/ep-interviews.md`);
- items 5 (decision log) and 6 (Owner agenda) of
  `~/.local/share/swarm-coord/org-design/org-tools.md`;
- the published Owner-question norm (revision `b29674e1`, 2026-10-20);
- gate receipts, which already carry a ruling ID ([gate-receipts.md](gate-receipts.md) §8, §10).

## 0. The problem

Rulings live in chat. EP's IDs (`EP-A1021`, `EP-A1684`) and Owner's answers
("OWNER 10/20 ~15:25Z: proposed inputs APPROVED") are found by archaeology
through tens of thousands of messages. The same question gets asked again by
each lane, or relayed to Owner by someone who only saw it in a channel. A
gate started 12 s after a recompose ruling ran on the old one, because
nothing tied the run to the ruling in force.

A **question record** gives each question one ID that peers reuse instead of
re-asking, routes it to the one who decides, and keeps the answer, the
ruling ID and what it binds (items, SHAs, lanes) where every tool can read
it. Receipts reference ruling IDs and go STALE when a ruling is superseded.

## 1. What this does not change

- **The Owner-question norm stays as published.** Any agent may ask Owner
  whatever its own work needs, directly, as today. A record is never a
  required step and never a gate. What changes is the follow-up: someone who
  only *saw* a question doesn't relay it, they follow the record instead.
- **EP is not a mandatory router.** A question names its own decision owner.
  EP answers what is EP's to answer (law, waivers, deploy calls, per EP round
  2); Owner answers Owner's; a lane lead answers its lane's.
- Chat stays the place for discussion. The record holds the question, the
  answer and its bindings, and links to the messages.

## 2. Data

One record per question, on a board (rulings bind that board's items; the
trader org's is `sejd`).

| Field | Notes |
|---|---|
| `id` | `Q-<board>-<seq>`, minted by CM, quotable in chat |
| `question` | text ≤ 2000 |
| `asker` | pid, stamped by the daemon |
| `owner` | decision owner pid (`owner` for Owner); required; set by the asker, re-routable by the owner or the board orchestrator |
| `status` | `open` → `answered` → `superseded`; or `withdrawn` (asker or owner, with a reason) |
| `answer` | text ≤ 4000, by the decision owner (or Owner) only |
| `ruling_id` | the answer's ID: the owner's own label (`EP-A1021`) or minted `R-<board>-<seq>`; unique per board |
| `answered_at`, `answered_by` | CM's clock; the actor, never a claim |
| `binds` | `{items: [n], shas: [40-hex], groups: [str], gates: [str]}`: what the ruling governs |
| `supersedes` / `superseded_by` | ruling IDs; a chain, never a tree (one successor each) |
| `kind` | `ruling` (default); later `waiver` / `variance` / `selection` (§6) |
| `links` | `{asked_in, answered_in}` message IDs |
| `followers` | pids that get a push on answer, supersede or withdraw |
| `created_at` | |

Every change is an `item_events` row on each bound item (`question_asked`,
`ruling_answered`, `ruling_superseded`), so a board read reflects it.

## 3. Lifecycle

1. **Ask.** `question(text, owner, binds?)`. Before creating, CM looks for an
   open or answered record on the same board with an overlapping binding and
   similar text (normalized-token overlap; threshold in §8 Q4). If it finds
   any, it returns them as `existing` and creates nothing unless the caller
   passes `new=True`. That is the "reuse the ID instead of re-asking" path,
   and also the dedupe of Owner's agenda (org-tools §6).
2. **Follow.** `follow(id)` adds the caller to `followers`. A peer that only
   saw the question follows it; it never re-relays it.
3. **Answer.** `answer(id, text, ruling_id?, supersedes?)`, decision owner or
   Owner only (403 otherwise). It sets `answered`, mints or records the ruling
   ID, and pushes the asker and followers.
4. **Supersede.** An answer with `supersedes=<ruling id>` marks the older
   record `superseded`, links both ways, and restales receipts bound to the
   old ruling (§4). Only the old ruling's owner, Owner, or someone the old
   owner names can supersede it.
5. **Withdraw.** The asker or owner closes an open question that no longer
   matters, with a reason.

The decision owner gets one push when a question is routed to them and a
digest line while it stays open. An open question routed to Owner is shown
in an Owner agenda view (`questions(owner="owner", status="open")`), deduped
by binding and topic, so Owner gets one list instead of digest fragments.

## 4. Receipts and rulings

Today a receipt's `ruling_id` is free text, compared only with the item's
`gate_binding.ruling_id` set by `item_set(n, ruling=…)`
([gate-receipts.md](gate-receipts.md) §10). With records:

- **Current ruling of an item** = the newest `answered`, non-superseded
  ruling whose `binds` names the item, its group, or (for gates) its gate.
  `item_set(n, ruling=…)` remains as an explicit pin, and must name an
  answered ruling on the board.
- **Stale on supersede.** Superseding a ruling restales every live receipt
  that cites it, with `ruling_superseded <old> → <new>`, through the same
  one-way staleness path. Nobody has to remember an `item_set`.
- **Unknown or open ruling.** A receipt citing a ruling ID that is not on
  the board, or whose question is still open, computes INCOMPLETE ("ruling
  not answered").
- **Stale on arrival** keeps working: a receipt posted against a ruling the
  item has already moved past is STALE at once.
- **SHA bindings.** A ruling binding SHAs ("MF is now c3d4") can move the
  item's MF/head binding when answered, which restales the old receipts
  through the existing binding path.

## 5. Tools

MCP (daemon-routed, actor stamped by the daemon):

```
question(text, owner, items=None, shas=None, groups=None, gates=None,
         board=None, new=False) -> {question: {id, status}, existing?: [...]}
answer(id, text, ruling_id=None, supersedes=None, board=None) -> {question}
follow(id, board=None) / withdraw(id, reason, board=None)
questions(board=None, status=None, owner=None, item=None, query=None)
  -> [{id, question, owner, status, ruling_id, answer?, binds}]
```

- `board()` and `item(...)` show the rulings governing an item; Alt+B's
  detail pane gets a **Rulings** section and the banner `N open questions
  for you`.
- `gate_receipts(n)` names the item's current ruling next to the receipt's.

Storage (migration `024`, number claimed at implementation): `item_rulings`
(the record), `ruling_binds` (`ruling → item | sha | group | gate`), and
`ruling_followers`, in the items style (rerunnable DDL, board-row lock,
history in `item_events`).

## 6. Grants as rulings (later)

Gate waivers, variances and node selections are rulings with a precise
scope. Today they are `item_grants`, linked to a chat message the grantor's
daemon checks ([gate-receipts.md](gate-receipts.md) §4). Once records exist:

- `gate_waive` creates a record of kind `waiver` / `variance` / `selection`,
  answered at once by the grantor (the actor *is* the grantor, so no message
  parsing is needed), binding the head SHA (and base), the step and nodes.
- The verification rules stay the same: exact head, exact step, reason and
  risk, grantor authority.
- Superseding or withdrawing such a ruling restales receipts that relied on
  it, as revoking a grant does today.
- Existing `item_grants` rows stay valid; new grants are written as rulings.

## 7. Suggested v1 cut

Records with ask / follow / answer / supersede / withdraw; the dedupe check
on ask; bindings to items and gates; the item's current ruling feeding
receipt staleness; MCP tools; the Alt+B Rulings section and the open-for-you
count. Deferred: SHA bindings moving MF/head; grants as rulings (§6); the
digest line; similarity beyond token overlap.

## 8. Questions for EP

1. **Ruling IDs.** Keep your existing labels (`EP-A1021`) as the ruling ID
   when you give one, with CM minting `R-<board>-<n>` otherwise? Or should
   CM mint all of them?
2. **Scope.** Board-level (the `sejd` board), or should a ruling be able to
   bind several boards (org-wide law)?
3. **Who supersedes.** Only the ruling's owner and Owner, or also a deputy
   you name per ruling or per class?
4. **Dedupe.** On ask, should overlapping bindings plus similar wording
   return the existing record and refuse to create a new one (the caller
   must pass `new=True`), or only warn?
5. **Grants.** Move gate grants onto ruling records in the same batch as the
   records, or after the records have run on `sejd` for a while?
