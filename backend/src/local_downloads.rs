//! Local (client-machine) persistence and reveal for downloads.
//!
//! The plugin webview cannot save files itself: the host's `fileTransfer` API
//! is optional (absent on current hosts / web mode) and `<a download>` is
//! silently cancelled inside a Tauri/WKWebView without a download handler.
//! The sidecar therefore writes finished downloads to the user's Downloads
//! folder so the completion notice can show a real path and the transfer
//! panel can offer reveal/open. `files/local/reveal` opens the file manager
//! and `files/local/open` opens the downloaded file in the OS default app.
//!
//! Reveal/open are deliberately restricted to paths recorded by a completed
//! local download in the persisted transfer history — never an arbitrary
//! open-path primitive (parity with dbx-plugin-ssh `local_downloads.rs`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::store::TransferRecord;

/// Forces the local-save capability on/off for deployments where the default
/// detection (see `can_save_local`) guesses wrong (e.g. a docker deployment
/// that mounts a Downloads folder).
pub const LOCAL_SAVE_ENV: &str = "DBX_FILES_LOCAL_SAVE";
/// Overrides the base directory downloads are saved into (also used by the
/// smoke tests to keep them out of the developer's real Downloads folder).
pub const DOWNLOAD_DIR_ENV: &str = "DBX_FILES_DOWNLOAD_DIR";

fn env_value(lookup: &impl Fn(&str) -> Option<OsString>, key: &str) -> Option<OsString> {
    lookup(key).filter(|value| !value.to_string_lossy().trim().is_empty())
}

/// Whether saving downloads next to this process is meaningful: true when the
/// sidecar runs inside a desktop session on the user's machine. The default
/// detection treats macOS/Windows as desktop (sidecar always ships inside the
/// app there); on Linux it requires a display, so headless web/docker hosts
/// keep using the host file-transfer / browser download fallback.
/// `LOCAL_SAVE_ENV` overrides.
pub fn can_save_local(lookup: impl Fn(&str) -> Option<OsString>) -> bool {
    if let Some(flag) = env_value(&lookup, LOCAL_SAVE_ENV) {
        return matches!(
            flag.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        );
    }
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    lookup("DISPLAY").is_some() || lookup("WAYLAND_DISPLAY").is_some()
}

/// Stable platform tag for the capabilities probe (`macos`/`windows`/`linux`/`other`).
pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

/// Validates a user-supplied download directory without creating or modifying
/// anything. Preferences must point at an existing absolute directory so a
/// typo cannot silently create a folder relative to the sidecar's cwd.
pub fn validate_download_dir(raw: &str) -> Result<PathBuf, String> {
    let path = raw.trim();
    if path.is_empty() {
        return Err("Download directory path must not be empty".to_string());
    }
    if path
        .chars()
        .any(|character| character == '\0' || character.is_control())
    {
        return Err("Download directory path contains invalid control characters".to_string());
    }

    let directory = Path::new(path);
    if !directory.is_absolute() {
        return Err("Download directory path must be absolute".to_string());
    }
    let metadata = std::fs::metadata(directory)
        .map_err(|error| format!("Download directory is not accessible: {error}"))?;
    if !metadata.is_dir() {
        return Err("Download directory path is not a directory".to_string());
    }
    Ok(directory.to_path_buf())
}

/// Base directory downloads land in: explicit `download_dir` override (the
/// workbench save-path preference), then `DOWNLOAD_DIR_ENV`, then the user's
/// Downloads folder (created on demand), then the home directory, then a
/// folder under the plugin data dir so this never fails. Explicit preferences
/// are validated by `validate_download_dir` before this helper is called.
pub fn downloads_base_dir(
    download_dir: Option<&str>,
    lookup: impl Fn(&str) -> Option<OsString>,
    data_dir: &Path,
) -> PathBuf {
    if let Some(dir) = download_dir
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| env_value(&lookup, DOWNLOAD_DIR_ENV).map(PathBuf::from))
    {
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }
    let home = if cfg!(windows) {
        env_value(&lookup, "USERPROFILE").map(PathBuf::from)
    } else {
        env_value(&lookup, "HOME").map(PathBuf::from)
    };
    if let Some(home) = home {
        let downloads = home.join("Downloads");
        if std::fs::create_dir_all(&downloads).is_ok() {
            return downloads;
        }
        return home;
    }
    let fallback = data_dir.join("downloads");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Strips path separators and control characters from a remote-provided file
