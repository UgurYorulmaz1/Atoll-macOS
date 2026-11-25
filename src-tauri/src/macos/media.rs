// macOS "now playing": artwork, track info and controls.
// - Apple Music and Spotify: their official AppleScript dictionaries (works on every macOS version, incl. shuffle /
//   repeat / seeking). Apps are only queried while they are running, so the island never launches them.
// - Everything else (Safari / Chrome / Arc / Firefox tabs, Podcasts, TV, VLC, IINA ...): the system Now Playing
//   service (MediaRemote). macOS 15.4+ limits that service to Apple-signed clients; on those versions
//   the AppleScript sources keep working and other players simply do not appear.
// The session selection / second-source / single-source logic matches the Windows version.
use crate::util::{b64, emit, unix_ms};
use serde_json::json;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const MUSIC: &str = "com.apple.Music";
const SPOTIFY: &str = "com.spotify.client";

static TX: OnceLock<Mutex<Sender<String>>> = OnceLock::new();
static LAST_APP: Mutex<String> = Mutex::new(String::new());

pub fn send(cmd: &str) {
    if let Some(tx) = TX.get() { let _ = tx.lock().unwrap().send(cmd.to_string()); }
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct Sess {
    app: String,          // bundle identifier
    title: String,
    artist: String,
    album: String,
    playing: bool,
    position: f64,
    duration: f64,
    shuffle: bool,
    repeat: i32,          // 0 none, 1 track, 2 list (same values as Windows)
    can_shuffle: bool,
    can_repeat: bool,
    art_url: String,      // Spotify artwork URL
    track_id: String,
    favorited: Option<bool>, // Apple Music heart
    playlist: String,         // Apple Music "playing from"
}

impl Sess {
    fn key(&self) -> String { format!("{}|{}", self.title, self.artist) }
    fn json(&self) -> serde_json::Value {
        let r2 = |x: f64| (x * 100.0).round() / 100.0;
        json!({
            "type": "media", "active": !self.title.is_empty(), "key": self.key(), "app": self.app,
            "title": self.title, "artist": self.artist, "album": self.album, "playing": self.playing,
            "position": r2(self.position), "duration": r2(self.duration), "updated": unix_ms(),
            "canSeek": self.duration > 0.0, "canShuffle": self.can_shuffle, "canRepeat": self.can_repeat,
            "canNext": true, "canPrev": true, "shuffle": self.shuffle, "repeat": self.repeat,
            "favorited": self.favorited, "playlist": self.playlist,
        })
    }
}

/* ---------- AppleScript sources ---------- */
const US: char = '\u{1f}';   // field separator

fn parse_num(s: &str) -> f64 { s.trim().replace(',', ".").parse().unwrap_or(0.0) }

fn script_for(app: &str) -> Option<&'static str> {
    Some(match app {
        MUSIC => r#"tell application "Music"
  set pstate to player state as string
  if pstate is "stopped" then
    set end of out to "MU" & US & pstate
  else
    set t to current track
    -- "favorited" (macOS 14+) / "loved" (macOS 13)
    set fav to "?"
    try
      set fav to (favorited of t) as string
    on error
      try
        set fav to (loved of t) as string
      end try
    end try
    set pl to ""
    try
      set pl to name of current playlist
    end try
    set end of out to "MU" & US & pstate & US & (name of t) & US & (artist of t) & US & (album of t) & US & (duration of t) & US & (player position) & US & (shuffle enabled) & US & (song repeat as string) & US & (persistent ID of t) & US & fav & US & pl
  end if
end tell"#,
        SPOTIFY => r#"tell application "Spotify"
  set pstate to player state as string
  if pstate is "stopped" then
    set end of out to "SP" & US & pstate
  else
    set t to current track
    set end of out to "SP" & US & pstate & US & (name of t) & US & (artist of t) & US & (album of t) & US & (duration of t) & US & (player position) & US & shuffling & US & repeating & US & (id of t) & US & (artwork url of t)
  end if
end tell"#,
        _ => return None,
    })
}

