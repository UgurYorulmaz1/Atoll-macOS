// macOS platform helpers shared by the macOS modules (AppKit / Foundation via objc2).
// Target: macOS 13 Ventura and later; newer APIs are used when the running system has them.
#![allow(dead_code)]

pub mod ffi;
pub mod hotkey;

use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSRunningApplication, NSWorkspace};
use objc2_foundation::{NSDictionary, NSString, NSURL};
use std::process::Command;
use std::sync::mpsc::channel;

/* ---------- threads ---------- */

/// Runs `f` on the AppKit main thread and returns its result (directly if we are already on it).
pub fn on_main<T: Send + 'static>(f: impl FnOnce(MainThreadMarker) -> T + Send + 'static) -> Option<T> {
    if let Some(mtm) = MainThreadMarker::new() { return Some(f(mtm)); }
    let (tx, rx) = channel();
    crate::util::app().run_on_main_thread(move || {
        let mtm = MainThreadMarker::new().expect("main thread");
        let _ = tx.send(f(mtm));
    }).ok()?;
    rx.recv().ok()
}

/* ---------- Finder / Launch Services ---------- */

pub fn open_path_or_url(target: &str) {
    let ws = NSWorkspace::sharedWorkspace();
    let url = if target.contains("://") {
        NSURL::URLWithString(&NSString::from_str(target))
    } else {
        Some(NSURL::fileURLWithPath(&NSString::from_str(target)))
    };
    if let Some(u) = url { let _ = ws.openURL(&u); }
}

pub fn reveal_in_finder(path: &str) {
    if path.is_empty() { return; }
    let _ = Command::new("/usr/bin/open").arg("-R").arg(path).spawn();
}

/// First entry of the user's preferred languages ("tr-TR", "en-US", "zh-Hans-CN" ...)
pub fn preferred_language() -> String {
    let langs = objc2_foundation::NSLocale::preferredLanguages();
    langs.firstObject().map(|s| s.to_string()).unwrap_or_else(|| "en".into())
}

/// Bring our own (accessory) app to the front, e.g. for the Settings window
pub fn activate_self() {
    let _ = on_main(|mtm| {
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    });
}

/* ---------- running applications ---------- */

pub fn running_apps(bundle_id: &str) -> Vec<Retained<NSRunningApplication>> {
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(bundle_id)).to_vec()
}

pub fn is_running(bundle_id: &str) -> bool { !running_apps(bundle_id).is_empty() }

pub fn bundle_id_of_pid(pid: i32) -> Option<String> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    app.bundleIdentifier().map(|s| s.to_string())
}

pub fn app_name_of_pid(pid: i32) -> Option<String> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    app.localizedName().map(|s| s.to_string())
}

pub fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace().frontmostApplication().map(|a| a.processIdentifier())
}

pub fn activate_pid(pid: i32) {
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        #[allow(deprecated)]
        let _ = app.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::ActivateIgnoringOtherApps);
    }
}

/// Activate a running app by bundle identifier or display name; launch it by bundle id otherwise.
pub fn focus_app(query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() { return false; }
    let ws = NSWorkspace::sharedWorkspace();
    let lq = q.to_lowercase();
    for app in ws.runningApplications().to_vec() {
        let bid = app.bundleIdentifier().map(|s| s.to_string()).unwrap_or_default();
        let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
        if bid.eq_ignore_ascii_case(q) || name.to_lowercase() == lq {
            if app.isHidden() { app.unhide(); }
            #[allow(deprecated)]
            return app.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::ActivateIgnoringOtherApps);
        }
    }
    if let Some(url) = ws.URLForApplicationWithBundleIdentifier(&NSString::from_str(q)) {
        return ws.openURL(&url);
    }
    false
}

