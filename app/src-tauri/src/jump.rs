//! Click-to-jump deep links (TWI-18): from a card's working directory to
//! an editor or terminal on this machine, translating WSL paths where
//! needed. The widget always runs on the Windows side, so "jump" means
//! launching Windows Terminal or VS Code pointed at the right place —
//! possibly inside WSL.

/// Distro used for `\\wsl.localhost` UNC paths and `vscode-remote` URIs.
/// A single-distro assumption is fine for Phase 1.
fn wsl_distro() -> String {
    std::env::var("TWIN_WSL_DISTRO").unwrap_or_else(|_| "Ubuntu".into())
}

/// `app` is `"code"` or `"terminal"`; `dir` is the session's cwd as seen on
/// `machine` (a POSIX path for WSL sessions, a drive path for Windows ones).
#[tauri::command]
pub fn jump(app: String, machine: String, dir: String) -> Result<(), String> {
    let wsl = machine != "windows" && dir.starts_with('/');
    match app.as_str() {
        "terminal" => {
            let path = if wsl {
                format!("\\\\wsl.localhost\\{}{}", wsl_distro(), dir.replace('/', "\\"))
            } else {
                dir
            };
            std::process::Command::new("wt.exe")
                .args(["-d", &path])
                .spawn()
                .map_err(|e| format!("wt.exe: {e}"))?;
        }
        "code" => {
            let uri = if wsl {
                format!("vscode://vscode-remote/wsl+{}{}", wsl_distro(), dir)
            } else {
                format!("vscode://file/{}", dir.replace('\\', "/"))
            };
            // explorer.exe dispatches the URI to the registered handler —
            // no shell quoting, no extra plugin.
            std::process::Command::new("explorer.exe")
                .arg(&uri)
                .spawn()
                .map_err(|e| format!("explorer.exe: {e}"))?;
        }
        other => return Err(format!("unknown jump app: {other}")),
    }
    Ok(())
}
