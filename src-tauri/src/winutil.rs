// Window: top center, always on top, never takes focus, click-through unless the cursor is over the island
use crate::Rect;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::WindowsAndMessaging::*;

// Transparent window sized for the largest island state (115% scale + 48 px top offset + lyrics/queue panel + shelf)
// plus the download ring on the left. Clicks outside the island pass through to the apps below.
pub const W: f64 = 640.0;
pub const H: f64 = 780.0;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static RECT_JS: Mutex<Option<Rect>> = Mutex::new(None);
pub static DRAGGING: AtomicBool = AtomicBool::new(false);   // a drag out of the shelf is in progress

fn hwnd() -> HWND { HWND(HWND_RAW.load(Ordering::Relaxed) as *mut _) }

pub fn set_rect(r: Rect) { *RECT_JS.lock().unwrap() = Some(r); }

fn set_noactivate(on: bool) {
    unsafe {
        let h = hwnd();
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let flags = (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize;
        let new = if on { ex | flags } else { (ex & !(WS_EX_NOACTIVATE.0 as isize)) | WS_EX_TOOLWINDOW.0 as isize };
        if new != ex { SetWindowLongPtrW(h, GWL_EXSTYLE, new); }
    }
}

/// Is the foreground window a fullscreen game or video?
fn is_foreground_fullscreen(mon_x: i32, mon_y: i32, mon_w: i32, mon_h: i32) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == hwnd() {
            return false;
        }
        // The desktop (Progman / WorkerW) and the taskbar (Shell_TrayWnd) do not count as fullscreen
        let mut cls = [0u16; 64];
        let len = GetClassNameW(fg, &mut cls) as usize;
        let cname = String::from_utf16_lossy(&cls[..len]);
        if matches!(cname.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        r.left <= mon_x && r.top <= mon_y && (r.right - r.left) >= mon_w && (r.bottom - r.top) >= mon_h
    }
}

fn pick_monitor(win: &WebviewWindow) -> Option<tauri::Monitor> {
    let idx = crate::settings::get().monitor_index;
    if idx > 0 {
        if let Ok(list) = win.available_monitors() {
            if let Some(m) = list.into_iter().nth(idx) {
                return Some(m);
            }
        }
    }
    win.primary_monitor().ok().flatten()
}

