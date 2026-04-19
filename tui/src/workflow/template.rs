//! Prompt templating for workflow activation prompts.
//!
//! Template substitutions (all optional — missing keys expand to empty string):
//!
//! ```text
//!   {{ roles.<role>.user[N] }}         Nth user-typed message (N < 0 from end)
//!   {{ roles.<role>.assistant[N] }}    Nth assistant message
//!   {{ roles.<role>.last_message }}    alias for `assistant[-1]`
//!   {{ roles.<role>.initial_prompt }}  alias for `user[0]`
//! ```
//!
//! The template engine is deliberately small: no conditionals, loops, or filters.
//! It calls a `RoleResolver` which the caller implements to fetch messages for a
//! role (typically by reading that role's Claude/Codex JSONL transcript).

/// How the template engine asks about a role's messages.
///
/// Implementors read the role's transcript and return the requested slice.
/// Returning `None` for any accessor expands to empty string in the template.
pub trait RoleResolver {
    /// All user-typed turns for the role, in order. Used for `user[N]` and the
    /// `initial_prompt` alias.
    fn user_messages(&self, role: &str) -> Vec<String>;
    /// All assistant turns for the role, in order. Used for `assistant[N]` and
    /// the `last_message` alias.
    fn assistant_messages(&self, role: &str) -> Vec<String>;
}

pub fn render<R: RoleResolver + ?Sized>(template: &str, resolver: &R) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(close) = find_close(bytes, i + 2) {
                let key = std::str::from_utf8(&bytes[i + 2..close]).unwrap_or("").trim();
                out.push_str(&resolve(key, resolver));
                i = close + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
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
    let Some((role, accessor, idx)) = parse_key(key) else {
        return String::new();
    };
    match (accessor, idx) {
        ("last_message", None) => {
            let msgs = resolver.assistant_messages(role);
            msgs.into_iter().last().unwrap_or_default()
        }
        ("initial_prompt", None) => {
            let msgs = resolver.user_messages(role);
            msgs.into_iter().next().unwrap_or_default()
        }
        ("user", Some(n)) => index_into(resolver.user_messages(role), n),
        ("assistant", Some(n)) => index_into(resolver.assistant_messages(role), n),
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
    }

    impl RoleResolver for Stub {
        fn user_messages(&self, role: &str) -> Vec<String> {
            self.user.get(role).cloned().unwrap_or_default()
        }
        fn assistant_messages(&self, role: &str) -> Vec<String> {
            self.assistant.get(role).cloned().unwrap_or_default()
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
        Stub { user, assistant }
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
}
