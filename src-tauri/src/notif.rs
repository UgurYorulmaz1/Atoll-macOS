// Windows notifications (UserNotificationListener): forwards new toasts to the island
use crate::util::emit;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use windows::core::{Interface, Result};
use windows::Foundation::Size;
use windows::Storage::Streams::IRandomAccessStreamReference;
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{KnownNotificationBindings, NotificationKinds, UserNotification};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub fn start() {
    std::thread::spawn(|| {
        unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
        if let Err(e) = run() { eprintln!("notifications disabled: {e:?}"); }
    });
}

fn run() -> Result<()> {
    let l = UserNotificationListener::Current()?;
    if l.GetAccessStatus()? != UserNotificationListenerAccessStatus::Allowed {
        let _ = l.RequestAccessAsync().and_then(|op| op.join());
    }
    if l.GetAccessStatus()? != UserNotificationListenerAccessStatus::Allowed {
        emit("media", json!({ "type": "notif-denied" }));
        return Ok(());
    }
    let mut seen: HashSet<u32> = HashSet::new();
    for n in l.GetNotificationsAsync(NotificationKinds::Toast)?.join()? { seen.insert(n.Id()?); }  // do not show old ones
    let mut icons: HashMap<String, Option<String>> = HashMap::new();
    let mut calls: HashSet<u32> = HashSet::new();   // call notifications currently on screen
    loop {
        std::thread::sleep(Duration::from_millis(1500));
        let Ok(list) = l.GetNotificationsAsync(NotificationKinds::Toast).and_then(|op| op.join()) else { continue };
        let mut present: HashSet<u32> = HashSet::new();
        for n in list {
            let Ok(id) = n.Id() else { continue };
            present.insert(id);
            if !seen.insert(id) { continue; }
            if let Ok(v) = describe(&n, &mut icons) {
                if v["call"].as_bool().unwrap_or(false) { calls.insert(id); }
                emit("media", v);
            }
        }
        // if the call notification is gone (answered / declined / missed), close the call card on the island
        let gone: Vec<u32> = calls.iter().filter(|id| !present.contains(id)).copied().collect();
        for id in gone { calls.remove(&id); emit("media", json!({ "type": "notif-gone", "id": id.to_string() })); }
    }
}

/// Is this an incoming voice/video call notification? (WhatsApp, Telegram, Teams, Skype, Signal, Zoom, Messenger, Discord)
fn call_kind(app: &str, aumid: &str, texts: &[String]) -> Option<&'static str> {
    let a = format!("{} {}", app, aumid).to_lowercase();
    let apps = ["whatsapp", "telegram", "teams", "skype", "signal", "zoom", "messenger", "discord", "viber"];
    if !apps.iter().any(|k| a.contains(k)) { return None; }
    let t = texts.join(" ").to_lowercase();
    let video = ["görüntülü", "goruntulu", "video call", "video arama", "incoming video"];
    let voice = ["arıyor", "ariyor", "gelen arama", "sesli arama", "incoming call", "is calling", "voice call", "calling you", "arama geliyor"];
    if video.iter().any(|k| t.contains(k)) { Some("video") }
    else if voice.iter().any(|k| t.contains(k)) { Some("voice") }
    else { None }
}

fn describe(n: &UserNotification, icons: &mut HashMap<String, Option<String>>) -> Result<serde_json::Value> {
    let binding = n.Notification()?.Visual()?.GetBinding(&KnownNotificationBindings::ToastGeneric()?)?;
    let texts: Vec<String> = binding.GetTextElements()?.into_iter()
        .filter_map(|t| t.Text().ok().map(|h| h.to_string().trim().to_string()))
        .filter(|t| !t.is_empty()).collect();
    let info = n.AppInfo()?;
    let app = info.DisplayInfo()?.DisplayName().map(|h| h.to_string()).unwrap_or_default();
    let aumid = info.AppUserModelId().map(|h| h.to_string()).unwrap_or_default();
    let call = call_kind(&app, &aumid, &texts);
    let icon = icons.entry(aumid).or_insert_with(|| {
        info.DisplayInfo().ok()
            .and_then(|d| d.GetLogo(Size { Width: 64.0, Height: 64.0 }).ok())
            .and_then(|r| r.cast::<IRandomAccessStreamReference>().ok())
            .and_then(|r| crate::media::read_ref(&r))
    }).clone();
    Ok(json!({
        "type": "notif", "id": id_str(n), "app": app,
        "title": texts.first().cloned().unwrap_or_else(|| app.clone()),
        "body": texts.iter().skip(1).cloned().collect::<Vec<_>>().join(" "),
        "icon": icon,
        "call": call.is_some(), "video": call == Some("video"),
    }))
}

fn id_str(n: &UserNotification) -> String { n.Id().map(|i| i.to_string()).unwrap_or_default() }

/// Bring the app that sent the notification to the front (matched by window title)
#[tauri::command]
pub fn focus_notif_app(app_name: String) {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let q = app_name.trim().to_lowercase();
    if q.is_empty() { return; }
    struct Ctx { q: String, found: Option<HWND> }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() { return BOOL(1); }
        let mut buf = [0u16; 256];
        let n = GetWindowTextW(hwnd, &mut buf) as usize;
        if n == 0 { return BOOL(1); }
        let title = String::from_utf16_lossy(&buf[..n]).to_lowercase();
        if title.contains(&ctx.q) {
            ctx.found = Some(hwnd);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut ctx = Ctx { q, found: None };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
        if let Some(h) = ctx.found {
            if IsIconic(h).as_bool() { let _ = ShowWindow(h, SW_RESTORE); }
            let _ = SetForegroundWindow(h);
        }
    }
}
