//! Local (client-machine) persistence and reveal for downloads.
//!
//! The plugin webview cannot save files itself: the host's `fileTransfer` API
//! is optional (absent on current hosts / web mode) and `<a download>` is
//! silently cancelled inside a Tauri/WKWebView without a download handler.
//! The sidecar therefore writes finished downloads to the user's Downloads
//! folder so the completion notice can show a real path and the transfer
//! panel can offer reveal/open. `files/local/reveal` opens the file manager
//! and `files/local/open` opens the downloaded file in the OS default app,
//! or in a user-configured external application when the optional `app`
//! preference is supplied (issue #11).
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

/// Validates a user-supplied external application path for `files/local/open`
/// (issue #11). Must be a single existing absolute *file* — never a shell, a
/// command line or a flag blob — so the sidecar only ever executes one binary
/// the user explicitly pinned in the settings panel. The same check runs when
/// the preference is saved (`files/local/validate-open-app`) and again at
/// open time so a stale path cannot linger.
/// macOS additionally accepts an `.app` bundle directory: launches go through
/// `open -a <app>` (see `app_launch_command`), which expects the bundle rather
/// than the buried Mach-O binary.
pub fn validate_open_app(raw: &str) -> Result<PathBuf, String> {
    let path_text = raw.trim();
    if path_text.is_empty() {
        return Err("External app path must not be empty".to_string());
    }
    if path_text
        .chars()
        .any(|character| character == '\0' || character.is_control())
    {
        return Err("External app path contains invalid control characters".to_string());
    }
    let path = Path::new(path_text);
    if !path.is_absolute() {
        return Err("External app path must be absolute".to_string());
    }
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("External app is not accessible: {error}"))?;
    if metadata.is_file() {
        return Ok(path.to_path_buf());
    }
    if metadata.is_dir() && is_macos_app_bundle(path) {
        return Ok(path.to_path_buf());
    }
    Err("External app path is not a file".to_string())
}

/// Pure bundle check behind `validate_open_app`: only `<name>.app` directories
/// count as macOS application bundles — arbitrary directories stay rejected so
/// the setting can never become a "open folder with…" primitive.
fn is_macos_app_bundle(path: &Path) -> bool {
    platform_name() == "macos"
        && path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

/// One built-in "open with" suggestion for the current platform. Paths follow
/// each vendor's default install layout; `detect_apps` filters by existence
/// so the UI only offers apps that are actually installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub path: &'static str,
}

/// Pure per-platform preset table (`macos`/`windows`/`linux`/`other`), unit
/// tested for shape; existence probing happens in `detect_apps`.
pub fn app_presets(platform: &str) -> Vec<AppPreset> {
    match platform {
        "macos" => vec![
            AppPreset { id: "wps", name: "WPS Office", path: "/Applications/wpsoffice.app" },
            AppPreset { id: "excel", name: "Microsoft Excel", path: "/Applications/Microsoft Excel.app" },
            AppPreset { id: "word", name: "Microsoft Word", path: "/Applications/Microsoft Word.app" },
            AppPreset { id: "numbers", name: "Numbers", path: "/System/Applications/Numbers.app" },
            AppPreset { id: "libreoffice", name: "LibreOffice", path: "/Applications/LibreOffice.app" },
            AppPreset { id: "vscode", name: "VS Code", path: "/Applications/Visual Studio Code.app" },
        ],
        "windows" => vec![
            AppPreset { id: "wps", name: "WPS Office", path: "C:\\Program Files\\Kingsoft\\WPS Office\\ksolaunch.exe" },
            AppPreset { id: "excel", name: "Microsoft Excel", path: "C:\\Program Files\\Microsoft Office\\root\\Office16\\EXCEL.EXE" },
            AppPreset { id: "word", name: "Microsoft Word", path: "C:\\Program Files\\Microsoft Office\\root\\Office16\\WINWORD.EXE" },
            AppPreset { id: "libreoffice", name: "LibreOffice", path: "C:\\Program Files\\LibreOffice\\program\\soffice.exe" },
            AppPreset { id: "vscode", name: "VS Code", path: "C:\\Program Files\\Microsoft VS Code\\Code.exe" },
        ],
        "linux" => vec![
            AppPreset { id: "wps", name: "WPS Office", path: "/usr/bin/wps" },
            AppPreset { id: "libreoffice", name: "LibreOffice", path: "/usr/bin/libreoffice" },
            AppPreset { id: "vscode", name: "VS Code", path: "/usr/bin/code" },
        ],
        _ => Vec::new(),
    }
}

