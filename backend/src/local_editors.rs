//! Known external editors and "open with a specific app" support (parity
//! with dbx-plugin-ssh `local_editors.rs`).
//!
//! `files/local/editors/list` returns a per-platform catalog of well-known
//! editors (VS Code, Sublime, TextEdit/Notepad, WPS, MS Office, LibreOffice, …)
//! probed from the filesystem/PATH — never by spawning processes.
//! `files/local/open-with` launches a catalog entry or a user-defined command
//! line for a path that this plugin previously downloaded (the same
//! transfer-history allowlist as `files/local/open`), so it can never become
//! an arbitrary "execute anything" primitive.
//!
//! Launch always goes through an argument list (no shell), and user-provided
//! command strings are tokenized POSIX-shlex style with a Windows-friendly
//! twist: backslash is literal unless it escapes a quote or another backslash,
//! so `"C:\Program Files\App\app.exe" {file}` keeps its separators.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::local_downloads;
use crate::store::TransferRecord;

/// One well-known editor: how to launch it, plus whether the probe found it.
pub struct CatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub launch: LaunchSpec,
    pub available: bool,
    pub suggested_extensions: &'static [&'static str],
}

/// How an editor is started. `openApp` uses macOS `open -a`; `exe` spawns the
/// binary with an argument template where `{file}` is replaced by the
/// downloaded path (missing placeholder: path appended).
#[derive(Clone, Debug, PartialEq)]
pub enum LaunchSpec {
    OpenApp { app_id: String },
    Exe { exe: String, args: Vec<String> },
}

impl LaunchSpec {
    fn to_json(&self) -> Value {
        match self {
            LaunchSpec::OpenApp { app_id } => json!({ "kind": "openApp", "appId": app_id }),
            LaunchSpec::Exe { exe, args } => json!({ "kind": "exe", "exe": exe, "args": args }),
        }
    }
}

fn open_app(app_id: &str) -> LaunchSpec {
    LaunchSpec::OpenApp {
        app_id: app_id.to_string(),
    }
}

fn exe_spec(exe: Option<&Path>) -> LaunchSpec {
    LaunchSpec::Exe {
        exe: exe
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        args: vec!["{file}".to_string()],
    }
}

/// Everything the catalog probe needs from the machine, injected so tests can
/// fake installs without touching the real filesystem.
pub struct EnvPaths {
    pub local_appdata: Option<PathBuf>,
    pub program_files: Option<PathBuf>,
    pub program_files_x86: Option<PathBuf>,
    pub windows_dir: Option<PathBuf>,
    pub path_dirs: Vec<PathBuf>,
    /// macOS application folders, most specific last (`/Applications`, `~/Applications`).
    pub applications_dirs: Vec<PathBuf>,
}

/// Reads the real machine's environment into `EnvPaths`.
pub fn detect_env_paths() -> EnvPaths {
    let var = |key: &str| std::env::var_os(key).map(PathBuf::from);
    let home_dir = if cfg!(windows) {
        var("USERPROFILE")
    } else {
        var("HOME")
    };
    let mut path_dirs = Vec::new();
    if let Some(raw) = std::env::var_os("PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };
        for part in raw.to_string_lossy().split(separator) {
            if !part.trim().is_empty() {
                path_dirs.push(PathBuf::from(part));
            }
        }
    }
    let mut applications_dirs = vec![PathBuf::from("/Applications")];
    if let Some(home_dir) = &home_dir {
        applications_dirs.push(home_dir.join("Applications"));
    }
    EnvPaths {
        local_appdata: var("LOCALAPPDATA"),
        program_files: var("ProgramFiles"),
        program_files_x86: var("ProgramFiles(x86)"),
        windows_dir: var("SystemRoot").or_else(|| var("WINDIR")),
        path_dirs,
        applications_dirs,
    }
}

fn probe_app(env: &EnvPaths, probe: &dyn Fn(&Path) -> bool, app_dir_name: &str) -> bool {
    env.applications_dirs
        .iter()
        .any(|dir| probe(&dir.join(format!("{app_dir_name}.app"))))
}

fn probe_on_path(name: &str, env: &EnvPaths, probe: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    env.path_dirs
        .iter()
        .map(|dir| dir.join(name))
        .find(|candidate| probe(candidate))
}

