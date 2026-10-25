# Staffing policy

Owner's engine guidance is one versioned policy for the organization served by
the planning API. `ping().staffing` returns its version, readable digest, rules,
default and preference. `staffing.get` exposes the same cached snapshot to the
TUI/operator. Staffing selection for spawn and ping never waits for the planning
API; existing launch work such as task creation and prompt/model lookup still
uses that API.

`start_session(label="investigate", work_kind="math")` selects the engine from
the policy. Hints are `math`, `design`, `architecture`, `metric`, `coding` and
`fanout`. Precedence is explicit `type` or `model`, strong preference, matching
work rule, lean preference, default. An unrecognized explicit model requires
an explicit type; a type/model mismatch retains the normal validation error.
The initial policy uses Codex for math/design/architecture/metrics, Claude for
coding/fan-out, and Codex as the default. No temporary preference is seeded.

An explicit choice is respected. A result includes `staffing_version` and, when
the choice opposes the policy or current preference, a one-line `staffing_note`
stating the precedence. Native forks retain the source engine and return a note
instead of switching engines. Shell sessions are outside the agent-engine
policy. The later `hire` tool can use the same policy resolver when S6 adds it.

**F9 → Staffing policy** edits the default, each rule, and an optional preference
with strength, reason and RFC3339 expiry. Tab or arrows select fields; left/right
cycles engines (rules can be unset), and text is typed into reason/expiry.
Enter saves centrally; Esc returns; Ctrl+r reloads and discards edits. A failed
save leaves the draft visible. Concurrent edits are refused with a version
conflict; reload before retrying. A-n, A-s and A-f preselect from the latest
cached policy when their forms open; later updates do not change a user's
selection in an already-open form.

Operators can also use `scripts/cm-op staffing.get '{"refresh":true}'`, followed
by `scripts/cm-op staffing.set` with a JSON object containing `expected_version`
and `policy`. The policy has `default_engine`, a `rules` object, and `preference`
(null, or `{engine,strength,reason,expires_at}`). Engines use `codex` or
`claude-code`; strength is `strong` or `lean`. Only authenticated Operators can
edit, including when an agent has global session permissions. The API trusts
the authenticated daemon's Owner stamp, as it does board actors.

Daemons fetch `/staffing` in the background every 30 seconds and persist the
last policy and recent spawners in `<CM root>/staffing.json`. An API outage keeps
that policy across restarts. With no cached policy, launches use today's Codex
default. Expiry is checked at selection time, so an offline cache cannot keep
an expired preference active. A new policy version sends one durable native
notice to live sessions that launched/forked anything within the last three
days. Notice IDs and delivery checkpoints prevent duplicate notices on retry.

The current planning deployment represents a single organization, stored under
the stable `default` org key, independent of projects/teams. Future explicit
org IDs can replace that key without changing launch selection. The database
migration seeds the initial policy once and never overwrites Owner edits.

Deployment requires the API migration/routes, both brains and the MCP payload,
plus the laptop TUI for the editor/defaults. Deploy the API before the consumers;
old daemons need explicit `type`. No holder upgrade or session respawn is needed
for daemon-side selection. The `orchestrate-swarm` skill's engine checklist must
point to `ping().staffing` when the coordinator deploys the updated guidance.
