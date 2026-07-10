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

/// Best-effort focus of the window hosting an agent: the first visible
/// top-level window whose title contains `query` (IDE windows and terminal
/// tabs usually carry the project folder name). Returns false when nothing
/// matched, so the UI can fall back to doing nothing rather than guessing.
#[tauri::command]
pub fn focus_agent(query: String) -> bool {
    #[cfg(windows)]
    return focus_matching_window(&query);
    #[cfg(not(windows))]
    {
        let _ = query;
        false
    }
}

#[cfg(windows)]
fn focus_matching_window(query: &str) -> bool {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, IsIconic, IsWindowVisible,
        SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    struct Search {
        needle: String,
        found: HWND,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> i32 {
        let search = &mut *(lparam as *mut Search);
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len == 0 {
            return 1;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let read = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        let title = String::from_utf16_lossy(&buf[..read.max(0) as usize]).to_lowercase();
        // Skip our own pill window.
        if title.contains(&search.needle) && title != "twinagent" {
            search.found = hwnd;
            return 0; // stop enumerating
        }
        1
    }

    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return false;
    }
    let mut search = Search {
        needle,
        found: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(Some(visit), &mut search as *mut Search as LPARAM);
        if search.found.is_null() {
            return false;
        }
        if IsIconic(search.found) != 0 {
            ShowWindow(search.found, SW_RESTORE);
        }
        SetForegroundWindow(search.found) != 0
    }
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