/// Windows installers that nest a version directory (WPS Office lives in
/// `<root>\Kingsoft\WPS Office\<version>\office6\`): iterate the direct
/// children of `base` and return the first `child/tail` that exists.
fn probe_versioned(
    base: &Path,
    tail: &[&str],
    probe: &dyn Fn(&Path) -> bool,
    lister: &dyn Fn(&Path) -> Vec<PathBuf>,
) -> Option<PathBuf> {
    lister(base).into_iter().find_map(|child| {
        let candidate = tail.iter().fold(child, |acc, part| acc.join(part));
        probe(&candidate).then_some(candidate)
    })
}

fn first_existing(
    candidates: &[Option<PathBuf>],
    probe: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .iter()
        .flatten()
        .find(|candidate| probe(candidate))
        .cloned()
}

fn text_extensions() -> &'static [&'static str] {
    &[
        ".txt", ".md", ".log", ".json", ".xml", ".yaml", ".yml", ".ini", ".conf", ".toml", ".sh",
        ".py", ".js", ".ts", ".sql", ".env",
    ]
}

const OFFICE_EXTS: &[&str] = &[".odt", ".ods", ".odp", ".doc", ".docx", ".xls", ".xlsx"];

/// Builds the well-known editor catalog for `platform`. Pure over the
/// injected probe/lister, so every branch is unit-testable off-platform.
pub fn build_catalog(
    platform: &str,
    env: &EnvPaths,
    probe: &dyn Fn(&Path) -> bool,
    lister: &dyn Fn(&Path) -> Vec<PathBuf>,
) -> Vec<CatalogEntry> {
    let mut entries: Vec<CatalogEntry> = Vec::new();
    let mut push = |id: &'static str,
                    name: &'static str,
                    launch: LaunchSpec,
                    available: bool,
                    exts: &'static [&'static str]| {
        entries.push(CatalogEntry {
            id,
            name,
            launch,
            available,
            suggested_extensions: exts,
        });
    };

    match platform {
        "macos" => {
            let app = |dir: &str| probe_app(env, probe, dir);
            push(
                "vscode",
                "Visual Studio Code",
                open_app("Visual Studio Code"),
                app("Visual Studio Code"),
                text_extensions(),
            );
            push(
                "sublime",
                "Sublime Text",
                open_app("Sublime Text"),
                app("Sublime Text"),
                text_extensions(),
            );
            push(
                "textedit",
                "TextEdit",
                open_app("TextEdit"),
                app("TextEdit") || probe(&PathBuf::from("/System/Applications/TextEdit.app")),
                text_extensions(),
            );
            push(
                "wps",
                "WPS Office",
                open_app("wpsoffice"),
                app("wpsoffice"),
                &[".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx"],
            );
            push(
                "msword",
                "Microsoft Word",
                open_app("Microsoft Word"),
                app("Microsoft Word"),
                &[".doc", ".docx", ".rtf"],
            );
            push(
                "msexcel",
                "Microsoft Excel",
                open_app("Microsoft Excel"),
                app("Microsoft Excel"),
                &[".xls", ".xlsx", ".csv"],
            );
            push(
                "msppt",
                "Microsoft PowerPoint",
                open_app("Microsoft PowerPoint"),
                app("Microsoft PowerPoint"),
                &[".ppt", ".pptx"],
            );
            push(
                "libreoffice",
                "LibreOffice",
                open_app("LibreOffice"),
                app("LibreOffice"),
                OFFICE_EXTS,
            );
        }
        "windows" => {
            // CreateProcess cannot exec `code.cmd`, so only real .exe install
            // locations are probed (the two standard per-user/machine paths).
            let vscode = first_existing(
                &[
                    env.local_appdata
                        .as_ref()
                        .map(|d| d.join(r"Programs\Microsoft VS Code\Code.exe")),
                    env.program_files
                        .as_ref()
                        .map(|d| d.join(r"Microsoft VS Code\Code.exe")),
                ],
                probe,
            );
            push(
                "vscode",
                "Visual Studio Code",
                exe_spec(vscode.as_deref()),
                vscode.is_some(),
                text_extensions(),
            );
            let notepad = env
                .windows_dir
                .as_ref()
                .map(|d| d.join(r"System32\notepad.exe"))
                .filter(|path| probe(path));
            push(
                "notepad",
                "Notepad",
                exe_spec(notepad.as_deref()),
                notepad.is_some(),
                text_extensions(),
            );
            let notepadpp = first_existing(
                &[
                    env.program_files
                        .as_ref()
                        .map(|d| d.join(r"Notepad++\notepad++.exe")),
                    env.local_appdata
                        .as_ref()
                        .map(|d| d.join(r"Programs\Notepad++\notepad++.exe")),
                ],
                probe,
            );
            push(
                "notepadpp",
                "Notepad++",
                exe_spec(notepadpp.as_deref()),
                notepadpp.is_some(),
                text_extensions(),
            );
            let sublime = first_existing(
                &[
                    env.program_files
                        .as_ref()
                        .map(|d| d.join(r"Sublime Text\sublime_text.exe")),
                    env.local_appdata
                        .as_ref()
                        .map(|d| d.join(r"Programs\Sublime Text\sublime_text.exe")),
                ],
                probe,
            );
            push(
                "sublime",
                "Sublime Text",
                exe_spec(sublime.as_deref()),
                sublime.is_some(),
                text_extensions(),
            );
            // WPS Windows installs into a versioned directory:
            // <root>\Kingsoft\WPS Office\<version>\office6\{wps,et,wpp}.exe
            let wps_candidates = [
                env.local_appdata
                    .as_ref()
                    .map(|d| d.join(r"Kingsoft\WPS Office")),
                env.program_files_x86
                    .as_ref()
                    .map(|d| d.join(r"Kingsoft\WPS Office")),
            ];
            let wps_roots: Vec<&PathBuf> = wps_candidates.iter().flatten().collect();
            let wps = |binary: &'static str| -> Option<PathBuf> {
                wps_roots
                    .iter()
                    .find_map(|root| probe_versioned(root, &["office6", binary], probe, lister))
            };
            let writer = wps("wps.exe");
            push(
                "wps-writer",
                "WPS Writer",
                exe_spec(writer.as_deref()),
                writer.is_some(),
                &[".doc", ".docx", ".rtf"],
            );
            let sheet = wps("et.exe");
            push(
                "wps-spreadsheet",
                "WPS Spreadsheets",
                exe_spec(sheet.as_deref()),
                sheet.is_some(),
                &[".xls", ".xlsx", ".csv", ".et"],
            );
            let slides = wps("wpp.exe");
            push(
                "wps-presentation",
                "WPS Presentation",
                exe_spec(slides.as_deref()),
                slides.is_some(),
                &[".ppt", ".pptx", ".dps"],
            );
            let office = |binary: &'static str| -> Option<PathBuf> {
                first_existing(
                    &[
                        env.program_files
                            .as_ref()
                            .map(|d| d.join(format!(r"Microsoft Office\root\Office16\{binary}"))),
                        env.program_files_x86
                            .as_ref()
                            .map(|d| d.join(format!(r"Microsoft Office\root\Office16\{binary}"))),
                    ],
                    probe,
                )
            };
            let word = office("WINWORD.EXE");
            push(
                "msword",
                "Microsoft Word",
                exe_spec(word.as_deref()),
                word.is_some(),
                &[".doc", ".docx", ".rtf"],
            );
            let excel = office("EXCEL.EXE");
            push(
                "msexcel",
                "Microsoft Excel",
                exe_spec(excel.as_deref()),
                excel.is_some(),
                &[".xls", ".xlsx", ".csv"],
            );
            let ppt = office("POWERPNT.EXE");
            push(
                "msppt",
                "Microsoft PowerPoint",
                exe_spec(ppt.as_deref()),
                ppt.is_some(),
                &[".ppt", ".pptx"],
            );
            let libre = first_existing(
                &[env
                    .program_files
                    .as_ref()
                    .map(|d| d.join(r"LibreOffice\program\soffice.exe"))],
                probe,
            );
            push(
                "libreoffice",
                "LibreOffice",
                exe_spec(libre.as_deref()),
                libre.is_some(),
                OFFICE_EXTS,
            );
        }
        // Linux/other: PATH lookup, direct argv launch (no shell).
        _ => {
            let mut cli = |id: &'static str,
                           name: &'static str,
                           binary: &'static str,
                           exts: &'static [&'static str]| {
                let found = probe_on_path(binary, env, probe);
                // 非 Windows 平台的 exe 展示路径统一 POSIX 分隔符：Path::join
                // 的分隔符随构建宿主变化（Windows 上 join 会往 POSIX 风格目录
                // 里插 '\'）。探测用原始 join 结果，落进 payload 前归一化。
                let normalized =
                    found.map(|path| PathBuf::from(path.to_string_lossy().replace('\\', "/")));
                push(
                    id,
                    name,
                    exe_spec(normalized.as_deref()),
                    normalized.is_some(),
                    exts,
                );
            };
            cli("vscode", "Visual Studio Code", "code", text_extensions());
            cli("sublime", "Sublime Text", "subl", text_extensions());
            cli("gedit", "gedit", "gedit", text_extensions());
            cli("kate", "Kate", "kate", text_extensions());
            cli("libreoffice", "LibreOffice", "soffice", OFFICE_EXTS);
            cli("wps-writer", "WPS Writer", "wps", &[".doc", ".docx"]);
            cli(
                "wps-spreadsheet",
                "WPS Spreadsheets",
                "et",
                &[".xls", ".xlsx", ".csv", ".et"],
            );
            cli(
                "wps-presentation",
                "WPS Presentation",
                "wpp",
                &[".ppt", ".pptx"],
            );
        }
    }
    entries
}

