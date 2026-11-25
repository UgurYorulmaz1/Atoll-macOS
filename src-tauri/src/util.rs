// Shared helpers: app handle, event emitting, data folder, shell commands, local time
use std::path::PathBuf;
use std::sync::OnceLock;
use tauri::{AppHandle, Emitter, Manager};

static APP: OnceLock<AppHandle> = OnceLock::new();

pub fn set_app(app: AppHandle) { let _ = APP.set(app); }
pub fn app() -> &'static AppHandle { APP.get().expect("app not set") }

pub fn emit<S: serde::Serialize + Clone>(event: &str, payload: S) {
    if let Some(a) = APP.get() { let _ = a.emit(event, payload); }
}

pub fn is_test() -> bool { std::env::var("ISLAND_TEST").map(|v| v == "1").unwrap_or(false) }

/// Windows: %APPDATA%\com.island.dynamic, macOS: ~/Library/Application Support/com.island.dynamic (...-test in test mode)
pub fn data_dir() -> PathBuf {
    let base = app().path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("island"));
    let dir = if is_test() { PathBuf::from(format!("{}-test", base.display())) } else { base };
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Local wall-clock time: (year, month, day, hour, minute, second)
pub fn local_time() -> (i32, u32, u32, u32, u32, u32) {
    #[cfg(windows)]
    {
        let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        (st.wYear as i32, st.wMonth as u32, st.wDay as u32, st.wHour as u32, st.wMinute as u32, st.wSecond as u32)
    }
    #[cfg(unix)]
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_year + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32, tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
    }
}

#[cfg(windows)]
pub fn wide(s: &str) -> windows::core::HSTRING { windows::core::HSTRING::from(s) }

/// Open a file/URL with its default app
pub fn shell_open(target: &str) {
    #[cfg(windows)]
    {
        use windows::core::w;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        unsafe { let _ = ShellExecuteW(None, w!("open"), &wide(target), None, None, SW_SHOWNORMAL); }
    }
    #[cfg(target_os = "macos")]
    crate::macos::open_path_or_url(target);
}

/// Show the file selected in Explorer / Finder
pub fn show_in_folder(path: &str) {
    #[cfg(windows)]
    { let _ = std::process::Command::new("explorer.exe").arg(format!("/select,{}", path)).spawn(); }
    #[cfg(target_os = "macos")]
    crate::macos::reveal_in_finder(path);
}

/// Windows file time (100 ns since 1601) -> unix ms
#[cfg(windows)]
pub fn filetime_to_unix_ms(ut: i64) -> i64 { (ut - 116_444_736_000_000_000) / 10_000 }

pub fn unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
