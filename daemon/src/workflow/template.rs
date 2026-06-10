//! Prompt templating for workflow activation prompts.
//!
//! Template substitutions (all optional — missing keys expand to empty string):
//!
//! ```text
//!   {{ roles.<role>.user[N] }}           Nth post-launch user-typed message
//!   {{ roles.<role>.assistant[N] }}      Nth post-launch assistant message
//!   {{ roles.<role>.prior_user[N] }}     Nth user message from BEFORE launch
//!   {{ roles.<role>.prior_assistant[N] }} Nth assistant message from BEFORE launch
//!   {{ roles.<role>.last_message }}      alias for `assistant[-1]`
//!   {{ roles.<role>.this_turn }}         every assistant text message the role
//!                                        produced since its most recent activation,
//!                                        joined with `\n\n---\n\n`. Useful when
//!                                        the role tends to speak in multiple turns
//!                                        per round and `last_message` would drop
//!                                        all but the final one.
//!   {{ roles.<role>.initial_prompt }}    alias for `user[0]`
//!   {{ roles.<role>.plan }}              the markdown plan from the most recent
//!                                        ExitPlanMode tool call in that role's
//!                                        Claude transcript (empty if never used)
//!   {{ goal }}                           run-level goal set at launch (falls back
//!                                        to worker.initial_prompt when empty)
//!   {{ rejected_findings }}              header + bulleted list of findings the
//!                                        manager has rejected this run via
//!                                        `workflow_reject_finding`. Empty string
//!                                        when the stash is empty so first-round
//!                                        prompts stay clean.
//! ```
//!
//! Negative indices count from the end of the respective slice (e.g.
//! `prior_assistant[-1]` is the last assistant turn before the workflow launched).
//! Useful when the user ran planning mode or had a long conversation with the
//! worker before kicking the workflow off — the meaningful context lives in the
//! pre-launch history.
//!
//! The template engine is deliberately small: no conditionals, loops, or filters.
//! It calls a `RoleResolver` which the caller implements to fetch messages for a
//! role (typically by reading that role's Claude/Codex JSONL transcript).

/// How the template engine asks about a role's messages.
///
/// Implementors read the role's transcript and return the requested slice.
/// Returning `None` for any accessor expands to empty string in the template.
pub trait RoleResolver {
    /// Post-launch user-typed turns for the role, in order. Used for `user[N]`
    /// and the `initial_prompt` alias.
    fn user_messages(&self, role: &str) -> Vec<String>;
    /// Post-launch assistant turns for the role, in order. Used for
    /// `assistant[N]` and the `last_message` alias.
    fn assistant_messages(&self, role: &str) -> Vec<String>;
    /// Assistant turns the role produced since its most recent activation in
    /// the run's history — i.e. everything it has said this round. Used for
    /// `this_turn`. Default impl returns the same as `assistant_messages` so
    /// the trait stays backward-compatible with simpler resolvers that don't
    /// track activation history.
    fn assistant_since_activation(&self, role: &str) -> Vec<String> {
        self.assistant_messages(role)
    }
    /// User turns from *before* the workflow launched (pre-baseline). Used for
    /// `prior_user[N]`.
    fn prior_user_messages(&self, role: &str) -> Vec<String>;
    /// Assistant turns from *before* the workflow launched. Used for
    /// `prior_assistant[N]`.
    fn prior_assistant_messages(&self, role: &str) -> Vec<String>;
    /// The most recent `ExitPlanMode` plan text, if any. Claude-only; returns
    /// `None` for Codex sessions. Used for the `plan` accessor.
    fn latest_plan(&self, role: &str) -> Option<String>;
    /// Run-level goal string set by the user at launch. Used by the bare
    /// `{{ goal }}` template var. Return `None` to fall back to the worker's
    /// `initial_prompt`.
    fn goal(&self) -> Option<String>;
    /// Free-text findings the manager has dismissed via
    /// `workflow_reject_finding`. Surfaced via the bare
    /// `{{ rejected_findings }}` var; empty vec renders to empty string so
    /// first-round prompts stay clean. Default impl returns empty so simpler
    /// resolvers don't need to know about the stash.
    fn rejected_findings(&self) -> Vec<String> {
        Vec::new()
    }
}