/// Real directory lister for the versioned probes (only direct subdirs).
pub fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    // Latest version first: version names sort lexicographically the way
    // installers bump them (11.x.y); a stable order also keeps tests sane.
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    dirs
}

/// `files/local/editors/list` payload: platform plus the full catalog with
/// per-entry availability. Unavailable entries are still listed so a stale
/// preference stays resolvable after the editor is installed later.
pub fn editors_list_payload() -> Value {
    let platform = local_downloads::platform_name();
    let env = detect_env_paths();
    let entries = build_catalog(platform, &env, &|path| path.exists(), &list_subdirs);
    let editors: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "name": entry.name,
                "available": entry.available,
                "launch": entry.launch.to_json(),
                "suggestedExtensions": entry.suggested_extensions,
            })
        })
        .collect();
    json!({ "platform": platform, "editors": editors })
}

/// Splits a user-provided command line into argv tokens. POSIX-shlex style
/// with one Windows-friendly deviation: outside quotes a backslash is literal
/// unless it escapes a quote or another backslash (so `C:\Tools\app.exe`
/// survives unquoted and `"C:\Program Files\..."` keeps its separators).
/// Unterminated quotes are an error, not silently swallowed.
pub fn tokenize_command(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' | '\r' => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '"' | '\'' => {
                has_token = true;
                let quote = ch;
                loop {
                    match chars.next() {
                        None => return Err("Command has an unterminated quote".to_string()),
                        Some(c) if c == quote => break,
                        Some('\\') if quote == '"' => match chars.peek() {
                            Some('\\') | Some('"') => {
                                current.push(chars.next().expect("peeked"));
                            }
                            _ => current.push('\\'),
                        },
                        Some(c) => current.push(c),
                    }
                }
            }
            '\\' => match chars.peek() {
                Some('\\') => {
                    chars.next();
                    current.push('\\');
                    has_token = true;
                }
                Some('"') | Some('\'') => {
                    current.push(chars.next().expect("peeked"));
                    has_token = true;
                }
                _ => {
                    current.push('\\');
                    has_token = true;
                }
            },
            _ => {
                current.push(ch);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Replaces every `{file}` placeholder with `path`; when the template has no
/// placeholder the path is appended, so `code` alone still opens the file.
pub fn substitute_file(args: &[String], path: &Path) -> Vec<String> {
    let text = path.to_string_lossy().into_owned();
    let substituted: Vec<String> = args
        .iter()
        .map(|arg| arg.replace("{file}", &text))
        .collect();
    if args.iter().any(|arg| arg.contains("{file}")) {
        substituted
    } else {
        substituted
            .into_iter()
            .chain(std::iter::once(text))
            .collect()
    }
}

/// Builds the launch spec for a `files/local/open-with` request: a catalog
/// `editorId`, or a user-defined `custom` command string.
pub fn launch_spec_from_params(params: &Value) -> Result<LaunchSpec, String> {
    if let Some(id) = params.get("editorId").and_then(Value::as_str) {
        let platform = local_downloads::platform_name();
        let env = detect_env_paths();
        let entry = build_catalog(platform, &env, &|path| path.exists(), &list_subdirs)
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| format!("Unknown editor id: {id}"))?;
        if !entry.available {
            return Err(format!(
                "Editor '{}' is not installed on this machine",
                entry.name
            ));
        }
        return Ok(entry.launch);
    }
    if let Some(custom) = params.get("custom") {
        let command = custom
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "Custom editor requires a non-empty command".to_string())?;
        let mut tokens = tokenize_command(command)?;
        let exe = tokens.remove(0);
        return Ok(LaunchSpec::Exe { exe, args: tokens });
    }
    Err("editorId or custom is required".to_string())
}