/// Queries the running scriptable players in one osascript call
fn scripted_sessions() -> Vec<Sess> {
    let apps: Vec<&str> = [MUSIC, SPOTIFY].into_iter().filter(|a| crate::macos::is_running(a)).collect();
    if apps.is_empty() { return vec![]; }
    let mut src = String::from("set US to character id 31\nset out to {}\n");
    for a in &apps {
        src += &format!("try\n{}\nend try\n", script_for(a).unwrap());
    }
    src += "set AppleScript's text item delimiters to linefeed\nreturn out as text\n";
    let Some(out) = crate::macos::osascript(&src) else { return vec![] };
    out.lines().filter_map(|line| {
        let f: Vec<&str> = line.split(US).collect();
        let app = match f.first()? { &"MU" => MUSIC, &"SP" => SPOTIFY, _ => return None };
        if f.len() < 10 { return None; }   // stopped / nothing loaded
        let playing = f[1] == "playing";
        let mut s = Sess { app: app.into(), title: f[2].into(), artist: f[3].into(), album: f[4].into(), playing,
            position: parse_num(f[6]), can_shuffle: true, can_repeat: true, shuffle: f[7] == "true", track_id: f[9].into(), ..Default::default() };
        if app == MUSIC {
            s.duration = parse_num(f[5]);
            s.repeat = match f[8] { "one" => 1, "all" => 2, _ => 0 };
            s.playlist = f.get(11).map(|x| x.to_string()).unwrap_or_default();
            s.favorited = match f.get(10).copied() { Some("true") => Some(true), Some("false") => Some(false), _ => None };
        } else {
            s.duration = parse_num(f[5]) / 1000.0;   // Spotify reports milliseconds
            s.repeat = if f[8] == "true" { 2 } else { 0 };
            s.art_url = f.get(10).map(|x| x.to_string()).unwrap_or_default();
        }
        if s.title.is_empty() { return None; }
        Some(s)
    }).collect()
}

fn tell(app: &str, cmd: &str) { let _ = crate::macos::osascript(&format!("tell application \"{}\" to {}", app_name(app), cmd)); }
fn app_name(bundle: &str) -> &'static str { if bundle == MUSIC { "Music" } else { "Spotify" } }

/// Apple Music artwork: "«data JPEG FFD8…»" from osascript -> bytes
fn music_artwork() -> Option<Vec<u8>> {
    let out = crate::macos::osascript("tell application \"Music\" to get raw data of artwork 1 of current track")?;
    let start = out.find("«data ")? + "«data ".len();
    let hex = out[start..].get(4..)?.trim_end_matches('»');
    let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
    (bytes.len() > 100).then_some(bytes)
}

fn fetch_url(url: &str) -> Option<Vec<u8>> {
    if !url.starts_with("https://") { return None; }
    let url = url.to_string();
    tauri::async_runtime::block_on(async move {
        let r = reqwest::Client::new().get(url).timeout(Duration::from_secs(8)).send().await.ok()?;
        if !r.status().is_success() { return None; }
        r.bytes().await.ok().map(|b| b.to_vec())
    })
}

fn data_url(bytes: &[u8]) -> String {
    let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) { "image/png" }
        else if bytes.starts_with(b"GIF") { "image/gif" }
        else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" { "image/webp" }
        else { "image/jpeg" };
    format!("data:{};base64,{}", mime, b64(bytes))
}

