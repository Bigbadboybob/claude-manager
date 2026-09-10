//! Operator-only, host-local cleanup jobs. Python workers retain their own
//! durable receipts and survive viewer disconnects and brain-only restarts.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PAYLOADS: &[(&str, &str)] = &[
    (
        "worktree_lineage.py",
        include_str!("../../scripts/worktree_lineage.py"),
    ),
    (
        "worktree_reaper.py",
        include_str!("../../scripts/worktree_reaper.py"),
    ),
    (
        "worktree_cleanup.py",
        include_str!("../../scripts/worktree_cleanup.py"),
    ),
];

pub fn install_runtime() -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME missing"))?
        .join(".cm/worktree-tools");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    for (name, body) in PAYLOADS {
        let target = dir.join(name);
        if std::fs::read_to_string(&target).ok().as_deref() != Some(body) {
            let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
            use std::io::Write;
            temp.write_all(body.as_bytes())?;
            temp.as_file().sync_all()?;
            temp.persist(&target)?;
        }
    }
    Ok(dir)
}

pub fn prepare_repo(repo: &Path) {
    // Git hooks preserve the repository's existing hook and its exit status.
    // Failure is observable but must not block ordinary worktree creation.
    let result = install_runtime().and_then(|dir| {
        let output = Command::new("python3")
            .arg(dir.join("worktree_lineage.py"))
            .arg("install")
            .arg(repo)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    });
    if let Err(error) = result {
        eprintln!("cm-daemon: worktree ownership hook: {error}");
    }
}

pub fn rpc(params: &Value) -> Result<Value, String> {
    let dir = install_runtime().map_err(|e| e.to_string())?;
    let output = Command::new("python3")
        .arg(dir.join("worktree_cleanup.py"))
        .arg("rpc")
        .arg(serde_json::to_string(params).map_err(|e| e.to_string())?)
        .output()
        .map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        format!(
            "Cleanup helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    if !output.status.success() {
        return Err(value["error"]
            .as_str()
            .unwrap_or("Cleanup request failed")
            .to_string());
    }
    Ok(value)
}

pub fn bootstrap() {
    // Never hold up control-socket acceptance or holder adoption to scan Git.
    std::thread::spawn(|| {
        let result = install_runtime().and_then(|dir| {
            let status = Command::new("python3")
                .arg(dir.join("worktree_cleanup.py"))
                .arg("bootstrap")
                .stdin(Stdio::null())
                .status()?;
            anyhow::ensure!(status.success(), "ownership bootstrap exited {status}");
            Ok(())
        });
        if let Err(error) = result {
            eprintln!("cm-daemon: worktree cleanup bootstrap: {error}");
        }
    });
}
