// macOS window layer: the island floats above the menu bar at the top center of the screen (hugging the notch
// on MacBooks that have one), shows on every Space and over fullscreen apps, and lets clicks pass through
// everywhere except over the island itself.
use crate::Rect;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSScreen, NSWindow, NSWindowCollectionBehavior};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::WebviewWindow;

// Transparent window sized for the largest island state (same layout as on Windows)
pub const W: f64 = 640.0;
pub const H: f64 = 780.0;

static RECT_JS: Mutex<Option<Rect>> = Mutex::new(None);
pub static DRAGGING: AtomicBool = AtomicBool::new(false);   // a drag out of the shelf is in progress
static KBD: AtomicBool = AtomicBool::new(false);            // search box has keyboard focus
static NSWIN: AtomicUsize = AtomicUsize::new(0);            // the island's NSWindow*
static PREV_APP: AtomicI32 = AtomicI32::new(0);             // app that was frontmost before we got activated

/// Geometry in global "top-left" points (CoreGraphics space): window frame, target screen frame
#[derive(Clone, Copy, Default)]
struct Geo { win: (f64, f64, f64, f64), mon: (f64, f64, f64, f64) }
static GEO: Mutex<Geo> = Mutex::new(Geo { win: (0.0, 0.0, W, H), mon: (0.0, 0.0, 1440.0, 900.0) });

/// Notch of the target screen (points): (width, height); None on displays without one
static NOTCH: Mutex<Option<(f64, f64, f64)>> = Mutex::new(None);   // (w, h, menu bar h)
static MENUBAR_H: Mutex<f64> = Mutex::new(24.0);

pub fn set_rect(r: Rect) { *RECT_JS.lock().unwrap() = Some(r); }

/// Window level just above the menu bar (NSMainMenuWindowLevel = 24) so the island can sit inside the notch area
const ISLAND_LEVEL: isize = 24 + 3;

fn ns_window(mtm: MainThreadMarker) -> Option<&'static NSWindow> {
    let _ = mtm;
    let p = NSWIN.load(Ordering::Relaxed);
    if p == 0 { None } else { Some(unsafe { &*(p as *const NSWindow) }) }
}

fn pick_screen(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    let screens = NSScreen::screens(mtm);
    let idx = crate::settings::get().monitor_index;
    if idx > 0 && idx < screens.count() { return Some(screens.objectAtIndex(idx)); }
    if screens.count() > 0 { Some(screens.objectAtIndex(0)) } else { None }
}

/// Notch metrics of a screen (macOS 12+ API, present on every system we support)
fn screen_notch(scr: &NSScreen) -> Option<(f64, f64)> {
    let insets = scr.safeAreaInsets();
    if insets.top <= 0.0 { return None; }
    let l = scr.auxiliaryTopLeftArea();
    let r = scr.auxiliaryTopRightArea();
    let w = scr.frame().size.width - l.size.width - r.size.width;
    if w <= 0.0 { return None; }
    Some((w, insets.top))
}

/// List of connected displays for the Settings page (NSScreen order: 0 = the one with the menu bar)
pub fn list_monitors() -> Vec<serde_json::Value> {
    crate::macos::on_main(|mtm| {
        let screens = NSScreen::screens(mtm);
        (0..screens.count()).map(|i| {
            let s = screens.objectAtIndex(i);
            let f = s.frame();
            let sf = s.backingScaleFactor();
            let name = s.localizedName().to_string();
            json!({
                "index": i,
                "name": if name.is_empty() { format!("{}", i + 1) } else { name },
                "width": (f.size.width * sf).round() as u32,
                "height": (f.size.height * sf).round() as u32,
                "primary": i == 0,
                "notch": screen_notch(&s).is_some(),
            })
        }).collect()
    }).unwrap_or_default()
}

/// Notch info for the UI: the idle island grows to cover the notch and sticks to the top edge
pub fn notch_info() -> serde_json::Value {
    let n = *NOTCH.lock().unwrap();
    let mb = *MENUBAR_H.lock().unwrap();
    match n {
        Some((w, h, _)) => json!({ "present": true, "width": w, "height": h, "menuBar": mb }),
        None => json!({ "present": false, "width": 0, "height": 0, "menuBar": mb }),
    }
}

pub fn place_now() { let _ = crate::macos::on_main(place); }

