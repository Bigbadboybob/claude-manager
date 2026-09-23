//! `graphics.read_file`: read the file behind a kitty graphics command that
//! names a path on this host (`t=f`, `t=t`, `t=s`), for a viewer whose
//! terminal runs on another machine. The viewer forwards the bytes in-band.
//! Operator-only; the operator can already read these files over SSH.
//! See doc/kitty-graphics-passthrough.md.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::Value;

/// Largest image a viewer may pull, across all pages.
pub const MAX_BYTES: u64 = 32 << 20;

/// Bytes per reply. Control frames are capped at 4 MiB and base64 adds a
/// third, so larger files are read in pages (`offset` advancing).
pub const PAGE_BYTES: u64 = 2 << 20;

#[derive(serde::Deserialize)]
struct Params {
    medium: String,
    path: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    size: u64,
}

pub fn rpc(params: &Value) -> Result<Value, String> {
    let p: Params =
        serde_json::from_value(params.clone()).map_err(|e| format!("graphics.read_file params: {e}"))?;
    let (bytes, eof) = read(&p.medium, &p.path, p.offset, p.size)?;
    Ok(serde_json::json!({
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        // Last page of the requested range; the viewer stops here.
        "eof": eof,
    }))
}

/// Read one page of `[offset, offset + size)` (`size == 0`: to the end).
/// Temporary files and shared memory are deleted once the last page is read.
fn read(medium: &str, path: &str, offset: u64, size: u64) -> Result<(Vec<u8>, bool), String> {
    let (path, delete_after) = match medium {
        "f" => (PathBuf::from(path), false),
        "t" => {
            let path = PathBuf::from(path);
            if !is_kitty_temp_file(&path) {
                return Err("EPERM:temporary files must be in a temp directory and contain \
                            tty-graphics-protocol in their name"
                    .into());
            }
            (path, true)
        }
        "s" => {
            let name = path.trim_start_matches('/');
            if name.is_empty() || name.contains('/') {
                return Err("EINVAL:bad shared memory name".into());
            }
            (Path::new("/dev/shm").join(name), true)
        }
        other => return Err(format!("EINVAL:unsupported medium {other:?}")),
    };
    if !path.is_absolute() {
        return Err("EINVAL:path must be absolute".into());
    }
    let result = read_regular(&path, offset, size);
    if delete_after && matches!(result, Ok((_, true))) {
        let _ = std::fs::remove_file(&path);
    }
    result
}

fn read_regular(path: &Path, offset: u64, size: u64) -> Result<(Vec<u8>, bool), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("ENOENT:{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("EINVAL:{} is not a regular file", path.display()));
    }
    let available = meta.len().saturating_sub(offset);
    let want = if size == 0 { available } else { size.min(available) };
    if want > MAX_BYTES {
        return Err(format!("EFBIG:{} bytes exceeds the {MAX_BYTES}-byte limit", want));
    }
    let page = want.min(PAGE_BYTES);
    let mut file = std::fs::File::open(path).map_err(|e| format!("EBADF:{}: {e}", path.display()))?;
    file.seek(SeekFrom::Start(offset)).map_err(|e| format!("EBADF:{e}"))?;
    let mut bytes = Vec::with_capacity(page as usize);
    file.take(page)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("EBADF:{e}"))?;
    let eof = bytes.len() as u64 >= want;
    Ok((bytes, eof))
}

/// Kitty only deletes (and therefore only accepts) temporary files that
/// live in a temp directory and carry this marker in their name.
fn is_kitty_temp_file(path: &Path) -> bool {
    if !path.to_string_lossy().contains("tty-graphics-protocol") {
        return false;
    }
    let mut roots: Vec<PathBuf> = vec!["/tmp".into(), "/dev/shm".into(), "/var/tmp".into()];
    if let Some(dir) = std::env::var_os("TMPDIR") {
        roots.push(dir.into());
    }
    let Ok(canonical) = path.canonicalize() else {
        return false;
    };
    roots
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .any(|root| canonical.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(v: Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(v["data"].as_str().unwrap())
            .unwrap()
    }

    #[test]
    fn graphics_read_file_reads_ranges_of_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.png");
        std::fs::write(&path, b"0123456789").unwrap();
        let p = path.to_str().unwrap();
        assert_eq!(decode(rpc(&serde_json::json!({"medium": "f", "path": p})).unwrap()), b"0123456789");
        assert_eq!(
            decode(rpc(&serde_json::json!({"medium": "f", "path": p, "offset": 2, "size": 3})).unwrap()),
            b"234"
        );
        assert!(path.exists(), "t=f must not delete the file");
    }

    #[test]
    fn graphics_read_file_pages_large_files_and_deletes_temp_files_after_the_last_page() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let path = dir.path().join("tty-graphics-protocol-big.png");
        let content: Vec<u8> = (0..PAGE_BYTES + 10).map(|i| i as u8).collect();
        std::fs::write(&path, &content).unwrap();
        let p = path.to_str().unwrap();
        let first = rpc(&serde_json::json!({"medium": "t", "path": p})).unwrap();
        assert_eq!(first["eof"], false);
        assert!(path.exists(), "not deleted before the last page");
        let mut got = decode(first);
        assert_eq!(got.len() as u64, PAGE_BYTES);
        let last = rpc(&serde_json::json!({"medium": "t", "path": p, "offset": got.len()})).unwrap();
        assert_eq!(last["eof"], true);
        got.extend(decode(last));
        assert_eq!(got, content);
        assert!(!path.exists());
    }

    #[test]
    fn graphics_read_file_refuses_directories_relative_paths_and_bad_media() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        assert!(rpc(&serde_json::json!({"medium": "f", "path": d})).unwrap_err().starts_with("EINVAL"));
        assert!(rpc(&serde_json::json!({"medium": "f", "path": "rel.png"})).is_err());
        assert!(rpc(&serde_json::json!({"medium": "x", "path": "/tmp/a"})).is_err());
        assert!(rpc(&serde_json::json!({"medium": "s", "path": "../etc/passwd"})).is_err());
    }

    #[test]
    fn graphics_read_file_deletes_only_marked_temp_files() {
        let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let unmarked = dir.path().join("plain.png");
        std::fs::write(&unmarked, b"x").unwrap();
        let err = rpc(&serde_json::json!({"medium": "t", "path": unmarked.to_str().unwrap()})).unwrap_err();
        assert!(err.starts_with("EPERM"), "{err}");
        assert!(unmarked.exists());

        let marked = dir.path().join("tty-graphics-protocol-1.png");
        std::fs::write(&marked, b"img").unwrap();
        let v = rpc(&serde_json::json!({"medium": "t", "path": marked.to_str().unwrap()})).unwrap();
        assert_eq!(decode(v), b"img");
        assert!(!marked.exists(), "t=t files are deleted after reading");
    }
}