/* ---------- MediaRemote (system Now Playing) ---------- */
mod mr {
    use block2::{Block, RcBlock};
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSData, NSDate, NSDictionary, NSNumber, NSString};
    use std::ffi::c_void;
    use std::sync::mpsc::channel;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    type InfoFn = unsafe extern "C" fn(*mut c_void, *const Block<dyn Fn(*const c_void)>);
    type PidFn = unsafe extern "C" fn(*mut c_void, *const Block<dyn Fn(i32)>);
    type SendFn = unsafe extern "C" fn(u32, *const c_void) -> bool;
    type ElapsedFn = unsafe extern "C" fn(f64);

    pub struct Lib { info: InfoFn, pid: PidFn, send: SendFn, elapsed: ElapsedFn }
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();

    pub fn lib() -> Option<&'static Lib> {
        LIB.get_or_init(|| unsafe {
            let h = libc::dlopen(c"/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote".as_ptr(), libc::RTLD_LAZY);
            if h.is_null() { return None; }
            let sym = |n: &std::ffi::CStr| { let p = libc::dlsym(h, n.as_ptr()); (!p.is_null()).then_some(p) };
            Some(Lib {
                info: std::mem::transmute::<*mut c_void, InfoFn>(sym(c"MRMediaRemoteGetNowPlayingInfo")?),
                pid: std::mem::transmute::<*mut c_void, PidFn>(sym(c"MRMediaRemoteGetNowPlayingApplicationPID")?),
                send: std::mem::transmute::<*mut c_void, SendFn>(sym(c"MRMediaRemoteSendCommand")?),
                elapsed: std::mem::transmute::<*mut c_void, ElapsedFn>(sym(c"MRMediaRemoteSetElapsedTime")?),
            })
        }).as_ref()
    }

    #[derive(Default)]
    pub struct Info {
        pub title: String, pub artist: String, pub album: String,
        pub duration: f64, pub position: f64, pub playing: bool,
        pub shuffle: bool, pub repeat: i32, pub art: Option<Vec<u8>>, pub id: String,
    }

    fn queue() -> *mut c_void { unsafe { crate::macos::ffi::dispatch_get_global_queue(0, 0) } }

    pub fn now_playing_pid() -> i32 {
        let Some(l) = lib() else { return 0 };
        let (tx, rx) = channel();
        let tx = Mutex::new(Some(tx));
        let blk = RcBlock::new(move |pid: i32| { if let Some(t) = tx.lock().unwrap().take() { let _ = t.send(pid); } });
        unsafe { (l.pid)(queue(), &*blk) };
        rx.recv_timeout(Duration::from_millis(700)).unwrap_or(0)
    }

    pub fn now_playing(want_art: bool) -> Option<Info> {
        let l = lib()?;
        let (tx, rx) = channel::<Option<Info>>();
        let tx = Mutex::new(Some(tx));
        let blk = RcBlock::new(move |dict: *const c_void| {
            let info = unsafe { parse(dict, want_art) };
            if let Some(t) = tx.lock().unwrap().take() { let _ = t.send(info); }
        });
        unsafe { (l.info)(queue(), &*blk) };
        rx.recv_timeout(Duration::from_millis(900)).ok().flatten()
    }

    unsafe fn parse(dict: *const c_void, want_art: bool) -> Option<Info> {
        if dict.is_null() { return None; }
        let d: &NSDictionary<NSString, AnyObject> = &*(dict as *const NSDictionary<NSString, AnyObject>);
        let get = |k: &str| -> Option<Retained<AnyObject>> { d.objectForKey(&NSString::from_str(k)) };
        let s = |k: &str| get(k).and_then(|o| o.downcast::<NSString>().ok()).map(|x| x.to_string()).unwrap_or_default();
        let n = |k: &str| get(k).and_then(|o| o.downcast::<NSNumber>().ok()).map(|x| x.doubleValue());
        let title = s("kMRMediaRemoteNowPlayingInfoTitle");
        if title.is_empty() { return None; }
        let rate = n("kMRMediaRemoteNowPlayingInfoPlaybackRate").unwrap_or(0.0);
        let mut pos = n("kMRMediaRemoteNowPlayingInfoElapsedTime").unwrap_or(0.0);
        if rate > 0.0 {
            if let Some(ts) = get("kMRMediaRemoteNowPlayingInfoTimestamp").and_then(|o| o.downcast::<NSDate>().ok()) {
                pos += -ts.timeIntervalSinceNow() * rate;
            }
        }
        let art = if want_art {
            get("kMRMediaRemoteNowPlayingInfoArtworkData").and_then(|o| o.downcast::<NSData>().ok()).map(|d| d.to_vec())
        } else { None };
        Some(Info {
            artist: s("kMRMediaRemoteNowPlayingInfoArtist"),
            album: s("kMRMediaRemoteNowPlayingInfoAlbum"),
            duration: n("kMRMediaRemoteNowPlayingInfoDuration").unwrap_or(0.0),
            position: pos.max(0.0),
            playing: rate > 0.0,
            shuffle: n("kMRMediaRemoteNowPlayingInfoShuffleMode").map(|v| v as i32 >= 2).unwrap_or(false),
            repeat: match n("kMRMediaRemoteNowPlayingInfoRepeatMode").map(|v| v as i32) { Some(2) => 1, Some(3) => 2, _ => 0 },
            id: s("kMRMediaRemoteNowPlayingInfoUniqueIdentifier"),
            title, art,
        })
    }

    /// 0 play, 1 pause, 2 toggle, 4 next, 5 previous, 6 advance shuffle, 7 advance repeat
    pub fn command(c: u32) -> bool { lib().map(|l| unsafe { (l.send)(c, std::ptr::null()) }).unwrap_or(false) }
    pub fn seek(sec: f64) { if let Some(l) = lib() { unsafe { (l.elapsed)(sec) } } }
}

