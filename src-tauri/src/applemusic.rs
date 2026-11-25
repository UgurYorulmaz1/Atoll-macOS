// Apple Music extras on macOS (no account / API key needed, uses Music's AppleScript dictionary):
// - heart: favorite / unfavorite the current track ("favorited" on macOS 14+, "loved" on macOS 13)
// - AirPlay: list the Music app's AirPlay devices and switch output (⌥-click adds/removes for multi-room)
// - search: search your library and play a result
// - up next: the next tracks of the playlist / album that is playing (click to jump there)
// - ⋯ menu (native NSMenu): add to playlist, star rating, "suggest less", show in Music
// Results reuse the island's Spotify panels: "sp-devices" / "sp-search-results" events with a "source": "music" field.
// On Windows the commands are no-ops (Apple Music for Windows has no scripting interface).
use serde_json::{json, Value};

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use crate::util::emit;

    const US: char = '\u{1f}';
    pub const MUSIC: &str = "com.apple.Music";

    fn running() -> bool { crate::macos::is_running(MUSIC) }

    /// AppleScript string literal
    fn q(s: &str) -> String { format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")) }

    fn run(body: &str) -> Option<String> {
        crate::macos::osascript(&format!("set US to character id 31\ntell application \"Music\"\n{body}\nend tell"))
    }

    pub fn favorite(on: bool) {
        if !running() { return; }
        let v = if on { "true" } else { "false" };
        let _ = run(&format!("try\nset favorited of current track to {v}\non error\nset loved of current track to {v}\nend try"));
        crate::media::send("refresh");
    }

    pub fn airplay_list() {
        if !running() {
            emit("sp-devices", json!({ "source": "music", "items": [], "error": crate::settings::l("Müzik uygulaması açık değil", "Music is not running") }));
            return;
        }
        let out = run(r#"set out to {}
repeat with d in AirPlay devices
  try
    set end of out to (name of d) & US & (kind of d as string) & US & (selected of d as string) & US & (available of d as string)
  end try
end repeat
set AppleScript's text item delimiters to linefeed
return out as text"#).unwrap_or_default();
        let items: Vec<Value> = out.lines().filter_map(|l| {
            let f: Vec<&str> = l.split(US).collect();
            if f.len() < 4 || f[3] != "true" { return None; }
            Some(json!({ "id": f[0], "name": f[0], "type": kind_label(f[1]), "kind": f[1], "active": f[2] == "true" }))
        }).collect();
        emit("sp-devices", json!({ "source": "music", "items": items }));
    }

    fn kind_label(kind: &str) -> String {
        match kind {
            "computer" => "Mac",
            "Apple TV" => "Apple TV",
            "HomePod" => "HomePod",
            "AirPort Express" => "AirPort Express",
            "Bluetooth device" => "Bluetooth",
            "AirPlay device" => "AirPlay",
            k => k,
        }.to_string()
    }

    /// add = false: play only on this device; add = true: toggle it in the current set (multi-room)
    pub fn airplay_select(name: &str, add: bool) {
        if !running() { return; }
        let dev = format!("(first AirPlay device whose name is {})", q(name));
        let body = if add {
            format!(r#"set d to {dev}
set cur to current AirPlay devices
if selected of d then
  if (count of cur) > 1 then set selected of d to false
else
  set selected of d to true
end if"#)
        } else {
            format!("set current AirPlay devices to {{{dev}}}")
        };
        let _ = run(&body);
        airplay_list();
    }

    pub fn search(query: &str) {
        let query = query.trim().to_string();
        if query.is_empty() { return; }
        if !running() {
            emit("sp-search-results", json!({ "source": "music", "q": query, "items": [], "error": crate::settings::l("Müzik uygulaması açık değil", "Music is not running") }));
            return;
        }
        let out = run(&format!(r#"set res to search library playlist 1 for {} only songs
set out to {{}}
set n to 0
repeat with t in res
  set n to n + 1
  if n > 25 then exit repeat
  try
    set end of out to (persistent ID of t) & US & (name of t) & US & (artist of t) & US & (album of t)
  end try
end repeat
set AppleScript's text item delimiters to linefeed
return out as text"#, q(&query)));
        let items: Vec<Value> = out.unwrap_or_default().lines().filter_map(|l| {
            let f: Vec<&str> = l.split(US).collect();
            if f.len() < 4 { return None; }
            let artist = if f[3].is_empty() { f[2].to_string() } else { format!("{} · {}", f[2], f[3]) };
            Some(json!({ "uri": format!("am:{}", f[0]), "name": f[1], "artist": artist, "image": null }))
        }).collect();
        emit("sp-search-results", json!({ "source": "music", "q": query, "items": items }));
    }

    /// Next tracks of the current playlist (Music has no scriptable "Up Next"; this is the playlist order)
    pub fn queue() {
        if !running() { emit("am-queue", json!({ "items": [] })); return; }
        let out = run(r#"set out to {}
try
  set cp to current playlist
  set ct to current track
  set i to index of ct
  set n to count of tracks of cp
  set sh to shuffle enabled
  set end of out to "H" & US & (name of cp) & US & (sh as string)
  repeat with k from (i + 1) to (i + 15)
    if k > n then exit repeat
    set t to track k of cp
    set end of out to "T" & US & k & US & (name of t) & US & (artist of t)
  end repeat
on error
  -- not playing from a library playlist (album page, Apple Music stream ...): next tracks of the same album in the library
  try
    set ct to current track
    set al to album of ct
    set ar to album artist of ct
    if ar is "" then set ar to artist of ct
    set cn to (disc number of ct) * 1000 + (track number of ct)
    set end of out to "A" & US & al
    repeat with t in (tracks of library playlist 1 whose album is al)
      set aa to album artist of t
      if aa is "" then set aa to artist of t
      set k to (disc number of t) * 1000 + (track number of t)
      if aa is ar and k > cn then set end of out to "P" & US & k & US & (name of t) & US & (artist of t) & US & (persistent ID of t)
    end repeat
  end try
end try
set AppleScript's text item delimiters to linefeed
return out as text"#).unwrap_or_default();
        let mut album: Option<String> = None;
        let mut album_items: Vec<(u32, Value)> = Vec::new();
        let mut playlist = String::new();
        let mut shuffled = false;
        let mut items = Vec::new();
        for l in out.lines() {
            let f: Vec<&str> = l.split(US).collect();
            match f.first() {
                Some(&"H") if f.len() >= 3 => { playlist = f[1].into(); shuffled = f[2] == "true"; }
                Some(&"T") if f.len() >= 4 => items.push(json!({ "index": f[1].parse::<u32>().unwrap_or(0), "name": f[2], "artist": f[3] })),
                Some(&"A") if f.len() >= 2 => album = Some(f[1].to_string()),
                Some(&"P") if f.len() >= 5 => album_items.push((f[1].parse().unwrap_or(0), json!({ "pid": f[4], "name": f[2], "artist": f[3] }))),
                _ => {}
            }
        }
        if let Some(al) = album {
            album_items.sort_by_key(|(k, _)| *k);
            items = album_items.into_iter().take(15).map(|(_, v)| v).collect();
            emit("am-queue", json!({ "album": al, "items": items }));
            return;
        }
        emit("am-queue", json!({ "playlist": playlist, "shuffled": shuffled, "items": items }));
    }

    /// Jump to track #index of the current playlist (keeps the playlist as the playing context)
    pub fn skip_to(index: u32) {
        if !running() || index == 0 { return; }
        let _ = run(&format!("play track {index} of current playlist"));
        crate::media::send("refresh");
        std::thread::sleep(std::time::Duration::from_millis(600));
        queue();
    }

    /* ---------- ⋯ menu ---------- */
    pub struct MenuData { pub playlists: Vec<(String, String)>, pub rating: u32, pub disliked: bool, pub title: String }

    pub fn menu_data() -> Option<MenuData> {
        if !running() { return None; }
        let out = run(r#"set out to {}
set t to current track
set dl to "false"
try
  set dl to (disliked of t) as string
end try
set end of out to "C" & US & (rating of t) & US & dl & US & (name of t)
repeat with p in (user playlists whose smart is false and special kind is none)
  try
    set end of out to "P" & US & (persistent ID of p) & US & (name of p)
  end try
end repeat
set AppleScript's text item delimiters to linefeed
return out as text"#)?;
        let mut d = MenuData { playlists: vec![], rating: 0, disliked: false, title: String::new() };
        for l in out.lines() {
            let f: Vec<&str> = l.split(US).collect();
            match f.first() {
                Some(&"C") if f.len() >= 4 => { d.rating = f[1].parse().unwrap_or(0); d.disliked = f[2] == "true"; d.title = f[3].into(); }
                Some(&"P") if f.len() >= 3 => d.playlists.push((f[1].into(), f[2].into())),
                _ => {}
            }
        }
        Some(d)
    }

    fn msg(text: String) { emit("am-msg", json!({ "text": text })); }

    pub fn on_menu(id: &str) {
        let l = crate::settings::l;
        if let Some(pid) = id.strip_prefix("am-pl:") {
            if !pid.chars().all(|c| c.is_ascii_hexdigit()) { return; }
            let r = run(&format!(r#"set p to (first user playlist whose persistent ID is {})
duplicate current track to p
return name of p"#, q(pid)));
            match r {
                Some(name) => msg(format!("{} {}", l("Eklendi:", "Added to"), name)),
                None => msg(l("Çalma listesine eklenemedi", "Could not add to the playlist").into()),
            }
        } else if let Some(n) = id.strip_prefix("am-rate:") {
            let stars: u32 = n.parse().unwrap_or(0).min(5);
            let _ = run(&format!("set rating of current track to {}", stars * 20));
            msg(if stars == 0 { l("Puan kaldırıldı", "Rating removed").into() } else { format!("{} {}", "★".repeat(stars as usize), l("puan verildi", "rated")) });
        } else if id == "am-dislike" {
            let r = run(r#"set t to current track
set v to not (disliked of t)
set disliked of t to v
if v then set favorited of t to false
return v as string"#);
            let on = r.as_deref() == Some("true");
            msg(if on { l("Daha az önerilecek", "Will suggest less like this").into() } else { l("Öneri tercihi kaldırıldı", "Preference removed").into() });
            crate::media::send("refresh");
        } else if id == "am-reveal" {
            let _ = run("reveal current track
activate");
        }
    }

    pub fn play(pid: &str) {
        if !running() || !pid.chars().all(|c| c.is_ascii_hexdigit()) { return; }
        // Right after Music launches, the first "play" of a streamed (subscription) track can be ignored: retry once
        let _ = run(&format!(r#"set t to (first track of library playlist 1 whose persistent ID is {})
play t
delay 1.2
if player state is not playing then
  delay 0.8
  play t
end if"#, q(pid)));
        crate::media::send("refresh");
    }
}

#[tauri::command]
pub fn am_favorite(on: bool) {
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || imp::favorite(on));
    #[cfg(not(target_os = "macos"))]
    let _ = on;
}

#[tauri::command]
pub fn am_devices() {
    #[cfg(target_os = "macos")]
    std::thread::spawn(imp::airplay_list);
}

#[tauri::command]
pub fn am_airplay(name: String, add: bool) {
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || imp::airplay_select(&name, add));
    #[cfg(not(target_os = "macos"))]
    let _ = (name, add);
}

#[tauri::command]
pub fn am_search(q: String) {
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || imp::search(&q));
    #[cfg(not(target_os = "macos"))]
    let _ = q;
}

#[tauri::command]
pub fn am_play(uri: String) {
    #[cfg(target_os = "macos")]
    if let Some(pid) = uri.strip_prefix("am:").map(str::to_string) { std::thread::spawn(move || imp::play(&pid)); }
    #[cfg(not(target_os = "macos"))]
    let _ = uri;
}

#[tauri::command]
pub fn am_queue() {
    #[cfg(target_os = "macos")]
    std::thread::spawn(imp::queue);
}

#[tauri::command]
pub fn am_skipto(index: u32) {
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || imp::skip_to(index));
    #[cfg(not(target_os = "macos"))]
    let _ = index;
}

/// ⋯ button: native menu with playlists, rating, "suggest less" and "show in Music"
#[tauri::command]
pub async fn am_menu(window: tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
        use tauri::Manager;
        let l = crate::settings::l;
        let Some(d) = tauri::async_runtime::spawn_blocking(imp::menu_data).await.ok().flatten() else { return Ok(()) };
        let app = window.app_handle();
        let e = |x: tauri::Error| x.to_string();
        // add to playlist
        let mut pl_items: Vec<MenuItem<tauri::Wry>> = Vec::new();
        for (pid, name) in d.playlists.iter().take(60) {
            pl_items.push(MenuItem::with_id(app, format!("am-pl:{pid}"), name, true, None::<&str>).map_err(e)?);
        }
        let empty = MenuItem::with_id(app, "am-none", l("Çalma listesi yok", "No playlists"), false, None::<&str>).map_err(e)?;
        let pl_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = if pl_items.is_empty() { vec![&empty] } else { pl_items.iter().map(|x| x as &dyn IsMenuItem<tauri::Wry>).collect() };
        let pl_menu = Submenu::with_items(app, l("Çalma listesine ekle", "Add to Playlist"), true, &pl_refs).map_err(e)?;
        // star rating
        let cur = (d.rating + 10) / 20;
        let mut rate_items: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
        for n in 0..=5u32 {
            let label = if n == 0 { l("Puan yok", "None").to_string() } else { "★".repeat(n as usize) };
            rate_items.push(CheckMenuItem::with_id(app, format!("am-rate:{n}"), label, true, n == cur, None::<&str>).map_err(e)?);
        }
        let rate_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = rate_items.iter().map(|x| x as &dyn IsMenuItem<tauri::Wry>).collect();
        let rate_menu = Submenu::with_items(app, l("Puan ver", "Rating"), true, &rate_refs).map_err(e)?;
        let title = MenuItem::with_id(app, "am-none", &d.title, false, None::<&str>).map_err(e)?;
        let dislike = CheckMenuItem::with_id(app, "am-dislike", l("Daha az öner", "Suggest Less"), true, d.disliked, None::<&str>).map_err(e)?;
        let reveal = MenuItem::with_id(app, "am-reveal", l("Müzik'te göster", "Show in Music"), true, None::<&str>).map_err(e)?;
        let sep1 = PredefinedMenuItem::separator(app).map_err(e)?;
        let sep2 = PredefinedMenuItem::separator(app).map_err(e)?;
        let menu = Menu::with_items(app, &[&title, &sep1, &pl_menu, &rate_menu, &dislike, &sep2, &reveal]).map_err(e)?;
        window.popup_menu(&menu).map_err(e)?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = window;
    Ok(())
}

/// Menu events with an "am-" id
pub fn on_menu(id: &str) {
    #[cfg(target_os = "macos")]
    { let id = id.to_string(); std::thread::spawn(move || imp::on_menu(&id)); }
    #[cfg(not(target_os = "macos"))]
    let _ = id;
}

#[allow(dead_code)]
fn _unused() -> Value { json!(null) }