/// Same allowlist as `files/local/open` (a path recorded by a completed
/// download), then hands off to the injected launcher. `open_with_validated_with`
/// plugs in the real spawner; tests inject a recorder.
pub fn open_with_validated_with(
    history: &[TransferRecord],
    path: &Path,
    spec: &LaunchSpec,
    launcher: &dyn Fn(&Path, &LaunchSpec) -> Result<(), String>,
) -> Result<(), String> {
    if !local_downloads::is_recorded_download(history, path) {
        return Err("Path was not saved by a completed download of this plugin".to_string());
    }
    launcher(path, spec)
}

/// Production launcher: `open -a <app> <file>` on macOS, otherwise direct
/// argv spawn with `{file}` substitution. Never a shell.
pub fn launch_editor(path: &Path, spec: &LaunchSpec) -> Result<(), String> {
    if !path.is_file() {
        return Err("Downloaded file no longer exists".to_string());
    }
    match spec {
        LaunchSpec::OpenApp { app_id } => {
            if !cfg!(target_os = "macos") {
                return Err("openApp launch is only supported on macOS".to_string());
            }
            std::process::Command::new("open")
                .arg("-a")
                .arg(app_id)
                .arg(path)
                .spawn()
                .map(|_| ())
                .map_err(|error| format!("Failed to launch '{app_id}': {error}"))
        }
        LaunchSpec::Exe { exe, args } => {
            if exe.is_empty() {
                return Err("Editor executable path is empty".to_string());
            }
            std::process::Command::new(exe)
                .args(substitute_file(args, path))
                .spawn()
                .map(|_| ())
                .map_err(|error| format!("Failed to launch editor: {error}"))
        }
    }
}