/// name; trailing dots/spaces are removed for Windows targets. Empty results
/// fall back to "download".
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !matches!(c, '/' | '\\'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']).trim();
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Picks a non-colliding path in `base` for `file_name`, appending " (n)"
/// before the extension like browsers do. The final name is decided when the
/// download finishes so a failed transfer never reserves a name.
pub fn pick_download_path(base: &Path, file_name: &str) -> PathBuf {
    let name = sanitize_file_name(file_name);
    let candidate = base.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    let stem_end = name.rfind('.').filter(|dot| *dot > 0).unwrap_or(name.len());
    let (stem, ext) = name.split_at(stem_end);
    for index in 1..=999 {
        let candidate = base.join(format!("{stem} ({index}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    base.join(format!("{stem}-{stamp}{ext}"))
}

/// Opens the platform file manager with `path` selected (or its parent folder
/// selected when the file was already moved away). Spawn failures surface as
/// errors; explorer's nonzero exit codes are famously meaningless and ignored.
pub fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        if std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .status()
            .map_err(|error| format!("Failed to launch Finder: {error}"))?
            .success()
        {
            return Ok(());
        }
        let parent = path.parent().unwrap_or(path);
        return std::process::Command::new("open")
            .arg(parent)
            .status()
            .map_err(|error| format!("Failed to launch Finder: {error}"))
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err("Finder exited with an error".to_string())
                }
            });
    }
    if cfg!(windows) {
        let selected = format!("/select,{}", path.as_os_str().to_string_lossy());
        return std::process::Command::new("explorer")
            .arg(selected)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Failed to launch Explorer: {error}"));
    }
    let parent = path.parent().unwrap_or(path);
    std::process::Command::new("xdg-open")
        .arg(parent)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Failed to launch file manager: {error}"))
}

/// Opens a downloaded file with the operating system's default application.
pub fn open_in_default_app(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err("Downloaded file no longer exists".to_string());
    }
    if cfg!(target_os = "macos") {
        return std::process::Command::new("open")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Failed to open downloaded file: {error}"));
    }
    if cfg!(windows) {
        return std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Failed to open downloaded file: {error}"));
    }
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Failed to open downloaded file: {error}"))
}

/// Pure membership check behind `reveal_validated`/`open_validated`, unit
/// testable without launching anything: only a completed download row that
/// recorded this exact `localPath` may be opened. Survives sidecar restarts
/// because history rows persist in `transfers.json`.
pub fn is_recorded_download(history: &[TransferRecord], path: &Path) -> bool {
    let path_text = path.to_string_lossy();
    history.iter().any(|record| {
        record.kind == "download"
            && record.status == "completed"
            && record.local_path.as_deref() == Some(path_text.as_ref())
    })
}

/// Validates `path` against the persisted transfer history before revealing:
/// `local/reveal` (file manager, path selected).
pub fn reveal_validated(history: &[TransferRecord], path: &Path) -> Result<(), String> {
    if is_recorded_download(history, path) {
        reveal_in_file_manager(path)
    } else {
        Err("Path was not saved by a completed download of this plugin".to_string())
    }
}