pub fn render<R: RoleResolver + ?Sized>(template: &str, resolver: &R) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    // Start of the current run of literal (non-placeholder) bytes. We flush
    // each run verbatim as a UTF-8 `&str` slice so multi-byte characters
    // survive intact. The previous implementation pushed `bytes[i] as char`
    // per byte, which Latin-1-expands every UTF-8 continuation byte into its
    // own code point: a smart quote `“` (E2 80 9C) became `â` + U+0080 +
    // U+009C, and an em-dash `—` (E2 80 94) became `â` + U+0080 + U+0094.
    // That produced the visible `â` mojibake AND silent text loss — the
    // C1 control characters (U+0080..U+009F) it synthesized include
    // control-string introducers (OSC/APC/PM/SOS) that the worker's
    // terminal swallows up to the next String Terminator, eating whole spans
    // of a manager→worker directive. Slicing only ever happens at `{`/`}`
    // (ASCII) boundaries, so every slice below is a valid char boundary.
    let mut literal_start = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(close) = find_close(bytes, i + 2) {
                out.push_str(&template[literal_start..i]);
                let key = template[i + 2..close].trim();
                out.push_str(&resolve(key, resolver));
                i = close + 2;
                literal_start = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&template[literal_start..]);
    out
}

fn find_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'}' && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a key like `roles.worker.user[0]` or `roles.X.last_message` into a tuple of
/// (role, accessor, index). Index is `None` for bare accessors.
fn parse_key(key: &str) -> Option<(&str, &str, Option<isize>)> {
    let mut rest = key.strip_prefix("roles.")?;
    // Role name up to next '.'.
    let dot = rest.find('.')?;
    let role = &rest[..dot];
    rest = &rest[dot + 1..];
    // Accessor may be: "last_message" | "initial_prompt" | "user[N]" | "assistant[N]"
    if let Some(open) = rest.find('[') {
        let close = rest.find(']')?;
        if close < open {
            return None;
        }
        let accessor = &rest[..open];
        let idx_str = &rest[open + 1..close];
        let idx: isize = idx_str.trim().parse().ok()?;
        Some((role, accessor, Some(idx)))
    } else {
        Some((role, rest, None))
    }
}

/// Normalize a possibly-negative index against a slice length.
fn norm_index(len: usize, idx: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    if idx >= 0 {
        let u = idx as usize;
        if u < len { Some(u) } else { None }
    } else {
        let back = (-idx) as usize;
        if back <= len { Some(len - back) } else { None }
    }
}

fn resolve<R: RoleResolver + ?Sized>(key: &str, resolver: &R) -> String {
    // Bare run-level vars (no `roles.` prefix).
    if key == "goal" {
        if let Some(g) = resolver.goal() {
            if !g.trim().is_empty() {
                return g;
            }
        }
        // Fall back to the worker's initial_prompt when no explicit goal was set.
        return resolver.user_messages("worker").into_iter().next().unwrap_or_default();
    }
    if key == "rejected_findings" {
        let items = resolver.rejected_findings();
        if items.is_empty() {
            return String::new();
        }
        // Header + bulleted list. Header lives inside the var (not the
        // surrounding template) so the empty case collapses cleanly to
        // an empty string and the surrounding blank line just becomes
        // a single blank line.
        let mut out = String::from(
            "Previously-rejected findings — the manager has decided these are not worth raising again. Do not re-surface them:\n",
        );
        for item in items {
            out.push_str("- ");
            out.push_str(item.trim());
            out.push('\n');
        }
        return out;
    }
    let Some((role, accessor, idx)) = parse_key(key) else {
        return String::new();
    };
    match (accessor, idx) {
        ("last_message", None) => {
            let msgs = resolver.assistant_messages(role);
            msgs.into_iter().last().unwrap_or_default()
        }
        ("this_turn", None) => resolver.assistant_since_activation(role).join("\n\n---\n\n"),
        ("initial_prompt", None) => {
            let msgs = resolver.user_messages(role);
            msgs.into_iter().next().unwrap_or_default()
        }
        ("plan", None) => resolver.latest_plan(role).unwrap_or_default(),
        ("user", Some(n)) => index_into(resolver.user_messages(role), n),
        ("assistant", Some(n)) => index_into(resolver.assistant_messages(role), n),
        ("prior_user", Some(n)) => index_into(resolver.prior_user_messages(role), n),
        ("prior_assistant", Some(n)) => index_into(resolver.prior_assistant_messages(role), n),
        _ => String::new(),
    }
}