/// `files/local/open-with`: allowlist check + real launch.
pub fn open_with_validated(
    history: &[TransferRecord],
    path: &Path,
    spec: &LaunchSpec,
) -> Result<(), String> {
    open_with_validated_with(history, path, spec, &launch_editor)
}

/// Resolves a catalog editor id into a launch spec for the remote-edit
/// channel (`files/remote-edit/open` `editorId` param): same catalog and
/// availability gate as `launch_spec_from_params`.
pub fn editor_launch_spec(editor_id: &str) -> Result<LaunchSpec, String> {
    let platform = local_downloads::platform_name();
    let env = detect_env_paths();
    let entry = build_catalog(platform, &env, &|path| path.exists(), &list_subdirs)
        .into_iter()
        .find(|entry| entry.id == editor_id)
        .ok_or_else(|| format!("Unknown editor id: {editor_id}"))?;
    if !entry.available {
        return Err(format!(
            "Editor '{}' is not installed on this machine",
            entry.name
        ));
    }
    Ok(entry.launch)
}

/// Resolves a user-defined command line into `(exe, args)` for the
/// remote-edit channel (`customCommand` param). The first token is the
/// executable, the rest are `{file}`-template arguments.
pub fn custom_launch_spec(command: &str) -> Result<(String, Vec<String>), String> {
    let tokens = tokenize_command(command)?;
    // tokenize_command never yields empty tokens, so an empty input is the
    // only way to end up without an executable.
    let exe = tokens
        .first()
        .ok_or_else(|| "Custom command requires a non-empty command".to_string())?
        .clone();
    Ok((exe, tokens[1..].to_vec()))
}