/// Display name of an installed application ("com.apple.MobileSMS" -> "Messages")
pub fn app_display_name(bundle_id: &str) -> Option<String> {
    let ws = NSWorkspace::sharedWorkspace();
    let url = ws.URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))?;
    let path = url.path()?;
    let fm = objc2_foundation::NSFileManager::defaultManager();
    let name = fm.displayNameAtPath(&path).to_string();
    Some(name.trim_end_matches(".app").to_string())
}

/* ---------- images ---------- */

/// NSImage -> PNG bytes at `px` x `px` (aspect kept, centered)
pub fn nsimage_png(img: &NSImage, px: f64) -> Option<Vec<u8>> {
    use objc2_app_kit::{NSCompositingOperation, NSGraphicsContext};
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    let size = img.size();
    if size.width <= 0.0 || size.height <= 0.0 { return None; }
    let k = (px / size.width).min(px / size.height);
    let (w, h) = ((size.width * k).round().max(1.0), (size.height * k).round().max(1.0));
    unsafe {
        let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), w as isize, h as isize, 8, 4, true, false,
            objc2_app_kit::NSDeviceRGBColorSpace, 0, 0)?;
        rep.setSize(NSSize::new(w, h));
        let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&ctx));
        img.drawInRect_fromRect_operation_fraction(
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)),
            NSRect::ZERO, NSCompositingOperation::Copy, 1.0);
        ctx.flushGraphics();
        NSGraphicsContext::restoreGraphicsState_class();
        let data = rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
        Some(data.to_vec())
    }
}

pub fn png_data_url(png: &[u8]) -> String { format!("data:image/png;base64,{}", crate::util::b64(png)) }

/// App icon (by bundle identifier) as a PNG data URL
pub fn app_icon_data_url(bundle_id: &str, px: f64) -> Option<String> {
    let ws = NSWorkspace::sharedWorkspace();
    let url = ws.URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))?;
    let path = url.path()?;
    let img = ws.iconForFile(&path);
    nsimage_png(&img, px).map(|p| png_data_url(&p))
}

/* ---------- AppleScript ---------- */

/// Runs AppleScript with /usr/bin/osascript (keeps Apple Events off our UI thread). Returns stdout.
pub fn osascript(src: &str) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn().ok()?;
    child.stdin.take()?.write_all(src.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
}

/* ---------- privacy / permissions ---------- */

pub fn accessibility_trusted() -> bool { unsafe { ffi::AXIsProcessTrusted() } }

/// Ask for Accessibility access (shows the system prompt once)
pub fn request_accessibility() {
    let _ = Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").spawn();
}

pub fn open_privacy_pane(kind: &str) {
    let anchor = match kind {
        "accessibility" => "Privacy_Accessibility",
        "fullDisk" => "Privacy_AllFiles",
        "automation" => "Privacy_Automation",
        "bluetooth" => "Privacy_Bluetooth",
        "notifications" => "Notifications",
        "loginItems" => return { let _ = Command::new("/usr/bin/open").arg("x-apple.systempreferences:com.apple.LoginItems-Settings.extension").spawn(); },
        _ => return,
    };
    let url = if anchor == "Notifications" {
        "x-apple.systempreferences:com.apple.preference.notifications".to_string()
    } else {
        format!("x-apple.systempreferences:com.apple.preference.security?{anchor}")
    };
    let _ = Command::new("/usr/bin/open").arg(url).spawn();
}

/// State shown on the Settings page (macOS section)
pub fn permissions_snapshot() -> serde_json::Value {
    serde_json::json!({
        "accessibility": accessibility_trusted(),
        "notifications": crate::notif::access_ok(),
        "notch": crate::winutil::notch_info(),
        "os": os_version(),
        "arch": std::env::consts::ARCH,
    })
}

/// "26.1", "15.4.1" ...
pub fn os_version() -> String {
    let v = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
    format!("{}.{}.{}", v.majorVersion, v.minorVersion, v.patchVersion)
}

pub fn os_major() -> isize { objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion().majorVersion }

/// Home folder
pub fn home() -> std::path::PathBuf {
    std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| "/".into())
}