fn place(mtm: MainThreadMarker) {
    let Some(win) = ns_window(mtm) else { return };
    let Some(scr) = pick_screen(mtm) else { return };
    let screens = NSScreen::screens(mtm);
    let primary_h = if screens.count() > 0 { screens.objectAtIndex(0).frame().size.height } else { 900.0 };
    let f = scr.frame();
    let vis = scr.visibleFrame();
    let (w, h) = (W.min(f.size.width), H.min(f.size.height));
    let x = f.origin.x + (f.size.width - w) / 2.0;
    let test = crate::util::is_test();
    let y = if test { f.origin.y + (f.size.height - h) / 2.0 } else { f.origin.y + f.size.height - h };   // Cocoa: bottom-left origin
    win.setFrame_display(NSRect::new(NSPoint::new(x.round(), y.round()), NSSize::new(w, h)), true);

    let top_tl = primary_h - (y + h);
    let mon_tl = primary_h - (f.origin.y + f.size.height);
    *GEO.lock().unwrap() = Geo { win: (x, top_tl, w, h), mon: (f.origin.x, mon_tl, f.size.width, f.size.height) };
    let menubar = (f.origin.y + f.size.height) - (vis.origin.y + vis.size.height);
    *MENUBAR_H.lock().unwrap() = if menubar > 0.0 { menubar } else { 24.0 };
    let notch = screen_notch(&scr).map(|(nw, nh)| (nw, nh, menubar));
    let changed = *NOTCH.lock().unwrap() != notch;
    *NOTCH.lock().unwrap() = notch;
    if changed { crate::util::emit("notch", notch_info()); }
}