/// The concrete launch for a remote-edit session copy: `(program, args)` or
/// `None` for "OS default app". Pure platform dispatch so every branch is
/// unit-testable off-platform:
/// - `app_id` set → macOS `open -a <app_id> <file>` (rejected elsewhere);
/// - `app` + non-empty `args` → direct argv spawn with `{file}` substitution;
/// - `app` alone → the shared `open_in_app` path rules (`open -a` for macOS
///   bundles, direct spawn elsewhere);
/// - nothing set → `None` (OS default).
pub fn remote_edit_launch_command(
    local: &Path,
    app: Option<&Path>,
    args: &[String],
    app_id: Option<&str>,
) -> Result<Option<(std::ffi::OsString, Vec<std::ffi::OsString>)>, String> {
    if let Some(app_id) = app_id.map(str::trim).filter(|id| !id.is_empty()) {
        if local_downloads::platform_name() != "macos" {
            return Err("openApp launch is only supported on macOS".to_string());
        }
        return Ok(Some((
            std::ffi::OsString::from("open"),
            vec![
                std::ffi::OsString::from("-a"),
                std::ffi::OsString::from(app_id),
                local.as_os_str().to_os_string(),
            ],
        )));
    }
    if let Some(app) = app {
        if !args.is_empty() {
            let argv: Vec<std::ffi::OsString> = substitute_file(args, local)
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect();
            return Ok(Some((app.as_os_str().to_os_string(), argv)));
        }
        let (program, mut prefix) = local_downloads::app_launch_command(local_downloads::platform_name(), app);
        prefix.push(local.as_os_str().to_os_string());
        return Ok(Some((program, prefix)));
    }
    Ok(None)
}