fn index_into(v: Vec<String>, idx: isize) -> String {
    match norm_index(v.len(), idx) {
        Some(i) => v.into_iter().nth(i).unwrap_or_default(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub resolver for testing.
    struct Stub {
        user: std::collections::HashMap<String, Vec<String>>,
        assistant: std::collections::HashMap<String, Vec<String>>,
        /// Per-role override for `assistant_since_activation`. When unset,
        /// the trait's default returns the same as `assistant_messages` —
        /// most tests want that, only the dedicated `this_turn` tests
        /// populate it to exercise the activation-slicing path.
        this_turn: std::collections::HashMap<String, Vec<String>>,
        prior_user: std::collections::HashMap<String, Vec<String>>,
        prior_assistant: std::collections::HashMap<String, Vec<String>>,
        plan: std::collections::HashMap<String, String>,
        goal: Option<String>,
        rejected: Vec<String>,
    }

    impl RoleResolver for Stub {
        fn user_messages(&self, role: &str) -> Vec<String> {
            self.user.get(role).cloned().unwrap_or_default()
        }
        fn assistant_messages(&self, role: &str) -> Vec<String> {
            self.assistant.get(role).cloned().unwrap_or_default()
        }
        fn assistant_since_activation(&self, role: &str) -> Vec<String> {
            if let Some(msgs) = self.this_turn.get(role) {
                return msgs.clone();
            }
            self.assistant_messages(role)
        }
        fn prior_user_messages(&self, role: &str) -> Vec<String> {
            self.prior_user.get(role).cloned().unwrap_or_default()
        }
        fn prior_assistant_messages(&self, role: &str) -> Vec<String> {
            self.prior_assistant.get(role).cloned().unwrap_or_default()
        }
        fn latest_plan(&self, role: &str) -> Option<String> {
            self.plan.get(role).cloned()
        }
        fn goal(&self) -> Option<String> {
            self.goal.clone()
        }
        fn rejected_findings(&self) -> Vec<String> {
            self.rejected.clone()
        }
    }

    fn stub() -> Stub {
        let mut user = std::collections::HashMap::new();
        user.insert(
            "worker".into(),
            vec!["fix the parser".into(), "also fix the tests".into()],
        );
        user.insert("reviewer".into(), vec![]);

        let mut assistant = std::collections::HashMap::new();
        assistant.insert(
            "worker".into(),
            vec!["done, ran tests".into(), "fixed".into()],
        );
        assistant.insert(
            "reviewer".into(),
            vec!["LGTM but nit on line 42".into()],
        );

        let mut prior_user = std::collections::HashMap::new();
        prior_user.insert(
            "worker".into(),
            vec!["let's plan".into(), "accept plan".into()],
        );

        let mut prior_assistant = std::collections::HashMap::new();
        prior_assistant.insert(
            "worker".into(),
            vec![
                "initial thoughts".into(),
                "here's the plan: step 1 ... step 2 ...".into(),
            ],
        );

        let mut plan = std::collections::HashMap::new();
        plan.insert("worker".into(), "# Plan\n\n1. thing\n2. other thing".into());

        Stub {
            user,
            assistant,
            this_turn: std::collections::HashMap::new(),
            prior_user,
            prior_assistant,
            plan,
            goal: None,
            rejected: Vec::new(),
        }
    }

    #[test]
    fn goal_explicit_overrides_initial_prompt() {
        let mut s = stub();
        s.goal = Some("migrate storage to particle filter".into());
        let out = render("{{ goal }}", &s);
        assert_eq!(out, "migrate storage to particle filter");
    }

    #[test]
    fn goal_falls_back_to_worker_initial_prompt() {
        let out = render("{{ goal }}", &stub());
        assert_eq!(out, "fix the parser");
    }

    #[test]
    fn goal_empty_string_falls_back() {
        let mut s = stub();
        s.goal = Some("   ".into());
        assert_eq!(render("{{ goal }}", &s), "fix the parser");
    }

    #[test]
    fn last_message_alias() {
        let s = render("{{ roles.worker.last_message }}", &stub());
        assert_eq!(s, "fixed");
    }

    #[test]
    fn initial_prompt_alias() {
        let s = render("{{ roles.worker.initial_prompt }}", &stub());
        assert_eq!(s, "fix the parser");
    }

    #[test]
    fn user_indexed() {
        assert_eq!(render("{{ roles.worker.user[0] }}", &stub()), "fix the parser");
        assert_eq!(render("{{ roles.worker.user[1] }}", &stub()), "also fix the tests");
        assert_eq!(render("{{ roles.worker.user[-1] }}", &stub()), "also fix the tests");
    }

    #[test]
    fn assistant_indexed() {
        assert_eq!(render("{{ roles.worker.assistant[0] }}", &stub()), "done, ran tests");
        assert_eq!(render("{{ roles.worker.assistant[-1] }}", &stub()), "fixed");
        assert_eq!(render("{{ roles.reviewer.assistant[-1] }}", &stub()), "LGTM but nit on line 42");
    }

    #[test]
    fn out_of_range_empty() {
        assert_eq!(render("{{ roles.worker.user[99] }}", &stub()), "");
        assert_eq!(render("{{ roles.worker.user[-99] }}", &stub()), "");
        assert_eq!(render("{{ roles.reviewer.user[0] }}", &stub()), "");
    }

    #[test]
    fn unknown_role_empty() {
        assert_eq!(render("[{{ roles.unknown.last_message }}]", &stub()), "[]");
    }

    #[test]
    fn unknown_accessor_empty() {
        assert_eq!(render("{{ roles.worker.fake }}", &stub()), "");
        assert_eq!(render("{{ roles.worker.fake[0] }}", &stub()), "");
    }

    #[test]
    fn literal_braces_preserved() {
        assert_eq!(render("fn x() { return 1; }", &stub()), "fn x() { return 1; }");
    }

    /// Regression: a dynamic transition prompt (manager → worker) is rendered
    /// through `render` before delivery to the worker's PTY. When the manager
    /// pastes the reviewer's findings — which routinely contain smart quotes
    /// and em-dashes — every non-ASCII byte must survive verbatim. The old
    /// `bytes[i] as char` path Latin-1-expanded each UTF-8 byte, yielding `â`
    /// mojibake plus C1 control characters that the terminal silently ate.
    #[test]
    fn literal_multibyte_utf8_preserved() {
        let t = "Reviewer’s feedback: churn is charged “not by E_perm” — \
                 fix the deadzone (x = 0 → 10 → 0).";
        assert_eq!(render(t, &stub()), t);
    }

    /// Multi-byte characters on BOTH sides of a substitution survive, and the
    /// substituted value (also possibly multi-byte) is spliced in cleanly.
    #[test]
    fn multibyte_around_substitution_preserved() {
        // worker.last_message in the stub is "fixed".
        let t = "“{{ roles.worker.last_message }}” — done ✅";
        assert_eq!(render(t, &stub()), "“fixed” — done ✅");
    }

    #[test]
    fn unclosed_braces_preserved() {
        assert_eq!(render("{{ oops", &stub()), "{{ oops");
    }

    #[test]
    fn multiple_substitutions() {
        let t = "Goal: {{ roles.worker.initial_prompt }}\nLast: {{ roles.worker.last_message }}";
        let s = render(t, &stub());
        assert_eq!(s, "Goal: fix the parser\nLast: fixed");
    }

    #[test]
    fn prior_user_indexed() {
        assert_eq!(render("{{ roles.worker.prior_user[0] }}", &stub()), "let's plan");
        assert_eq!(render("{{ roles.worker.prior_user[-1] }}", &stub()), "accept plan");
    }

    #[test]
    fn prior_assistant_indexed() {
        // The plan mode case: last pre-launch assistant message is the plan.
        assert_eq!(
            render("{{ roles.worker.prior_assistant[-1] }}", &stub()),
            "here's the plan: step 1 ... step 2 ..."
        );
        assert_eq!(
            render("{{ roles.worker.prior_assistant[0] }}", &stub()),
            "initial thoughts"
        );
    }

    #[test]
    fn prior_out_of_range_empty() {
        assert_eq!(render("{{ roles.reviewer.prior_user[0] }}", &stub()), "");
        assert_eq!(render("{{ roles.worker.prior_user[99] }}", &stub()), "");
    }

    #[test]
    fn plan_accessor() {
        assert_eq!(
            render("{{ roles.worker.plan }}", &stub()),
            "# Plan\n\n1. thing\n2. other thing"
        );
        assert_eq!(render("{{ roles.reviewer.plan }}", &stub()), "");
    }

    /// Multi-message `this_turn` joins each message with `---` separators
    /// so the manager can tell where one assistant reply ends and the
    /// next begins. Replaces the old `last_message` behavior that
    /// silently dropped all but the final message.
    #[test]
    fn this_turn_joins_multiple_messages_with_separator() {
        let mut s = stub();
        s.this_turn.insert(
            "worker".into(),
            vec![
                "first proposal".into(),
                "actually here's a better approach".into(),
                "done implementing".into(),
            ],
        );
        let out = render("{{ roles.worker.this_turn }}", &s);
        assert_eq!(
            out,
            "first proposal\n\n---\n\nactually here's a better approach\n\n---\n\ndone implementing"
        );
    }

    /// When the role has produced no messages since its most recent
    /// activation (e.g. activation just fired and the agent hasn't
    /// responded yet), `this_turn` expands to empty string.
    #[test]
    fn this_turn_empty_when_no_messages() {
        let mut s = stub();
        s.this_turn.insert("worker".into(), vec![]);
        assert_eq!(render("{{ roles.worker.this_turn }}", &s), "");
    }

    /// Default trait impl falls back to `assistant_messages` so simpler
    /// resolvers that don't track activation history (or test stubs
    /// that don't populate `this_turn`) still produce something
    /// reasonable — the full post-launch assistant transcript.
    #[test]
    fn this_turn_default_impl_falls_back_to_assistant_messages() {
        // stub() leaves this_turn empty → default impl returns
        // assistant_messages("worker") = ["done, ran tests", "fixed"]
        // joined with the separator.
        let out = render("{{ roles.worker.this_turn }}", &stub());
        assert_eq!(out, "done, ran tests\n\n---\n\nfixed");
    }

    /// Single-message case has no trailing separator.
    #[test]
    fn this_turn_single_message_has_no_separator() {
        let mut s = stub();
        s.this_turn.insert("worker".into(), vec!["only thing I said".into()]);
        assert_eq!(render("{{ roles.worker.this_turn }}", &s), "only thing I said");
    }

    /// Empty stash → empty string. First-round reviewer prompts shouldn't
    /// include a noisy "(no rejections yet)" block.
    #[test]
    fn rejected_findings_empty_renders_empty_string() {
        assert_eq!(render("{{ rejected_findings }}", &stub()), "");
    }

    /// Non-empty stash → header + bullets. Header text lives inside the
    /// var so callers can drop {{ rejected_findings }} into a template
    /// without conditional logic.
    #[test]
    fn rejected_findings_renders_header_and_bullets() {
        let mut s = stub();
        s.rejected = vec![
            "/tmp symlink check is paranoid for single-user laptops".into(),
            "perf nit on the hot path is below the noise floor".into(),
        ];
        let out = render("{{ rejected_findings }}", &s);
        assert!(
            out.starts_with("Previously-rejected findings"),
            "got: {:?}",
            out
        );
        assert!(out.contains("- /tmp symlink check is paranoid for single-user laptops"));
        assert!(out.contains("- perf nit on the hot path is below the noise floor"));
    }

    /// Whitespace inside an entry is preserved-but-trimmed; the bullet
    /// shouldn't have a double-space after the dash.
    #[test]
    fn rejected_findings_trims_per_entry() {
        let mut s = stub();
        s.rejected = vec!["   leading and trailing whitespace   ".into()];
        let out = render("{{ rejected_findings }}", &s);
        assert!(out.contains("- leading and trailing whitespace\n"));
    }
}
