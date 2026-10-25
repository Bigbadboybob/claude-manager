# Shell panes and agent workers

Bash sessions are terminal panes. They remain in their workspace and keep
running as before; this change does not close, kill or restart them.

`list_sessions` and `list_sessions_grouped` omit shells by default. The optional
`include_shells=true` includes them as `kind: "shell"`, with `type: "bash"` and
`status: "shell"` (or `exited`). The operator daemon listing keeps terminal
discovery enabled, so the TUI can still restore and attach to shells. Session
control, scope authorization and raw shell command delivery are preserved.

`send_input` runs a shell command with a newline and returns `kind: "shell"`;
it does not arm an agent monitor. Redirect shell output to a file.
`wait_for_session_idle`, `start_session(type="bash", wait=true)`,
`monitor_sessions(until="turn_end")` and `cm-wait` wait for PTY quiet or exit,
without an assistant reply. Quiet output does not prove a command succeeded.
Agent transcript reads, reply waits, final monitors, completion reports, chat
identity, work-item holding and org adoption refuse shells explicitly.

The sidebar marks shells with `$`. Alt+i and the F6 info pane show terminal
identity and workspace facts. F6 hides shells by default; `b` reveals them in
their owner's workspace position, labelled shell, outside assistant counts (placement and counts are supplied by the F6 placement lane, #20).
Shell settings omit agent permissions and transcript actions. Alt+B marks any
legacy shell holder with `$`; compact agent board reads use `[shell]`. New shell assignments are refused.

## Surface inventory (2026-10-25)

| Surface | Prior behavior | Boundary |
| --- | --- | --- |
| Session lists and grouped lists | Shells mixed with workers | Default exclusion; explicit inclusion labels shells, including tombstones and legacy TUI rows |
| Session resolver | Bash mapped to the Claude transcript engine | Truthful `engine: bash`; daemon resolver also carries `kind: shell` |
| Board free capacity and untracked workers | Already excluded bash in SQL | Preserved; database regression checks both lists |
| Work-item holders | Shell names, aliases, UIDs and participant IDs accepted | Daemon resolution and API transaction refuse shells |
| Alt+B holder chips | Agent activity glyph for any holder | Legacy shell assignments carry a shell marker |
| Monitors and quiet waits | Shells accepted, waiting for idle/exit | Preserve PTY quiet waits, including `cm-wait`; label shell results and refuse only `until="final"` (including aliases) |
| `report_done` | Any registered session could report | Shell refusal without changing its session |
| `send_input` | Shell newline delivery already existed; MCP armed a worker monitor | Shell-labelled response; skip automatic worker monitoring |
| Transcript reads and reply waits | Empty output with a missing-transcript advisory | Refuse known shells before parsing, waiting or sending a command |
| `start_session(wait=true)` | Shell treated as a transcript-less worker | Wait for shell PTY quiet or exit, returning `kind: "shell"` and `last_message: null` |
| Staffing | Shell spawn included staffing metadata/accounting | No shell staffing note, version or worker-spawn accounting |
| Chat identity and directory (`who`) | Live shells represented as agents | Refuse shell callers, hide shell candidates, exclude new shell enrollment and host presence; refuse direct chat to known shells |
| Org lead and task views | Lead rule already filtered agent engines | Preserved; task peer SQL excludes shells and shell callers receive a refusal |
| Org roster proposals and TUI adopt rows | Lead/lane proposals and TUI rows already filtered agent engines; continuous proposals did not | Filter every proposal source, including bash continuous tasks; authoritative API/daemon adopt also refuse shells |
| Sidebar | Shell engine mark existed but activity cell looked like a worker | Shell `$` cell; exclude shells from worker activity rollup |
| F6 | Default hide and `b` toggle already existed | Shell label/info, outside assistant attribution/counts; workspace placement remains owned by the F6 placement lane |
| Alt+i | Engine shown, followed by the task's agent instructions | Shell identity and directory, without agent prompt/model/activity |
| Transcript picker, yank and model selection | Already special-cased bash | Preserved; shell settings skip agent permissions and transcript actions |
| Automatic task prompts and agent spawn queue | Already skipped bash | Preserved |
| Agent notifications and MCP injection | Already unavailable for bash | Preserved |

Tool schema changes are additive: only optional `include_shells` parameters
were added. `mcp_server/tests/test_tool_schema_compat.py` guards the snapshot.

Focused tests cover list exclusion/labelling, shell quiet waits, agent reply/final refusals,
completion and chat refusals, holder references, database adoption and holder
transaction rollback, free capacity, and shell UI labels/refusals. F6 ownership/count coverage lives in lane #20.
