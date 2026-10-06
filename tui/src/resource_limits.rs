//! A viewer attaches to every open session, each with sockets, pipes and pollers.
//! Desktop launchers commonly inherit a 1024-file soft limit, too small for a fleet.

use std::io;

const VIEWER_NOFILE: libc::rlim_t = 65_536;

fn target_soft_limit(soft: libc::rlim_t, hard: libc::rlim_t) -> libc::rlim_t {
    soft.max(VIEWER_NOFILE.min(hard))
}

/// Raise only our soft limit, before starting threads or restoring attachments.
/// No privileged operation, global configuration change or hard-limit increase.
pub fn raise_open_file_limit() -> io::Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes the initialized rlimit; setrlimit reads it.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let target = target_soft_limit(limit.rlim_cur, limit.rlim_max);
    if target != limit.rlim_cur {
        limit.rlim_cur = target;
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_hard_limit_and_never_lowers_an_existing_limit() {
        assert_eq!(target_soft_limit(1024, 1_048_576), VIEWER_NOFILE);
        assert_eq!(target_soft_limit(256, 4096), 4096);
        assert_eq!(target_soft_limit(131_072, 1_048_576), 131_072);
        assert_eq!(target_soft_limit(1024, libc::RLIM_INFINITY), VIEWER_NOFILE);
    }

    #[test]
    fn desktop_limit_allows_more_than_1024_open_files_after_startup() {
        const CHILD: &str = "CM_TEST_VIEWER_NOFILE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "resource_limits::tests::desktop_limit_allows_more_than_1024_open_files_after_startup", "--nocapture"])
                .env(CHILD, "1")
                .status().unwrap();
            assert!(status.success());
            return;
        }
        // The test subprocess owns this process-wide limit; parallel tests do not.
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        assert!(
            limit.rlim_max > 1200,
            "test host needs a hard limit above 1200"
        );
        limit.rlim_cur = 1024;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut files = Vec::new();
        let error = loop {
            match std::fs::File::open("/dev/null") {
                Ok(file) => files.push(file),
                Err(e) => break e,
            }
        };
        assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
        let transport = crate::hosts::HostTransport::Unix {
            socket: std::path::PathBuf::from("/tmp/unused-clipboard-test.sock"),
        };
        let paste_error = match crate::clipboard::prepare_paste(&transport) {
            Err(error) => error,
            Ok(_) => panic!("an exhausted clipboard helper must not report empty"),
        };
        assert_eq!(paste_error.raw_os_error(), Some(libc::EMFILE));
        raise_open_file_limit().unwrap();
        while files.len() < 1200 {
            files.push(std::fs::File::open("/dev/null").unwrap());
        }
        assert!(std::process::Command::new("true")
            .status()
            .unwrap()
            .success());
    }
}