pub fn setup(win: &WebviewWindow) {
    if let Ok(p) = win.ns_window() { NSWIN.store(p as usize, Ordering::Relaxed); }
    if let Some(mtm) = MainThreadMarker::new() {
        if let Some(w) = ns_window(mtm) {
            w.setLevel(ISLAND_LEVEL);
            // every Space + over fullscreen apps, never in ⌘` cycling or Mission Control
            w.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::IgnoresCycle);
            w.setHidesOnDeactivate(false);
            w.setCanHide(false);              // ⌘H / "Hide Others" never hides the island
            w.setHasShadow(false);
            w.setAnimationBehavior(objc2_app_kit::NSWindowAnimationBehavior::None);
        }
        place(mtm);
    }
    let _ = win.set_ignore_cursor_events(true);

    // Screen changes (resolution, display plugged in/out, Dock/menu bar) + fullscreen check + remembering the frontmost app
    let w2 = win.clone();
    std::thread::spawn(move || {
        let mut last_fs_hide = false;
        let mut last_sig = String::new();
        let my_pid = std::process::id() as i32;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            // display configuration signature: re-place only when something changed
            let sig = crate::macos::on_main(|mtm| {
                let s = NSScreen::screens(mtm);
                let mut sig = format!("{}|{}", s.count(), crate::settings::get().monitor_index);
                for i in 0..s.count() { let f = s.objectAtIndex(i).frame(); sig += &format!("|{},{},{},{}", f.origin.x, f.origin.y, f.size.width, f.size.height); }
                sig
            }).unwrap_or_default();
            if sig != last_sig { last_sig = sig; place_now(); crate::settings::broadcast_usage(); }

            if let Some(pid) = crate::macos::frontmost_pid() {
                if pid != my_pid && pid > 0 { PREV_APP.store(pid, Ordering::Relaxed); }
            }

            // Auto-hide over fullscreen apps / videos (comes back when the cursor touches the top edge)
            let auto_hide = crate::settings::get().auto_hide_fullscreen;
            let geo = *GEO.lock().unwrap();
            let fs = auto_hide && is_front_fullscreen(geo.mon, my_pid);
            let (cx, cy) = cursor();
            let (mx, my, mw, _) = geo.mon;
            let top_peek = cy <= my + 6.0 && cx >= mx + mw / 2.0 - 240.0 && cx <= mx + mw / 2.0 + 240.0;
            let should_hide = fs && !top_peek;
            if should_hide != last_fs_hide {
                last_fs_hide = should_hide;
                crate::util::emit("fullscreen", json!({ "hidden": should_hide }));
            }
            let _ = &w2;
        }
    });

    // Is the cursor over the island? If not, clicks pass through to the apps below.
    // While a file is dragged in from Finder, the window becomes interactive early so the drop is never missed.
    let w3 = win.clone();
    std::thread::spawn(move || {
        let mut interactive = false;
        let mut ldown_start: Option<(f64, f64, bool)> = None;
        let mut ext_dragging = false;
        let mut was_in_drop = false;
        let mut was_down = false;
        let mut drop_grace: Option<Instant> = None;
        let mut sleep_ms = 25u64;
        loop {
            std::thread::sleep(Duration::from_millis(sleep_ms));
            let mut near_top = true;
            let lbtn = left_button_down();
            let want = if DRAGGING.load(Ordering::Relaxed) {
                ldown_start = None; ext_dragging = false; was_in_drop = false; drop_grace = None;
                true
            } else {
                let (px, py) = cursor();
                let geo = *GEO.lock().unwrap();
                let x = px - geo.win.0;
                let y = py - geo.win.1;
                near_top = lbtn || interactive || (y <= 260.0 && x >= -60.0 && x <= 700.0);
                if !lbtn {
                    if was_in_drop { drop_grace = Some(Instant::now() + Duration::from_millis(350)); }
                    ldown_start = None; ext_dragging = false; was_in_drop = false;
                } else if let Some((sx, sy, outside)) = ldown_start {
                    if outside && !ext_dragging && ((px - sx).abs() > 6.0 || (py - sy).abs() > 6.0) { ext_dragging = true; }
                } else {
                    ldown_start = Some((px, py, !interactive));
                }
                let in_drop_zone = ext_dragging && (40.0..=560.0).contains(&x) && (-12.0..=180.0).contains(&y);
                if in_drop_zone { was_in_drop = true; }
                let grace = drop_grace.map(|t| Instant::now() < t).unwrap_or(false);
                let in_rect = match *RECT_JS.lock().unwrap() {
                    None => false,
                    Some(r) => {
                        let pad = if interactive { 10.0 } else { 4.0 };
                        x >= r.x - pad && x <= r.x + r.w + pad && y >= r.y - pad && y <= r.y + r.h + pad
                    }
                };
                in_drop_zone || grace || in_rect
            };
            // A click on the island activates our app: hand the focus back to the previous app right after the
            // click (keeps typing / shortcuts going to the app the user was working in).
            if was_down && !lbtn && interactive { schedule_focus_restore(); }
            was_down = lbtn;
            if want != interactive {
                interactive = want;
                let _ = w3.set_ignore_cursor_events(!want);
                if !want { schedule_focus_restore(); }
            }
            sleep_ms = if near_top || interactive || ext_dragging { 25 } else { 90 };
        }
    });
}

/// Cursor position in global top-left points
fn cursor() -> (f64, f64) {
    unsafe {
        let ev = cg::CGEventCreate(std::ptr::null());
        if ev.is_null() { return (-1e6, -1e6); }
        let p = cg::CGEventGetLocation(ev);
        crate::macos::ffi::CFRelease(ev as *const _);
        (p.x, p.y)
    }
}

fn left_button_down() -> bool { unsafe { cg::CGEventSourceButtonState(0, 0) } }

mod cg {
    #[repr(C)]
    pub struct CGPoint { pub x: f64, pub y: f64 }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGEventCreate(src: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        pub fn CGEventGetLocation(ev: *mut std::ffi::c_void) -> CGPoint;
        pub fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    }
}

/// Is the frontmost normal window (not ours) covering the whole target screen? (fullscreen video, game, app)
fn is_front_fullscreen(mon: (f64, f64, f64, f64), my_pid: i32) -> bool {
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSString};
    unsafe {
        // kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements
        let raw = crate::macos::ffi::CGWindowListCopyWindowInfo(1 | 16, 0);
        let Some(list): Option<Retained<NSArray<NSDictionary<NSString, NSObject>>>> = Retained::from_raw(raw as *mut _) else { return false };
        let key = |s: &str| NSString::from_str(s);
        for d in list.to_vec() {
            let num = |k: &str| d.objectForKey(&key(k)).and_then(|o| o.downcast::<NSNumber>().ok()).map(|n| n.doubleValue());
            if num("kCGWindowLayer").unwrap_or(1.0) != 0.0 { continue; }
            if num("kCGWindowOwnerPID").map(|p| p as i32) == Some(my_pid) { continue; }
            let Some(b) = d.objectForKey(&key("kCGWindowBounds")).and_then(|o| o.downcast::<NSDictionary>().ok()) else { return false };
            let bv = |k: &str| { let kk = key(k); b.objectForKey(&kk)
                .and_then(|o| o.downcast::<NSNumber>().ok()).map(|n| n.doubleValue()).unwrap_or(0.0) };
            let (x, y, w, h) = (bv("X"), bv("Y"), bv("Width"), bv("Height"));
            // the first (= frontmost) normal window decides
            return x <= mon.0 + 1.0 && y <= mon.1 + 1.0 && w >= mon.2 - 1.0 && h >= mon.3 - 1.0;
        }
        false
    }
}