/// Same allowlist as reveal, but opens the file itself rather than its folder.
pub fn open_validated(history: &[TransferRecord], path: &Path) -> Result<(), String> {
    if is_recorded_download(history, path) {
        open_in_default_app(path)
    } else {
        Err("Path was not saved by a completed download of this plugin".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TransferRecord;

    fn lookup_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    fn record(task_id: &str, kind: &str, status: &str, local_path: Option<&str>) -> TransferRecord {
        TransferRecord {
            task_id: task_id.to_string(),
            connection_id: "c1".to_string(),
            kind: kind.to_string(),
            remote_path: format!("/remote/{task_id}.bin"),
            total_bytes: Some(10),
            transferred_bytes: 10,
            status: status.to_string(),
            error: None,
            started_at: Some(1),
            finished_at: Some(2),
            local_path: local_path.map(str::to_string),
        }
    }

    #[test]
    fn sanitize_strips_separators_and_traversal() {
        assert_eq!(sanitize_file_name("report.tar.gz"), "report.tar.gz");
        // Separators are gone, so ".."-heavy names can never traverse; the
        // residual dots are harmless (remote names never contain '/' anyway).
        assert_eq!(sanitize_file_name("../../etc/passwd"), "....etcpasswd");
        assert_eq!(sanitize_file_name("../.."), "download");
        assert_eq!(sanitize_file_name("a/b\\c"), "abc");
        assert_eq!(sanitize_file_name("name... "), "name");
        assert_eq!(sanitize_file_name("  "), "download");
        assert_eq!(sanitize_file_name(""), "download");
        assert_eq!(sanitize_file_name("we\nird"), "we ird");
    }

    #[test]
    fn pick_download_path_avoids_collisions() {
        let base = tempfile::tempdir().expect("tempdir");
        let first = pick_download_path(base.path(), "log.txt");
        assert_eq!(first.file_name().unwrap(), "log.txt");
        std::fs::write(&first, b"x").expect("write");
        let second = pick_download_path(base.path(), "log.txt");
        assert_eq!(second.file_name().unwrap(), "log (1).txt");
        // A dotfile ("stem" is the whole name) must not become ".hidden (1)."
        let dot = pick_download_path(base.path(), ".hidden");
        assert_eq!(dot.file_name().unwrap(), ".hidden");
    }

    #[test]
    fn downloads_base_dir_prefers_override_then_env_then_home() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let target = tempfile::tempdir().expect("tempdir");
        // Workbench save-path preference wins.
        let dir = downloads_base_dir(
            Some(target.path().to_string_lossy().as_ref()),
            lookup_from(&[]),
            data_dir.path(),
        );
        assert_eq!(dir, target.path());
        // Blank preference falls through to the env override.
        let dir = downloads_base_dir(
            Some("   "),
            lookup_from(&[(
                "DBX_FILES_DOWNLOAD_DIR",
                target.path().to_string_lossy().as_ref(),
            )]),
            data_dir.path(),
        );
        assert_eq!(dir, target.path());
        let home = tempfile::tempdir().expect("tempdir");
        let downloads = home.path().join("Downloads");
        std::fs::create_dir_all(&downloads).expect("mkdir");
        let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let dir = downloads_base_dir(
            None,
            lookup_from(&[(home_key, home.path().to_string_lossy().as_ref())]),
            data_dir.path(),
        );
        assert_eq!(dir, downloads);
    }

    #[test]
    fn validate_download_dir_requires_existing_absolute_directory() {
        let parent = tempfile::tempdir().expect("tempdir");
        let directory = parent.path().join("downloads");
        std::fs::create_dir(&directory).expect("mkdir");
        let valid = validate_download_dir(&format!("  {}  ", directory.display()))
            .expect("existing directory should validate");
        assert_eq!(valid, directory);

        let file = parent.path().join("file.txt");
        std::fs::write(&file, b"x").expect("write");
        assert!(validate_download_dir(&file.to_string_lossy()).is_err());
        assert!(validate_download_dir(&parent.path().join("missing").to_string_lossy()).is_err());
        assert!(validate_download_dir("downloads").is_err());
        assert!(validate_download_dir("/tmp/invalid\npath").is_err());
    }

    #[test]
    fn can_save_local_env_overrides_platform_default() {
        // env explicit off wins everywhere
        assert!(!can_save_local(lookup_from(&[("DBX_FILES_LOCAL_SAVE", "0")])));
        assert!(can_save_local(lookup_from(&[("DBX_FILES_LOCAL_SAVE", "true")])));
        // no env: Linux requires a display; a blank value is "unset", so it
        // must behave exactly like the absent case regardless of platform.
        assert_eq!(
            can_save_local(lookup_from(&[("DBX_FILES_LOCAL_SAVE", "  ")])),
            can_save_local(lookup_from(&[]))
        );
    }

    #[test]
    fn reveal_requires_recorded_completed_download() {
        let history = vec![
            record("t1", "download", "completed", Some("/Downloads/a.txt")),
            record("t2", "upload", "completed", None),
            record("t3", "download", "failed", None),
            // Upload rows never carry a reveal-able local path, and a finished
            // row whose localPath drifted (file moved) must not validate.
            record("t4", "download", "completed", Some("/Downloads/b.txt")),
        ];
        assert!(is_recorded_download(&history, Path::new("/Downloads/a.txt")));
        assert!(!is_recorded_download(&history, Path::new("/Downloads/c.txt")));
        assert!(!is_recorded_download(&history, Path::new("/etc/passwd")));
        let rejected = reveal_validated(&history, Path::new("/etc/passwd"));
        assert!(rejected
            .unwrap_err()
            .contains("not saved by a completed download"));
    }

    #[test]
    fn open_requires_recorded_completed_download() {
        let history = vec![record("t1", "download", "completed", Some("/Downloads/a.txt"))];
        assert!(is_recorded_download(&history, Path::new("/Downloads/a.txt")));
        assert!(!is_recorded_download(&history, Path::new("/etc/passwd")));
    }
}
