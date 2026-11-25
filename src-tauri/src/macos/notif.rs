// macOS notifications: new banners from Notification Center are mirrored on the island.
// macOS has no public API for reading other apps' notifications, so (like other notch utilities) we read
// Notification Center's own database read-only:
//   macOS 15+  : ~/Library/Group Containers/group.com.apple.usernoted/db2/db
//   macOS 13-14: $(getconf DARWIN_USER_DIR)/com.apple.notificationcenter/db2/db
// Reading it needs Full Disk Access (System Settings > Privacy & Security > Full Disk Access) on recent versions.
// Without access the UI shows a hint and everything else keeps working.
use crate::util::emit;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static ACCESS: AtomicBool = AtomicBool::new(false);

pub fn access_ok() -> bool { ACCESS.load(Ordering::Relaxed) }

fn darwin_user_dir() -> Option<PathBuf> {
    const CS_DARWIN_USER_DIR: libc::c_int = 65537;
    let mut buf = vec![0u8; 1024];
    let n = unsafe { libc::confstr(CS_DARWIN_USER_DIR, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if n == 0 || n > buf.len() { return None; }
    buf.truncate(n - 1);
    Some(PathBuf::from(String::from_utf8_lossy(&buf).to_string()))
}

fn db_candidates() -> Vec<PathBuf> {
    let mut v = vec![crate::macos::home().join("Library/Group Containers/group.com.apple.usernoted/db2/db")];
    if let Some(d) = darwin_user_dir() { v.push(d.join("com.apple.notificationcenter/db2/db")); }
    v
}

fn open_db() -> Option<rusqlite::Connection> {
    for p in db_candidates() {
        if !p.exists() { continue; }
        let Ok(c) = rusqlite::Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX) else { continue };
        // the file may exist but be unreadable without Full Disk Access
        if c.query_row("SELECT max(rec_id) FROM record", [], |r| r.get::<_, Option<i64>>(0)).is_ok() { return Some(c); }
    }
    None
}

pub fn start() {
    std::thread::spawn(|| {
        let mut warned = false;
        loop {
            match open_db() {
                Some(c) => { ACCESS.store(true, Ordering::Relaxed); crate::settings::broadcast_usage(); run(c); }
                None => {
                    ACCESS.store(false, Ordering::Relaxed);
                    if !warned { emit("media", json!({ "type": "notif-denied" })); warned = true; }
                    // retry: the user may grant Full Disk Access later
                    std::thread::sleep(Duration::from_secs(10));
                }
            }
        }
    });
}

struct Note { app: String, title: String, subtitle: String, body: String }

fn decode(blob: &[u8]) -> Option<Note> {
    let v: plist::Value = plist::from_bytes(blob).ok()?;
    let d = v.as_dictionary()?;
    let app = d.get("app").and_then(|x| x.as_string()).unwrap_or("").to_string();
    let req = d.get("req").and_then(|x| x.as_dictionary())?;
    let s = |k: &str| req.get(k).and_then(|x| x.as_string()).unwrap_or("").trim().to_string();
    Some(Note { app, title: s("titl"), subtitle: s("subt"), body: s("body") })
}

fn run(conn: rusqlite::Connection) {
    let max_id = |c: &rusqlite::Connection| c.query_row("SELECT ifnull(max(rec_id), 0) FROM record", [], |r| r.get::<_, i64>(0)).ok();
    let Some(mut last) = max_id(&conn) else { return };   // do not show old ones
    let mut icons: HashMap<String, Option<String>> = HashMap::new();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut calls: HashSet<i64> = HashSet::new();
    loop {
        std::thread::sleep(Duration::from_millis(1500));
        let rows: Vec<(i64, Option<String>, Vec<u8>)> = {
            let Ok(mut st) = conn.prepare_cached(
                "SELECT r.rec_id, a.identifier, r.data FROM record r LEFT JOIN app a ON a.app_id = r.app_id WHERE r.rec_id > ?1 ORDER BY r.rec_id LIMIT 20")
            else { return };
            let Ok(it) = st.query_map([last], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))) else { return };
            it.flatten().collect()
        };
        for (id, ident, data) in rows {
            last = last.max(id);
            let Some(n) = decode(&data) else { continue };
            let bundle = if n.app.is_empty() { ident.unwrap_or_default() } else { n.app.clone() };
            if bundle.is_empty() || bundle == "com.island.dynamic" { continue; }
            let app = names.entry(bundle.clone()).or_insert_with(|| crate::macos::app_display_name(&bundle).unwrap_or_else(|| bundle.rsplit('.').next().unwrap_or(&bundle).to_string())).clone();
            let icon = icons.entry(bundle.clone()).or_insert_with(|| crate::macos::app_icon_data_url(&bundle, 64.0)).clone();
            let mut texts = vec![n.title.clone(), n.subtitle.clone(), n.body.clone()];
            texts.retain(|t| !t.is_empty());
            let call = call_kind(&app, &bundle, &texts);
            if call.is_some() { calls.insert(id); }
            let title = texts.first().cloned().unwrap_or_else(|| app.clone());
            let body = texts.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            emit("media", json!({
                "type": "notif", "id": id.to_string(), "app": app, "bundle": bundle,
                "title": title, "body": body, "icon": icon,
                "call": call.is_some(), "video": call == Some("video"),
            }));
        }
        // call banner gone (answered / declined / missed): close the call card
        if !calls.is_empty() {
            let gone: Vec<i64> = calls.iter().copied().filter(|id| {
                conn.query_row("SELECT count(*) FROM record WHERE rec_id = ?1", [id], |r| r.get::<_, i64>(0)).map(|c| c == 0).unwrap_or(false)
            }).collect();
            for id in gone { calls.remove(&id); emit("media", json!({ "type": "notif-gone", "id": id.to_string() })); }
        }
        if max_id(&conn).is_none() { return; }   // access revoked / database replaced: reopen
    }
}

/// Incoming voice/video call? (FaceTime, WhatsApp, Telegram, Teams, Skype, Signal, Zoom, Messenger, Discord, Viber)
fn call_kind(app: &str, bundle: &str, texts: &[String]) -> Option<&'static str> {
    let a = format!("{} {}", app, bundle).to_lowercase();
    let apps = ["facetime", "whatsapp", "telegram", "teams", "skype", "signal", "zoom", "messenger", "discord", "viber"];
    if !apps.iter().any(|k| a.contains(k)) { return None; }
    let t = texts.join(" ").to_lowercase();
    let video = ["görüntülü", "goruntulu", "video call", "video arama", "incoming video", "facetime video"];
    let voice = ["arıyor", "ariyor", "gelen arama", "sesli arama", "incoming call", "is calling", "voice call", "calling you", "arama geliyor", "facetime audio"];
    if video.iter().any(|k| t.contains(k)) { Some("video") }
    else if voice.iter().any(|k| t.contains(k)) { Some("voice") }
    else { None }
}

/// "Open app": bring the app that posted the notification to the front (by name or bundle id)
#[tauri::command]
pub fn focus_notif_app(app_name: String) { crate::macos::focus_app(&app_name); }