/// Spawns the resolved remote-edit launch; `None` (no app configured) opens
/// the OS default application. Never a shell; the child is not awaited.
pub fn launch_remote_edit(
    local: &Path,
    app: Option<&Path>,
    args: &[String],
    app_id: Option<&str>,
) -> Result<(), String> {
    match remote_edit_launch_command(local, app, args, app_id)? {
        Some((program, argv)) => std::process::Command::new(&program)
            .args(&argv)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Failed to launch the chosen editor: {error}")),
        None => local_downloads::open_in_default_app(local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Separator-normalized probe: on a non-Windows host Path::join produces
    // '/' separators even for Windows-style relative parts, so both sides are
    // normalized to '/' before comparing.
    fn probe_from(paths: &[String]) -> impl Fn(&Path) -> bool + '_ {
        let wanted: Vec<String> = paths.iter().map(|p| p.replace('\\', "/")).collect();
        move |path: &Path| {
            let text = path.to_string_lossy().replace('\\', "/");
            wanted.contains(&text)
        }
    }

    fn lister_from(dirs: &[PathBuf]) -> impl Fn(&Path) -> Vec<PathBuf> + '_ {
        move |_dir: &Path| dirs.to_vec()
    }

    fn fake_env(apps: &[&str]) -> EnvPaths {
        EnvPaths {
            local_appdata: Some(PathBuf::from(r"C:\Users\u\AppData\Local")),
            program_files: Some(PathBuf::from(r"C:\Program Files")),
            program_files_x86: Some(PathBuf::from(r"C:\Program Files (x86)")),
            windows_dir: Some(PathBuf::from(r"C:\Windows")),
            path_dirs: vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")],
            applications_dirs: apps.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn macos_catalog_probes_applications() {
        let env = fake_env(&["/Applications"]);
        let app_paths = [
            "/Applications/Visual Studio Code.app".to_string(),
            "/Applications/wpsoffice.app".to_string(),
            "/System/Applications/TextEdit.app".to_string(),
        ];
        let probe = probe_from(&app_paths);
        let catalog = build_catalog("macos", &env, &probe, &lister_from(&[]));
        let by_id = |id: &str| catalog.iter().find(|e| e.id == id).expect("entry");
        assert!(by_id("vscode").available);
        assert!(by_id("wps").available);
        assert!(!by_id("msword").available);
        // TextEdit is always present via the system path.
        assert!(by_id("textedit").available);
        match &by_id("vscode").launch {
            LaunchSpec::OpenApp { app_id } => assert_eq!(app_id, "Visual Studio Code"),
            other => panic!("expected openApp, got {other:?}"),
        }
    }

    #[test]
    fn windows_catalog_resolves_literal_and_versioned_paths() {
        let env = fake_env(&[]);
        let win_paths = [
            r"C:\Users\u\AppData\Local\Programs\Microsoft VS Code\Code.exe".to_string(),
            r"C:\Users\u\AppData\Local\Kingsoft\WPS Office\11.1.0.12345\office6\wps.exe"
                .to_string(),
            r"C:\Program Files\Microsoft Office\root\Office16\WINWORD.EXE".to_string(),
        ];
        let probe = probe_from(&win_paths);
        // Real list_subdirs returns absolute children of the scanned dir.
        let lister = |dir: &Path| vec![dir.join("11.1.0.12345")];
        let catalog = build_catalog("windows", &env, &probe, &lister);
        let by_id = |id: &str| catalog.iter().find(|e| e.id == id).expect("entry");
        assert!(by_id("vscode").available);
        assert!(by_id("wps-writer").available);
        assert!(by_id("msword").available);
        assert!(!by_id("msexcel").available);
        match &by_id("wps-writer").launch {
            LaunchSpec::Exe { exe, args } => {
                let normalized = exe.replace('\\', "/");
                assert!(normalized.ends_with("office6/wps.exe"), "exe={exe}");
                assert_eq!(args, &["{file}"]);
            }
            other => panic!("expected exe, got {other:?}"),
        }
    }

    #[test]
    fn linux_catalog_probes_path() {
        let env = fake_env(&[]);
        let linux_paths = [
            "/usr/local/bin/code".to_string(),
            "/usr/bin/gedit".to_string(),
        ];
        let probe = probe_from(&linux_paths);
        let catalog = build_catalog("linux", &env, &probe, &lister_from(&[]));
        let by_id = |id: &str| catalog.iter().find(|e| e.id == id).expect("entry");
        assert!(by_id("vscode").available);
        let exe = match &by_id("vscode").launch {
            LaunchSpec::Exe { exe, .. } => exe.as_str(),
            _ => panic!("exe expected"),
        };
        assert_eq!(exe, "/usr/local/bin/code");
        assert!(!by_id("sublime").available);
    }

    #[test]
    fn editors_list_payload_has_shape() {
        let payload = editors_list_payload();
        let platform = payload["platform"].as_str().expect("platform");
        assert!(["macos", "windows", "linux", "other"].contains(&platform));
        let editors = payload["editors"].as_array().expect("editors array");
        assert!(!editors.is_empty());
        for editor in editors {
            assert!(editor["id"].as_str().is_some());
            assert!(editor["name"].as_str().is_some());
            assert!(editor["available"].is_boolean());
            assert!(editor["launch"].is_object());
            assert!(editor["suggestedExtensions"].is_array());
        }
    }

    #[test]
    fn tokenizer_handles_quotes_and_windows_paths() {
        assert_eq!(
            tokenize_command("code --wait {file}").expect("tokens"),
            ["code", "--wait", "{file}"]
        );
        // Windows path in quotes keeps its separators.
        assert_eq!(
            tokenize_command(r#""C:\Program Files\App\app.exe" -n {file}"#).expect("tokens"),
            [r"C:\Program Files\App\app.exe", "-n", "{file}"]
        );
        // Unquoted backslashes are literal (Windows-style).
        assert_eq!(
            tokenize_command(r"C:\Tools\app.exe {file}").expect("tokens")[0],
            r"C:\Tools\app.exe"
        );
        // Quoted escape only eats the quote/backslash itself.
        assert_eq!(tokenize_command(r#""a\"b""#).expect("tokens")[0], "a\"b");
        assert_eq!(tokenize_command("").expect("empty"), Vec::<String>::new());
        assert!(tokenize_command("\"unterminated").is_err());
        assert!(tokenize_command("'unterminated").is_err());
    }

    #[test]
    fn substitute_file_replaces_placeholder_or_appends() {
        let path = Path::new("/tmp/a b.txt");
        assert_eq!(
            substitute_file(&["--wait".into(), "{file}".into()], path),
            ["--wait", "/tmp/a b.txt"]
        );
        // No placeholder: path appended at the end.
        assert_eq!(
            substitute_file(&["-n".into()], path),
            ["-n", "/tmp/a b.txt"]
        );
        // Every occurrence is replaced.
        assert_eq!(
            substitute_file(&["{file}".into(), "-o".into(), "{file}".into()], path),
            ["/tmp/a b.txt", "-o", "/tmp/a b.txt"]
        );
    }

    #[test]
    fn launch_spec_from_params_resolves_catalog_and_custom() {
        // Unknown id is an error, not a silent fallback.
        assert!(launch_spec_from_params(&json!({ "editorId": "nope" })).is_err());
        // Custom command: first token is the exe, the rest are args.
        let spec = launch_spec_from_params(&json!({
            "custom": { "name": "Mine", "command": "code --wait {file}" }
        }))
        .expect("custom spec");
        match spec {
            LaunchSpec::Exe { exe, args } => {
                assert_eq!(exe, "code");
                assert_eq!(args, ["--wait", "{file}"]);
            }
            other => panic!("exe expected, got {other:?}"),
        }
        // Empty command is rejected.
        assert!(launch_spec_from_params(&json!({ "custom": { "command": "   " } })).is_err());
        // Neither id nor custom is an error.
        assert!(launch_spec_from_params(&json!({})).is_err());
    }

    #[test]
    fn open_with_requires_recorded_download() {
        let history = vec![record("t1", "download", "completed", Some("/Downloads/a.txt"))];
        let spec = LaunchSpec::Exe {
            exe: "true".into(),
            args: vec![],
        };
        let launched = std::cell::Cell::new(false);
        {
            let launcher = |_: &Path, _: &LaunchSpec| {
                launched.set(true);
                Ok(())
            };
            open_with_validated_with(&history, Path::new("/Downloads/a.txt"), &spec, &launcher)
                .expect("allowed");
        }
        assert!(launched.get());
        launched.set(false);
        let rejected =
            open_with_validated_with(&history, Path::new("/etc/passwd"), &spec, &|_, _| Ok(()));
        assert!(rejected
            .unwrap_err()
            .contains("not saved by a completed download"));
        assert!(!launched.get(), "rejected paths must not launch");
    }

    #[test]
    fn list_subdirs_sorts_versions_newest_first() {
        let base = tempfile::tempdir().expect("tempdir");
        for name in ["11.1.0.1", "12.0.0.9", "10.9.9.9"] {
            std::fs::create_dir_all(base.path().join(name)).expect("mkdir");
        }
        std::fs::write(base.path().join("loose.txt"), b"x").expect("file");
        let dirs = list_subdirs(base.path());
        let names: Vec<String> = dirs
            .iter()
            .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["12.0.0.9", "11.1.0.1", "10.9.9.9"]);
    }

    #[test]
    fn custom_launch_spec_splits_exe_and_args() {
        let (exe, args) = custom_launch_spec("code --wait {file}").expect("spec");
        assert_eq!(exe, "code");
        assert_eq!(args, ["--wait", "{file}"]);
        assert!(custom_launch_spec("   ").is_err());
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

    /// Remote-edit launch dispatch: app_id → open -a, app+args → argv with
    /// {file} substitution, app alone → shared app-path rules, none → default.
    #[test]
    fn remote_edit_launch_command_dispatches_by_channel() {
        let file = Path::new("/tmp/edit/a.txt");
        let macos = local_downloads::platform_name() == "macos";

        // app_id channel: `open -a` everywhere, rejected on non-macOS hosts.
        let by_id = remote_edit_launch_command(file, None, &[], Some("Visual Studio Code"));
        if macos {
            let (program, args) = by_id.expect("open -a").expect("some launch");
            assert_eq!(program, std::ffi::OsString::from("open"));
            assert_eq!(
                args,
                vec![
                    std::ffi::OsString::from("-a"),
                    std::ffi::OsString::from("Visual Studio Code"),
                    std::ffi::OsString::from("/tmp/edit/a.txt"),
                ]
            );
        } else {
            assert!(by_id.unwrap_err().contains("only supported on macOS"));
        }

        // app + args channel: direct argv, every {file} substituted.
        let app = Path::new("/usr/local/bin/code");
        let (program, args) =
            remote_edit_launch_command(file, Some(app), &["--wait".into(), "{file}".into()], None)
                .expect("argv spawn")
                .expect("some launch");
        assert_eq!(program, app.as_os_str().to_os_string());
        assert_eq!(
            args,
            vec![
                std::ffi::OsString::from("--wait"),
                std::ffi::OsString::from("/tmp/edit/a.txt"),
            ]
        );

        // app alone: platform rules (macOS `open -a <bundle>`, else the path).
        let (program, args) = remote_edit_launch_command(file, Some(app), &[], None)
            .expect("plain app")
            .expect("some launch");
        if macos {
            assert_eq!(program, std::ffi::OsString::from("open"));
            assert_eq!(args[0], std::ffi::OsString::from("-a"));
        } else {
            assert_eq!(program, app.as_os_str().to_os_string());
        }
        assert_eq!(args.last(), Some(&file.as_os_str().to_os_string()));

        // Nothing configured: the OS default app takes over.
        assert!(
            remote_edit_launch_command(file, None, &[], None)
                .expect("default")
                .is_none()
        );
    }
}