/// List of connected displays for the Settings page
pub fn list_monitors() -> Vec<serde_json::Value> {
    use tauri::Manager;
    let Some(win) = crate::util::app().get_webview_window("main") else { return vec![] };
    let primary_pos = win.primary_monitor().ok().flatten().map(|m| (m.position().x, m.position().y));
    let Ok(list) = win.available_monitors() else { return vec![] };
    list.into_iter().enumerate().map(|(i, m)| {
        let is_prim = primary_pos == Some((m.position().x, m.position().y));
        let raw_name = m.name().map(|s| s.as_str()).unwrap_or("");
        let clean = raw_name.trim_start_matches(r"\\.\");
        serde_json::json!({
            "index": i,
            "name": if clean.is_empty() { format!("{}", i + 1) } else { clean.to_string() },
            "width": m.size().width,
            "height": m.size().height,
            "primary": is_prim,
        })
    }).collect()
}

pub fn place_now() {
    use tauri::Manager;
    if let Some(win) = crate::util::app().get_webview_window("main") {
        place(&win);
    }
}

fn place(win: &WebviewWindow) {
    let Some(mon) = pick_monitor(win) else { return };
    let sf = mon.scale_factor();
    let (mx, my) = (mon.position().x as f64, mon.position().y as f64);
    let (mw, mh) = (mon.size().width as f64, mon.size().height as f64);
    let (w, h) = ((W * sf).min(mw), (H * sf).min(mh));   // never larger than the screen on small displays
    let y = if crate::util::is_test() { my + (mh - h) / 2.0 } else { my };
    let _ = win.set_size(PhysicalSize::new(w.round() as u32, h.round() as u32));
    let _ = win.set_position(PhysicalPosition::new((mx + (mw - w) / 2.0).round() as i32, y.round() as i32));
}

pub fn setup(win: &WebviewWindow) {
    if let Ok(h) = win.hwnd() { HWND_RAW.store(h.0 as isize, Ordering::Relaxed); }
    set_noactivate(true);
    place(win);
    let _ = win.set_ignore_cursor_events(true);

    // Reposition when displays change + periodically bring to top + fullscreen game/video check
    let w2 = win.clone();
    std::thread::spawn(move || {
        let mut last = (0u32, 0u32, usize::MAX);
        let mut last_fs_hide = false;
        let mut tick = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            tick = tick.wrapping_add(1);
            let mut mon_rect = (0i32, 0i32, 1920i32, 1080i32);
            let mon_idx = crate::settings::get().monitor_index;
            if let Some(m) = pick_monitor(&w2) {
                let cur = (m.size().width, m.size().height, mon_idx);
                mon_rect = (m.position().x, m.position().y, cur.0 as i32, cur.1 as i32);
                if cur != last { last = cur; place(&w2); }
            }
            // Auto-hide during fullscreen games or videos (slides back down when the mouse touches the top edge)
            let auto_hide = crate::settings::get().auto_hide_fullscreen;
            let fs = auto_hide && is_foreground_fullscreen(mon_rect.0, mon_rect.1, mon_rect.2, mon_rect.3);
            let top_peek = unsafe {
                let mut pt = POINT::default();
                GetCursorPos(&mut pt).is_ok()
                    && pt.y <= mon_rect.1 + 6
                    && pt.x >= mon_rect.0 + (mon_rect.2 / 2) - 240
                    && pt.x <= mon_rect.0 + (mon_rect.2 / 2) + 240
            };
            let should_hide = fs && !top_peek;
            if should_hide != last_fs_hide {
                last_fs_hide = should_hide;
                crate::util::emit("fullscreen", serde_json::json!({ "hidden": should_hide }));
            }
            if tick % 6 == 0 {
                unsafe {
                    let _ = SetWindowPos(hwnd(), Some(HWND_TOPMOST), 0, 0, 0, 0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
                }
            }
        }
    });

    // Is the cursor over the island? If not, window clicks pass through to the apps below.
    // Also, while the left button is held and a drag approaches the island,
    // the window becomes interactive in advance, so OLE DragEnter is never missed.
    let w3 = win.clone();
    std::thread::spawn(move || {
        let mut interactive = false;
        let mut ldown_start: Option<(i32, i32, bool)> = None; // (x, y, started_outside)
        let mut ext_dragging = false;
        let mut was_in_drop = false;
        let mut drop_grace: Option<std::time::Instant> = None;
        let mut sleep_ms = 25u64;
        loop {
            std::thread::sleep(Duration::from_millis(sleep_ms));
            let mut near_top = true;
            let want = if DRAGGING.load(Ordering::Relaxed) {
                ldown_start = None;
                ext_dragging = false;
                was_in_drop = false;
                drop_grace = None;
                true
            } else {
                unsafe {
                    let mut pt = POINT::default();
                    let mut wr = RECT::default();
                    if GetCursorPos(&mut pt).is_err() || GetWindowRect(hwnd(), &mut wr).is_err() {
                        false
                    } else {
                        let sf = (GetDpiForWindow(hwnd()) as f64 / 96.0).max(1.0);
                        let x = (pt.x - wr.left) as f64 / sf;
                        let y = (pt.y - wr.top) as f64 / sf;
                        let lbtn = (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0;
                        near_top = lbtn || interactive || (y <= 260.0 && x >= -60.0 && x <= 700.0);
                        if !lbtn {
                            if was_in_drop {
                                drop_grace = Some(std::time::Instant::now() + Duration::from_millis(350));
                            }
                            ldown_start = None;
                            ext_dragging = false;
                            was_in_drop = false;
                        } else if let Some((sx, sy, outside)) = ldown_start {
                            if outside && !ext_dragging && ((pt.x - sx).abs() > 6 || (pt.y - sy).abs() > 6) {
                                ext_dragging = true;
                            }
                        } else {
                            ldown_start = Some((pt.x, pt.y, !interactive));
                        }

                        // Large capture area around the island (520x180) while dragging in from outside
                        let in_drop_zone = ext_dragging && x >= 40.0 && x <= 560.0 && y >= -12.0 && y <= 180.0;
                        if in_drop_zone { was_in_drop = true; }
                        let grace = drop_grace.map(|t| std::time::Instant::now() < t).unwrap_or(false);

                        let r = *RECT_JS.lock().unwrap();
                        let in_rect = match r {
                            None => false,
                            Some(r) => {
                                let pad = if interactive { 10.0 } else { 4.0 };
                                x >= r.x - pad && x <= r.x + r.w + pad && y >= r.y - pad && y <= r.y + r.h + pad
                            }
                        };
                        in_drop_zone || grace || in_rect
                    }
                }
            };
            if want != interactive {
                interactive = want;
                let _ = w3.set_ignore_cursor_events(!want);
            }
            sleep_ms = if near_top || interactive || ext_dragging { 25 } else { 90 };
        }
    });
}

/// Temporary keyboard focus for the search box
pub fn kbd_focus(win: &WebviewWindow, on: bool) {
    if on {
        set_noactivate(false);
        unsafe { let _ = SetForegroundWindow(hwnd()); }
        let _ = win.set_focus();
    } else {
        set_noactivate(true);
    }
}

/* ---------- single instance ---------- */
pub fn single_instance() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    unsafe {
        let h = CreateMutexW(None, true, w!("Local\\DynamicIslandTauri"));
        // keep the handle open for the lifetime of the process
        std::mem::forget(h);
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/* ---------- start with Windows ---------- */
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "DynamicIsland";

pub fn autostart_enabled() -> bool {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    hkcu.open_subkey(RUN_KEY).and_then(|k| k.get_value::<String, _>(RUN_NAME)).is_ok()
}

pub fn set_autostart(on: bool) {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    if let Ok((k, _)) = hkcu.create_subkey(RUN_KEY) {
        if on {
            if let Ok(exe) = std::env::current_exe() { let _ = k.set_value(RUN_NAME, &format!("\"{}\"", exe.display())); }
        } else { let _ = k.delete_value(RUN_NAME); }
    }
}