/// Presets whose path exists on this machine (regular file, or an `.app`
/// bundle directory on macOS). Unknown platforms yield an empty list.
pub fn detect_apps(platform: &str) -> Vec<AppPreset> {
    filter_detected_apps(
        app_presets(platform),
        |path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.is_file() || is_macos_app_bundle(Path::new(path)))
                .unwrap_or(false)
        },
    )
}

/// Pure existence filter behind `detect_apps`, injectable for tests.
fn filter_detected_apps(presets: Vec<AppPreset>, exists: impl Fn(&str) -> bool) -> Vec<AppPreset> {
    presets
        .into_iter()
        .filter(|preset| exists(preset.path))
        .collect()
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

/// Resolves the final local path for a finished download. `overwrite`
/// replaces an existing file in place (the caller removes it before the
/// rename; std::fs::rename fails on Windows when the target exists);
/// otherwise [`pick_download_path`] appends " (n)" like browsers do. The
/// name is decided at finalize time so a failed transfer never reserves it.
pub fn finalize_download_path(base: &Path, file_name: &str, overwrite: bool) -> PathBuf {
    if overwrite {
        base.join(sanitize_file_name(file_name))
    } else {
        pick_download_path(base, file_name)
    }
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

/// Pure platform dispatch behind `open_in_app`, unit-testable on every host:
/// how the configured external application is launched for a given platform.
/// macOS routes through the `open` launcher (`open -a <app> <file>`, which
/// also accepts .app bundle paths); Windows and Linux execute the configured
/// path directly via `std::process::Command` — no shell, no parsing, so a
/// path with spaces is passed through as one argument and nothing the user
/// typed is ever interpreted as a command line.
pub fn app_launch_command(platform: &str, app: &Path) -> (OsString, Vec<OsString>) {
    if platform == "macos" {
        (
            OsString::from("open"),
            vec![OsString::from("-a"), app.as_os_str().to_os_string()],
        )
    } else {
        (app.as_os_str().to_os_string(), Vec::new())
    }
}

/// Opens a downloaded file with the operating system's default application,
/// or with the user-configured external application when `app` is `Some`
/// (already validated by `validate_open_app`). Spawn failures surface as
/// errors; like the default-app path the spawned process is not awaited.
pub fn open_in_app(path: &Path, app: Option<&Path>) -> Result<(), String> {
    if !path.is_file() {
        return Err("Downloaded file no longer exists".to_string());
    }
    let Some(app) = app else {
        return open_in_default_app(path);
    };
    let (program, mut args) = app_launch_command(platform_name(), app);
    args.push(path.as_os_str().to_os_string());
    std::process::Command::new(&program)
        .args(&args)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Failed to launch the configured external app: {error}"))
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
/// `app` is the optional user-configured external application (issue #11):
/// a blank value means the OS default app, anything else must pass
/// `validate_open_app` (existing absolute file) before the download-history
/// allowlist is consulted, so a stale preference fails with a config error
/// even when the target file itself is still recorded.
pub fn open_validated(
    history: &[TransferRecord],
    path: &Path,
    app: Option<&str>,
) -> Result<(), String> {
    let app = app
        .map(str::trim)
        .filter(|app| !app.is_empty())
        .map(validate_open_app)
        .transpose()?;
    if is_recorded_download(history, path) {
        open_in_app(path, app.as_deref())
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
    fn finalize_download_path_overwrite_replaces_existing_file() {
        let base = tempfile::tempdir().expect("tempdir");
        let target = base.path().join("log.txt");
        std::fs::write(&target, b"old").expect("write");
        // overwrite 档：final 名固定为原名（替换语义由调用方 remove+rename 完成）。
        let path = finalize_download_path(base.path(), "log.txt", true);
        assert_eq!(path, target);
        // rename 档（默认）：撞名让位。
        let renamed = finalize_download_path(base.path(), "log.txt", false);
        assert_eq!(renamed.file_name().unwrap(), "log (1).txt");
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

    #[test]
    fn validate_open_app_requires_existing_absolute_file() {
        let parent = tempfile::tempdir().expect("tempdir");
        let executable = parent.path().join("editor");
        std::fs::write(&executable, b"MZ").expect("write");
        let valid = validate_open_app(&format!("  {}  ", executable.display()))
            .expect("existing absolute file should validate");
        assert_eq!(valid, executable);

        // A directory, a missing path, a relative path and control characters
        // are all rejected: only one pinned binary may ever be executed.
        assert!(validate_open_app(&parent.path().to_string_lossy()).is_err());
        assert!(validate_open_app(&parent.path().join("missing").to_string_lossy()).is_err());
        assert!(validate_open_app("editor").is_err());
        assert!(validate_open_app("/tmp/bad\npath").is_err());
        assert!(validate_open_app("   ").is_err());
    }

    #[test]
    fn validate_open_app_accepts_macos_app_bundle_only_on_macos() {
        let parent = tempfile::tempdir().expect("tempdir");
        let bundle = parent.path().join("WPS Office.app");
        std::fs::create_dir(&bundle).expect("mkdir");
        let bundle_text = bundle.to_string_lossy().to_string();
        if platform_name() == "macos" {
            // `open -a` launches bundles, so the directory form must validate
            // there — a plain directory still does not.
            assert!(validate_open_app(&bundle_text).is_ok());
            assert!(validate_open_app(&parent.path().to_string_lossy()).is_err());
        } else {
            assert!(validate_open_app(&bundle_text).is_err());
        }
    }

    #[test]
    fn app_presets_cover_desktop_platforms_with_absolute_paths() {
        for platform in ["macos", "windows", "linux"] {
            let presets = app_presets(platform);
            assert!(!presets.is_empty(), "{platform} should ship presets");
            let mut ids: Vec<_> = presets.iter().map(|preset| preset.id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), presets.len(), "{platform} preset ids must be unique");
            for preset in &presets {
                assert!(!preset.name.is_empty());
                if platform == "windows" {
                    assert!(preset.path.starts_with('\\') || preset.path.as_bytes()[1] == b':');
                } else {
                    assert!(preset.path.starts_with('/'), "{} must be absolute", preset.path);
                }
            }
        }
        // macOS presets stay bundle-shaped: `open -a` expects the bundle.
        for preset in app_presets("macos") {
            assert!(preset.path.ends_with(".app"), "{} must be a bundle", preset.path);
        }
        assert!(app_presets("other").is_empty());
    }

    #[test]
    fn detect_apps_keeps_only_existing_paths() {
        let presets = vec![
            AppPreset { id: "wps", name: "WPS Office", path: "/opt/wps" },
            AppPreset { id: "excel", name: "Microsoft Excel", path: "/opt/office/excel" },
        ];
        let detected = filter_detected_apps(presets, |path| path == "/opt/wps");
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].id, "wps");
        assert!(filter_detected_apps(Vec::new(), |_| true).is_empty());
    }

    #[test]
    fn app_launch_command_routes_macos_through_open() {
        let app = Path::new("/Applications/Notepad++.app");
        let (program, args) = app_launch_command("macos", app);
        assert_eq!(program, OsString::from("open"));
        assert_eq!(
            args,
            vec![OsString::from("-a"), OsString::from("/Applications/Notepad++.app")]
        );
    }

    #[test]
    fn app_launch_command_executes_the_path_directly_elsewhere() {
        // Windows and Linux must not go through a shell: the configured path
        // is the program itself and the file is appended by the caller.
        for platform in ["windows", "linux", "other"] {
            let app = Path::new(if platform == "windows" {
                r"C:\Program Files\Notepad++\notepad++.exe"
            } else {
                "/usr/bin/notepad-plus-plus"
            });
            let (program, args) = app_launch_command(platform, app);
            assert_eq!(program, app.as_os_str().to_os_string());
            assert!(args.is_empty());
        }
    }

    #[test]
    fn open_validated_rejects_stale_or_invalid_app_before_launching() {
        let history = vec![record("t1", "download", "completed", Some("/Downloads/a.txt"))];
        let target = Path::new("/Downloads/a.txt");
        // An invalid app preference fails with a config error even though the
        // target path is whitelisted — nothing is spawned either way.
        let rejected = open_validated(&history, target, Some("relative/editor"));
        assert!(rejected.unwrap_err().contains("must be absolute"));
        // A blank app preference means the OS default app (only the history
        // allowlist applies; a non-recorded path still errors without spawn).
        let rejected = open_validated(&history, Path::new("/etc/passwd"), Some("   "));
        assert!(rejected
            .unwrap_err()
            .contains("not saved by a completed download"));
    }
}
