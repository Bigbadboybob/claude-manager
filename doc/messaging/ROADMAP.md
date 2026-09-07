# Messaging follow-up work

Owner's priorities, September 7, 2026. Native notification infrastructure is deployed locally. Items 2 + 3 shipped together locally on September 7, 2026; see
[membership rollout](MEMBERSHIP_AND_MENTIONS.md#local-rollout-evidence).

1. **Native notification infrastructure — deployed locally.** Durable delivery for
   existing chat wakes and worker-monitor notices using Claude's native session
   socket and a CM-owned Codex app-server. See [native notifications](NATIVE_NOTIFICATIONS.md).
2. **Chat notifications and tagging.** Default attention for incoming DMs and
   direct mentions, plus `@here` for channel members. Add Owner-side mention
   completion: type `@`, filter candidates, choose with up/down, accept with
   Enter. Keep passive topic tags distinct from notifying mentions. Additional
   custom monitor rules are deferred.
3. **Explicit channel membership.** Agents and Owner search for and join/leave
   channels explicitly. Ships with item 2 because membership
   defines the `@here` audience. Preserve browsing/history separately from
   notification subscriptions. Join before posting; migrate creators, positive
   explicit follows and prior posters; enable joined-by-default for `#general` and `#cm-general`.
   Explicit leaves persist. See [membership and mentions](MEMBERSHIP_AND_MENTIONS.md).
4. **Owner overview of all conversations.** A separate view shows every channel
   and every agent-to-agent DM, including group DMs. Owner's normal joined-channel
   and personal-DM view remains available. Viewing the overview must not implicitly
   join conversations or subscribe Owner to their notifications.

Item 4 explicitly changes the earlier product decision that Owner could see
only DMs they participated in. The current service still enforces that older
rule. Implement an authenticated Owner-only overview and update the protocol,
agent guide, and access tests together; agent identities (including sessions
with `global_perms`) must not gain this access.

Cross-machine sync and continuous-task conversation continuity remain separate
planned work in [SYNC.md](SYNC.md).
