//! Minimal Jinja-ish prompt templating.
//!
//! Supported substitutions (all optional — missing keys expand to empty string):
//!   {{ roles.<role>.last_message }}    — most recent captured assistant message
//!
//! Whitespace inside `{{ ... }}` is tolerated. Everything else is left as-is.
//! Deliberately small: we don't need conditionals/loops for the current workflows.

use crate::workflow::run::WorkflowRun;

pub fn render(template: &str, run: &WorkflowRun) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find closing }}.
            if let Some(close) = find_close(bytes, i + 2) {
                let key = std::str::from_utf8(&bytes[i + 2..close]).unwrap_or("").trim();
                out.push_str(&resolve(key, run));
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

fn resolve(key: &str, run: &WorkflowRun) -> String {
    // Only `roles.<role>.last_message` is supported.
    let parts: Vec<&str> = key.split('.').map(|s| s.trim()).collect();
    if parts.len() == 3 && parts[0] == "roles" && parts[2] == "last_message" {
        if let Some(msg) = run.last_message_for(parts[1]) {
            return msg.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::run::{RoleBinding, TriggerKind, WorkflowRun};
    use std::collections::BTreeMap;

    fn run_with_history() -> WorkflowRun {
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "w".into(),
                current_session_id: None,
            },
        );
        roles.insert(
            "reviewer".to_string(),
            RoleBinding {
                session_label: "r".into(),
                current_session_id: None,
            },
        );
        let mut run = WorkflowRun::new(
            "wf".into(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
        );
        run.close_active_role(Some("I wrote a thing.".into()));
        run.activate_role(
            "reviewer".into(),
            TriggerKind::StaticIdle { from_role: "worker".into() },
        );
        run.close_active_role(Some("Looks ok, but fix X.".into()));
        run
    }

    #[test]
    fn substitutes_last_message() {
        let run = run_with_history();
        let s = render(
            "Worker: {{ roles.worker.last_message }}\nReviewer: {{ roles.reviewer.last_message }}",
            &run,
        );
        assert_eq!(s, "Worker: I wrote a thing.\nReviewer: Looks ok, but fix X.");
    }

    #[test]
    fn missing_role_expands_empty() {
        let run = run_with_history();
        let s = render("X={{ roles.manager.last_message }}Y", &run);
        assert_eq!(s, "X=Y");
    }

    #[test]
    fn unknown_key_expands_empty() {
        let run = run_with_history();
        let s = render("[{{ something.else }}]", &run);
        assert_eq!(s, "[]");
    }

    #[test]
    fn literal_braces_preserved() {
        let run = run_with_history();
        let s = render("fn x() { return 1; }", &run);
        assert_eq!(s, "fn x() { return 1; }");
    }

    #[test]
    fn unclosed_braces_preserved() {
        let run = run_with_history();
        let s = render("{{ oops", &run);
        assert_eq!(s, "{{ oops");
    }
}