fn schedule_focus_restore() {
    if KBD.load(Ordering::Relaxed) { return; }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(140));
        if KBD.load(Ordering::Relaxed) { return; }
        let my_pid = std::process::id() as i32;
        if crate::macos::frontmost_pid() != Some(my_pid) { return; }
        // keep focus while the Settings window (or a menu) is in use
        let island_is_key = crate::macos::on_main(|mtm| {
            let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
            match app.keyWindow() {
                None => true,
                Some(k) => Retained::as_ptr(&k) as usize == NSWIN.load(Ordering::Relaxed),
            }
        }).unwrap_or(false);
        let prev = PREV_APP.load(Ordering::Relaxed);
        if island_is_key && prev > 0 { crate::macos::activate_pid(prev); }
    });
}

/// Temporary keyboard focus for the search box
pub fn kbd_focus(win: &WebviewWindow, on: bool) {
    if on {
        KBD.store(true, Ordering::Relaxed);
        crate::macos::activate_self();
        let _ = win.set_focus();
    } else {
        KBD.store(false, Ordering::Relaxed);
        let prev = PREV_APP.load(Ordering::Relaxed);
        if prev > 0 && crate::macos::frontmost_pid() == Some(std::process::id() as i32) { crate::macos::activate_pid(prev); }
    }
}

/* ---------- single instance (advisory file lock) ---------- */
pub fn single_instance() -> bool {
    use std::os::unix::io::AsRawFd;
    let path = std::env::temp_dir().join("com.island.dynamic.lock");
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path) else { return true };
    let ok = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if ok { std::mem::forget(f); }   // keep the lock for the lifetime of the process
    ok
}

/* ---------- Open at Login ----------
   Inside an .app bundle: SMAppService.mainAppService (macOS 13+; shows up in System Settings > General > Login Items).
   Unbundled (cargo run): a LaunchAgent plist in ~/Library/LaunchAgents. */
const AGENT_LABEL: &str = "com.island.dynamic";

fn bundled() -> bool {
    std::env::current_exe().map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/")).unwrap_or(false)
}

fn agent_plist() -> std::path::PathBuf { crate::macos::home().join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist")) }

#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

fn sm_service() -> Option<Retained<objc2::runtime::AnyObject>> {
    use objc2::runtime::AnyClass;
    let cls = AnyClass::get(c"SMAppService")?;
    unsafe { objc2::msg_send![cls, mainAppService] }
}

pub fn autostart_enabled() -> bool {
    if bundled() {
        if let Some(svc) = sm_service() {
            let status: isize = unsafe { objc2::msg_send![&*svc, status] };
            return status == 1 || status == 2;   // enabled / waiting for approval in Login Items
        }
    }
    agent_plist().is_file()
}

pub fn set_autostart(on: bool) {
    if bundled() {
        if let Some(svc) = sm_service() {
            let null: *mut *mut objc2::runtime::AnyObject = std::ptr::null_mut();
            let ok: bool = unsafe {
                if on { objc2::msg_send![&*svc, registerAndReturnError: null] }
                else { objc2::msg_send![&*svc, unregisterAndReturnError: null] }
            };
            if ok { let _ = std::fs::remove_file(agent_plist()); return; }
        }
    }
    let p = agent_plist();
    if on {
        let Ok(exe) = std::env::current_exe() else { return };
        let xml = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{AGENT_LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict></plist>
"#, exe.display());
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let _ = std::fs::write(p, xml);
    } else {
        let _ = std::fs::remove_file(p);
    }
}
