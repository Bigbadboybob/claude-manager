//! glibc malloc settings for the long-lived brain (#105).
//!
//! The brain runs ~460 threads. glibc lets threads spread over up to 8 arenas
//! per core (128 on cm-sessions) and raises its mmap threshold toward 32 MiB
//! as large blocks are freed, so multi-MB transient buffers end up carved
//! out of arena heaps. Freed space then stays in whichever arena served it.
//! On 2026-11-05 the cm-sessions brain's live heap held flat at ~3.5 GB all
//! day, while arena space grew 6.4→9.9 GB in two hours (19.9 GB on an 8-hour
//! brain) and RSS climbed 1.5–3 GB between 10-minute `malloc_trim` passes.
//!
//! Fewer arenas let threads reuse each other's freed space. A fixed mmap
//! threshold hands every block of 1 MiB or more back to the kernel when it is
//! freed, and stops glibc from moving the threshold up. An operator's own
//! `MALLOC_ARENA_MAX` / `MALLOC_MMAP_THRESHOLD_`, or the equivalent
//! `GLIBC_TUNABLES` entries, win over these defaults.

/// Arenas for the whole brain. Most of its threads block on I/O and seldom
/// allocate; the busy ones (delivery, pollers, RPC connections) are few.
pub const ARENA_MAX: i32 = 4;
/// Blocks this large or larger are mmapped and returned on free.
pub const MMAP_THRESHOLD: i32 = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// `None`: the environment already sets it.
    pub arena_max: Option<i32>,
    pub mmap_threshold: Option<i32>,
}

/// What to apply, given the process environment.
pub fn plan(env: impl Fn(&str) -> Option<String>) -> Plan {
    let tunables = env("GLIBC_TUNABLES").unwrap_or_default();
    let preset = |var: &str, tunable: &str| {
        env(var).is_some()
            || tunables
                .split(':')
                .any(|entry| entry.split('=').next() == Some(tunable))
    };
    Plan {
        arena_max: (!preset("MALLOC_ARENA_MAX", "glibc.malloc.arena_max")).then_some(ARENA_MAX),
        mmap_threshold: (!preset("MALLOC_MMAP_THRESHOLD_", "glibc.malloc.mmap_threshold"))
            .then_some(MMAP_THRESHOLD),
    }
}

/// Apply the plan. Call once, first thing in the brain, before it starts
/// threads: an arena cap does not merge arenas that already exist. Returns
/// a one-line summary for the daemon log.
pub fn apply() -> String {
    let plan = plan(|key| std::env::var(key).ok());
    #[cfg(target_env = "gnu")]
    {
        let set = |name: &str, param: libc::c_int, value: Option<i32>| match value {
            // SAFETY: mallopt only adjusts allocator parameters.
            Some(v) if unsafe { libc::mallopt(param, v) } == 1 => format!("{name}={v}"),
            Some(v) => format!("{name}={v} (refused)"),
            None => format!("{name}=environment"),
        };
        format!(
            "{} {}",
            set("arena_max", libc::M_ARENA_MAX, plan.arena_max),
            set("mmap_threshold", libc::M_MMAP_THRESHOLD, plan.mmap_threshold),
        )
    }
    #[cfg(not(target_env = "gnu"))]
    {
        let _ = plan;
        "unchanged (not glibc)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| vars.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn defaults_apply_when_the_environment_is_silent() {
        assert_eq!(
            plan(env(&[])),
            Plan { arena_max: Some(ARENA_MAX), mmap_threshold: Some(MMAP_THRESHOLD) }
        );
    }

    #[test]
    fn an_operator_setting_wins_by_variable_or_tunable() {
        assert_eq!(plan(env(&[("MALLOC_ARENA_MAX", "8")])).arena_max, None);
        assert_eq!(plan(env(&[("MALLOC_MMAP_THRESHOLD_", "65536")])).mmap_threshold, None);
        let tuned = plan(env(&[(
            "GLIBC_TUNABLES",
            "glibc.malloc.check=0:glibc.malloc.arena_max=2",
        )]));
        assert_eq!(tuned, Plan { arena_max: None, mmap_threshold: Some(MMAP_THRESHOLD) });
        let tuned = plan(env(&[("GLIBC_TUNABLES", "glibc.malloc.mmap_threshold=262144")]));
        assert_eq!(tuned, Plan { arena_max: Some(ARENA_MAX), mmap_threshold: None });
        // A prefix of a tunable name is not that tunable.
        let other = plan(env(&[("GLIBC_TUNABLES", "glibc.malloc.arena_max_x=1")]));
        assert_eq!(other.arena_max, Some(ARENA_MAX));
    }
}