fn remote_session(want_art: bool) -> (Option<Sess>, Option<Vec<u8>>) {
    let Some(info) = mr::now_playing(want_art) else { return (None, None) };
    let pid = mr::now_playing_pid();
    let app = if pid > 0 { crate::macos::bundle_id_of_pid(pid).unwrap_or_else(|| format!("pid.{pid}")) } else { "com.apple.mediaremote".into() };
    let s = Sess { app, title: info.title, artist: info.artist, album: info.album, playing: info.playing,
        position: info.position, duration: info.duration, shuffle: info.shuffle, repeat: info.repeat,
        can_shuffle: true, can_repeat: true, track_id: info.id, ..Default::default() };
    (Some(s), info.art)
}

/* ---------- polling loop ---------- */
fn all_sessions(want_remote_art: bool) -> (Vec<Sess>, Option<Vec<u8>>) {
    let mut list = scripted_sessions();
    let (remote, art) = remote_session(want_remote_art);
    if let Some(r) = remote {
        // the scriptable player already provides richer data for the same app
        if !list.iter().any(|s| s.app.eq_ignore_ascii_case(&r.app)) { list.push(r); }
    }
    (list, art)
}

fn is_scripted(app: &str) -> bool { app == MUSIC || app == SPOTIFY }

pub fn start() {
    let (tx, rx) = channel::<String>();
    let _ = TX.set(Mutex::new(tx));
    std::thread::spawn(move || {
        let mut last_json = String::new();
        let mut last_key = String::new();
        let mut thumb_tries: i32 = 0;
        let mut last_thumb: Option<serde_json::Value> = None;
        let mut tick: u32 = 0;
        let (mut sec_json, mut sec_key, mut sec_thumb) = (String::new(), String::new(), None::<serde_json::Value>);
        let mut prev_playing: HashSet<String> = HashSet::new();
        let mut idle_ticks = 0u32;
        loop {
            match rx.recv_timeout(Duration::from_millis(800)) {
                Ok(cmd) => { handle(&cmd); last_json.clear(); idle_ticks = 0; continue; }
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            tick = tick.wrapping_add(1);
            // nothing playing for a while: poll every 2.4 s instead of 0.8 s (less Apple Events traffic)
            if idle_ticks > 10 && tick % 3 != 0 { continue; }
            let resend = tick % 8 == 0;
            let want_art = (0..8).contains(&thumb_tries);
            let (sessions, remote_art) = all_sessions(want_art);
            if sessions.iter().any(|s| s.playing) { idle_ticks = 0; } else { idle_ticks += 1; }
            enforce_focus(&sessions, &mut prev_playing, tick == 1);

            match pick(&sessions) {
                Some(s) => {
                    if s.playing { crate::stats::record_tick(&s.title, &s.artist, 800); }
                    let v = s.json();
                    let st = v.to_string();
                    if st != last_json || resend { emit("media", v); last_json = st; }
                    let key = s.key();
                    if key != last_key { last_key = key.clone(); thumb_tries = 0; last_thumb = None; }
                    if (0..8).contains(&thumb_tries) {
                        let bytes = match s.app.as_str() {
                            MUSIC => music_artwork(),
                            SPOTIFY => fetch_url(&s.art_url),
                            _ => remote_art.clone(),
                        };
                        match bytes {
                            Some(b) => {
                                let t = json!({ "type": "thumb", "key": key, "data": data_url(&b) });
                                emit("media", t.clone()); last_thumb = Some(t); thumb_tries = -1;
                            }
                            None => thumb_tries += 1,
                        }
                    } else if resend && tick % 24 == 0 {
                        if let Some(t) = &last_thumb { emit("media", t.clone()); }
                    }
                }
                None => {
                    let st = r#"{"type":"media","active":false}"#;
                    if last_json != st { emit("media", json!({ "type": "media", "active": false })); last_json = st.into(); }
                    last_key.clear();
                }
            }

            // A SECOND source playing at the same time (bubble next to the island)
            match secondary(&sessions, &last_key) {
                Some(s) => {
                    let key = s.key();
                    let v = json!({ "type": "media2", "active": true, "app": s.app, "title": s.title, "artist": s.artist, "key": key });
                    let st = v.to_string();
                    if st != sec_json || resend { emit("media", v); sec_json = st; }
                    if key != sec_key {
                        sec_key = key.clone();
                        let bytes = match s.app.as_str() { MUSIC => music_artwork(), SPOTIFY => fetch_url(&s.art_url), _ => None };
                        sec_thumb = bytes.map(|b| json!({ "type": "thumb2", "key": key, "data": data_url(&b) }));
                        if let Some(t) = &sec_thumb { emit("media", t.clone()); }
                    } else if resend && tick % 24 == 0 {
                        if let Some(t) = &sec_thumb { emit("media", t.clone()); }
                    }
                }
                None => {
                    let st = r#"{"type":"media2","active":false}"#;
                    if sec_json != st || resend { emit("media", json!({ "type": "media2", "active": false })); sec_json = st.into(); }
                    sec_key.clear();
                }
            }
        }
    });
}

/* ---------- session selection (same rules as Windows) ---------- */
fn pick(list: &[Sess]) -> Option<Sess> {
    let last_id = LAST_APP.lock().unwrap().clone();
    let last = list.iter().find(|s| !last_id.is_empty() && s.app == last_id);
    let chosen = match last {
        Some(l) if l.playing => Some(l.clone()),
        _ => list.iter().find(|s| s.playing).cloned().or_else(|| last.cloned()).or_else(|| list.first().cloned()),
    }?;
    *LAST_APP.lock().unwrap() = chosen.app.clone();
    Some(chosen)
}

fn secondary(list: &[Sess], primary_key: &str) -> Option<Sess> {
    let primary = LAST_APP.lock().unwrap().clone();
    list.iter().find(|s| {
        s.app != primary && s.playing && !s.title.is_empty()
            && !s.key().eq_ignore_ascii_case(primary_key)
            && !s.title.trim().eq_ignore_ascii_case(primary_key.split('|').next().unwrap_or("").trim())
    }).cloned()
}

fn pause_app(app: &str) {
    if is_scripted(app) { tell(app, "pause"); } else { mr::command(1); }
}

fn enforce_focus(list: &[Sess], prev: &mut HashSet<String>, first: bool) {
    let order: Vec<String> = list.iter().filter(|s| s.playing).map(|s| s.app.clone()).collect();
    let mut now: HashSet<String> = order.iter().cloned().collect();
    if crate::settings::get().single_source && !first && now.len() > 1 {
        if let Some(newest) = order.iter().find(|id| !prev.contains(*id)).cloned() {
            *LAST_APP.lock().unwrap() = newest.clone();
            for id in order.iter().filter(|id| **id != newest) { pause_app(id); }
            now.retain(|id| *id == newest);
        }
    }
    *prev = now;
}

fn handle(cmd: &str) {
    let (sessions, _) = all_sessions(false);
    if let Some(id) = cmd.strip_prefix("focus ") {
        if sessions.iter().any(|s| s.app == id) {
            *LAST_APP.lock().unwrap() = id.to_string();
            if crate::settings::get().single_source {
                for s in sessions.iter().filter(|s| s.app != id && s.playing) { pause_app(&s.app); }
            }
        }
        return;
    }
    let Some(s) = pick(&sessions) else { return };
    let mut parts = cmd.split(' ');
    let verb = parts.next().unwrap_or("");
    if is_scripted(&s.app) {
        let music = s.app == MUSIC;
        match verb {
            "toggle" => tell(&s.app, "playpause"),
            "next" => tell(&s.app, "next track"),
            "prev" => tell(&s.app, "previous track"),
            "seek" => {
                let sec: f64 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
                tell(&s.app, &format!("set player position to {:.2}", sec.max(0.0)));
            }
            "shuffle" => tell(&s.app, if music { "set shuffle enabled to not shuffle enabled" } else { "set shuffling to not shuffling" }),
            "repeat" => {
                if music {
                    // None -> List -> Track -> None (same cycle as on Windows)
                    let next = match s.repeat { 0 => "all", 2 => "one", _ => "off" };
                    tell(&s.app, &format!("set song repeat to {next}"));
                } else {
                    tell(&s.app, "set repeating to not repeating");
                }
            }
            _ => {}
        }
    } else {
        match verb {
            "toggle" => { mr::command(2); }
            "next" => { mr::command(4); }
            "prev" => { mr::command(5); }
            "seek" => mr::seek(parts.next().and_then(|x| x.parse().ok()).unwrap_or(0.0)),
            "shuffle" => { mr::command(6); }
            "repeat" => { mr::command(7); }
            _ => {}
        }
    }
}

#[allow(dead_code)]
fn _unused(_: *const c_void) {}
