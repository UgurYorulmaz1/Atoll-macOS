// Shelf (file tray)
// - Dropped files/folders are copied IMMEDIATELY: to disk (default, folder selectable) or to RAM (Settings).
// - A RAM item is written to a temp folder once when it is exported (drag out, copy, open).
// - Total size limit (default 16 GB); RAM mode cannot exceed half of system RAM.
// - Clipboard history (optional): copied files, images and text land on the shelf (password managers excluded).
use crate::settings::{self, l};
use crate::util::{data_dir, emit, show_in_folder, shell_open};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::{Manager, WebviewWindow};
#[cfg(windows)]
use windows::Win32::Foundation::HWND;

const MAX_ITEMS: usize = 60;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Content kept in RAM: a single file or a folder tree (relative path, data)
enum Blob { File(Vec<u8>), Dir(Vec<(PathBuf, Vec<u8>)>) }

#[derive(Serialize, Deserialize, Clone)]
struct Item {
    id: String,
    name: String,
    #[serde(default)] path: String,        // copy on disk (empty for RAM items)
    #[serde(default)] orig: String,
    #[serde(default = "kind_file")] kind: String,   // file | dir | text | image
    #[serde(default)] size: u64,
    #[serde(default)] text: String,        // preview for text cards
    #[serde(skip)] copying: bool,
    #[serde(skip)] ram: Option<Arc<Blob>>,
}
fn kind_file() -> String { "file".into() }

static ITEMS: Mutex<Vec<Item>> = Mutex::new(Vec::new());
static ICONS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);     // item id -> icon
static EXPORTED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);  // RAM item -> temp file
static IGNORE_SEQ: AtomicU32 = AtomicU32::new(0);                            // our own clipboard writes

fn default_root() -> PathBuf { data_dir().join("shelf-cache") }
fn root() -> PathBuf {
    let d = settings::get().storage_dir;
    if !d.is_empty() && Path::new(&d).is_dir() { PathBuf::from(d) } else { default_root() }
}
fn export_root() -> PathBuf { std::env::temp_dir().join(if crate::util::is_test() { "DynamicIsland-test" } else { "DynamicIsland" }) }
fn list_file() -> PathBuf { data_dir().join("shelf.json") }
fn exists(p: &str) -> bool { !p.is_empty() && Path::new(p).exists() }
fn new_id() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{:x}{:04x}", t, rand::random::<u16>())
}
fn msg(text: &str) { emit("shelf-msg", text.to_string()); }
fn fmt_gb(b: f64) -> String { if b >= GB { format!("{:.1} GB", b / GB) } else { format!("{:.0} MB", b / 1024.0 / 1024.0) } }

pub fn limit_bytes() -> u64 { (settings::get().limit_gb * GB) as u64 }
pub fn ram_limit_bytes() -> u64 { crate::shellx::total_ram() / 2 }
pub fn used_bytes() -> u64 { ITEMS.lock().unwrap().iter().map(|x| x.size).sum() }
fn ram_used() -> u64 { ITEMS.lock().unwrap().iter().filter(|x| x.ram.is_some()).map(|x| x.size).sum() }
fn alive(x: &Item) -> bool { x.copying || x.ram.is_some() || exists(&x.path) }

fn save(items: &[Item]) {
    let keep: Vec<&Item> = items.iter().filter(|x| !x.copying && x.ram.is_none()).collect();
    let _ = std::fs::write(list_file(), serde_json::to_string(&keep).unwrap_or_default());
}

pub fn init() {
    let items: Vec<Item> = std::fs::read_to_string(list_file()).ok()
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let items: Vec<Item> = items.into_iter().filter(|x| exists(&x.path)).collect();
    // clean up leftovers in the default cache that are not in the list
    if let Ok(rd) = std::fs::read_dir(default_root()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !items.iter().any(|x| x.id == n) { let _ = std::fs::remove_dir_all(e.path()); }
        }
    }
    let _ = std::fs::remove_dir_all(export_root());
    *ITEMS.lock().unwrap() = items;
    *ICONS.lock().unwrap() = Some(HashMap::new());
    *EXPORTED.lock().unwrap() = Some(HashMap::new());
    start_clipboard_watch();
}

/// On exit: optionally write RAM items to disk (so they are on the shelf next launch)
pub fn on_exit() {
    let save_ram = settings::get().save_ram_on_exit;
    let mut g = ITEMS.lock().unwrap();
    if save_ram {
        for x in g.iter_mut() {
            if let Some(blob) = x.ram.take() {
                let dest = default_root().join(&x.id).join(&x.name);
                if write_blob(&blob, &dest).is_ok() { x.path = dest.to_string_lossy().to_string(); }
            }
        }
    }
    save(&g);
    let _ = std::fs::remove_dir_all(export_root());
}

/* ---------- export ---------- */
fn write_blob(blob: &Blob, dest: &Path) -> std::io::Result<()> {
    match blob {
        Blob::File(b) => { if let Some(p) = dest.parent() { std::fs::create_dir_all(p)?; } std::fs::write(dest, b) }
        Blob::Dir(files) => {
            std::fs::create_dir_all(dest)?;
            for (rel, b) in files {
                let p = dest.join(rel);
                if let Some(par) = p.parent() { std::fs::create_dir_all(par)?; }
                std::fs::write(p, b)?;
            }
            Ok(())
        }
    }
}

/// Item's path on disk; for a RAM item, writes it (once) to the temp folder
fn export_path(x: &Item) -> Option<String> {
    if x.copying { return None; }
    let Some(blob) = &x.ram else { return if exists(&x.path) { Some(x.path.clone()) } else { None } };
    if let Some(p) = EXPORTED.lock().unwrap().as_ref().and_then(|m| m.get(&x.id).cloned()) {
        if exists(&p) { return Some(p); }
    }
    let dest = export_root().join(&x.id).join(&x.name);
    write_blob(blob, &dest).ok()?;
    let p = dest.to_string_lossy().to_string();
    if let Some(m) = EXPORTED.lock().unwrap().as_mut() { m.insert(x.id.clone(), p.clone()); }
    Some(p)
}

fn items_by_ids(ids: &[String]) -> Vec<Item> {
    ITEMS.lock().unwrap().iter().filter(|x| ids.contains(&x.id) && !x.copying).cloned().collect()
}
fn ready_paths(ids: &[String]) -> Vec<String> { items_by_ids(ids).iter().filter_map(export_path).collect() }

/* ---------- send to UI ---------- */
fn send() {
    std::thread::spawn(|| {
        crate::shellx::thread_init();
        let items = {
            let mut g = ITEMS.lock().unwrap();
            g.retain(alive);   // drop disk items that were MOVED out
            save(&g);
            g.clone()
        };
        let out: Vec<serde_json::Value> = items.iter().map(|x| {
            let cached = ICONS.lock().unwrap().as_ref().and_then(|m| m.get(&x.id).cloned());
            let icon = cached.or_else(|| {
                if x.kind == "text" { return None; }
                let probe = if exists(&x.path) { x.path.clone() } else { x.orig.clone() };
                let ic = crate::shellx::icon_data_url(&probe, 96)?;
                if let Some(m) = ICONS.lock().unwrap().as_mut() { m.insert(x.id.clone(), ic.clone()); }
                Some(ic)
            }).unwrap_or_default();
            json!({ "id": x.id, "name": x.name, "copying": x.copying, "ram": x.ram.is_some(), "kind": x.kind,
                    "isDir": x.kind == "dir", "icon": icon, "text": x.text, "size": x.size })
        }).collect();
        emit("shelf", out);
        settings::broadcast_usage();
    });
}

/* ---------- adding ---------- */
fn dir_size(p: &Path) -> u64 {
    if p.is_file() { return std::fs::metadata(p).map(|m| m.len()).unwrap_or(0); }
    std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| dir_size(&e.path())).sum()).unwrap_or(0)
}
fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? { let e = e?; copy_recursive(&e.path(), &dst.join(e.file_name()))?; }
        Ok(())
    } else {
        if let Some(p) = dst.parent() { std::fs::create_dir_all(p)?; }
        std::fs::copy(src, dst).map(|_| ())
    }
}
fn read_tree(base: &Path, cur: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) -> std::io::Result<()> {
    for e in std::fs::read_dir(cur)? {
        let p = e?.path();
        if p.is_dir() { read_tree(base, &p, out)?; }
        else { out.push((p.strip_prefix(base).unwrap_or(&p).to_path_buf(), std::fs::read(&p)?)); }
    }
    Ok(())
}
/// Case-insensitive prefix check (NTFS and default APFS are both case-insensitive)
fn is_ours(p: &str) -> bool {
    let norm = |s: &str| {
        let mut x = if cfg!(windows) { s.replace('\\', "/") } else { s.to_string() }.to_lowercase();
        if !x.ends_with('/') { x.push('/'); }
        x
    };
    let lp = norm(p);
    [default_root(), root(), export_root()].iter().any(|r| lp.starts_with(&norm(&r.to_string_lossy())))
}
fn trim_count() {
    let mut g = ITEMS.lock().unwrap();
    while g.len() > MAX_ITEMS { let x = g.remove(0); drop_item_files(&x); }
}
fn drop_item_files(x: &Item) {
    if x.ram.is_none() && is_ours(&x.path) {
        if let Some(dir) = Path::new(&x.path).parent() { let _ = std::fs::remove_dir_all(dir); }
    }
    if let Some(p) = EXPORTED.lock().unwrap().as_mut().and_then(|m| m.remove(&x.id)) {
        if let Some(dir) = Path::new(&p).parent() { let _ = std::fs::remove_dir_all(dir); }
    }
    if let Some(m) = ICONS.lock().unwrap().as_mut() { m.remove(&x.id); }
}

fn add_paths(paths: Vec<String>) {
    let s = settings::get();
    for p in paths {
        // Windows shell APIs (icons) need backslashes
        let p = if cfg!(windows) { p.replace('/', "\\") } else { p };
        let pb = PathBuf::from(&p);
        if !pb.is_absolute() || !pb.exists() || is_ours(&p) { continue; }
        let is_dir = pb.is_dir();
        let size = dir_size(&pb);
        if used_bytes() + size > limit_bytes() {
            msg(&format!("{} ({})", l("Raf sınırı aşılıyor", "Shelf limit exceeded"), fmt_gb(limit_bytes() as f64)));
            continue;
        }
        let mut use_ram = s.storage == "ram";
        if use_ram && ram_used() + size > ram_limit_bytes() {
            use_ram = false;
            msg(l("RAM sınırı doldu, diske yazıldı", "RAM limit reached, stored on disk"));
        }
        let id = new_id();
        let name = pb.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.clone());
        let dest = if use_ram { String::new() } else { root().join(&id).join(&name).to_string_lossy().to_string() };
        // grab the icon from the original now (a RAM item has no file on disk)
        let item = Item { id: id.clone(), name, path: dest.clone(), orig: p.clone(), kind: if is_dir { "dir".into() } else { "file".into() },
            size, text: String::new(), copying: true, ram: None };
        ITEMS.lock().unwrap().push(item);
        std::thread::spawn(move || {
            crate::shellx::thread_init();
            if let Some(ic) = crate::shellx::icon_data_url(&p, 96) { if let Some(m) = ICONS.lock().unwrap().as_mut() { m.insert(id.clone(), ic); } }
            let src = Path::new(&p);
            let result: std::io::Result<Option<Arc<Blob>>> = if use_ram {
                if src.is_dir() { let mut v = Vec::new(); read_tree(src, src, &mut v).map(|_| Some(Arc::new(Blob::Dir(v)))) }
                else { std::fs::read(src).map(|b| Some(Arc::new(Blob::File(b)))) }
            } else { copy_recursive(src, Path::new(&dest)).map(|_| None) };
            {
                let mut g = ITEMS.lock().unwrap();
                match result {
                    Ok(ram) => { if let Some(x) = g.iter_mut().find(|x| x.id == id) { x.copying = false; x.ram = ram; } }
                    Err(_) => {
                        g.retain(|x| x.id != id);
                        if !dest.is_empty() { let _ = std::fs::remove_dir_all(Path::new(&dest).parent().unwrap_or(Path::new(&dest))); }
                    }
                }
            }
            send();
        });
    }
    trim_count();
    send();
}

/// Generated content (clipboard image / text): to RAM or disk depending on the setting
fn add_generated(name: String, kind: &str, bytes: Vec<u8>, text: String) {
    let size = bytes.len() as u64;
    if used_bytes() + size > limit_bytes() { msg(l("Raf sınırı aşılıyor", "Shelf limit exceeded")); return; }
    let id = new_id();
    let use_ram = settings::get().storage == "ram" && ram_used() + size <= ram_limit_bytes();
    let mut item = Item { id: id.clone(), name: name.clone(), path: String::new(), orig: String::new(), kind: kind.into(),
        size, text, copying: false, ram: None };
    if use_ram { item.ram = Some(Arc::new(Blob::File(bytes))); }
    else {
        let p = root().join(&id).join(&name);
        if p.parent().map(std::fs::create_dir_all).is_none() || std::fs::write(&p, bytes).is_err() { return; }
        item.path = p.to_string_lossy().to_string();
    }
    if kind == "image" {
        // prepare the icon now so the thumbnail can also be shown from RAM data
        if let Some(path) = export_path(&item) {
            let id2 = id.clone();
            std::thread::spawn(move || {
                crate::shellx::thread_init();
                if let Some(ic) = crate::shellx::icon_data_url(&path, 96) { if let Some(m) = ICONS.lock().unwrap().as_mut() { m.insert(id2, ic); } }
                send();
            });
        }
    }
    ITEMS.lock().unwrap().push(item);
    trim_count();
    send();
}

fn stamp() -> String {
    let (_, _, _, h, mi, se) = crate::util::local_time();
    format!("{:02}.{:02}.{:02}", h, mi, se)
}
fn add_text(text: String) {
    // do not add the same text again if it is already the latest item
    if ITEMS.lock().unwrap().last().map(|x| x.kind == "text" && x.text == preview(&text)).unwrap_or(false) { return; }
    let name = format!("{} {}.txt", l("Metin", "Text"), stamp());
    let pv = preview(&text);
    // UTF-8 BOM for Notepad (Windows); plain UTF-8 for TextEdit / Quick Look
    let mut bytes = if cfg!(windows) { vec![0xEF, 0xBB, 0xBF] } else { Vec::new() };
    bytes.extend_from_slice(text.as_bytes());
    add_generated(name, "text", bytes, pv);
}
fn preview(t: &str) -> String { t.trim().chars().take(160).collect() }
fn add_png(png: Vec<u8>) { add_generated(format!("{} {}.png", l("Pano", "Clipboard"), stamp()), "image", png, String::new()); }

fn remove(id: &str) {
    let mut g = ITEMS.lock().unwrap();
    if let Some(pos) = g.iter().position(|x| x.id == id) { let x = g.remove(pos); drop(g); drop_item_files(&x); }
}

/* ---------- clipboard history ---------- */
fn start_clipboard_watch() {
    std::thread::spawn(|| {
        let mut last = crate::shellx::clipboard_seq();
        loop {
            if !settings::get().clipboard_history {
                settings::wait_until(|s| s.clipboard_history);
                last = crate::shellx::clipboard_seq();
            }
            std::thread::sleep(Duration::from_millis(1000));
            let seq = crate::shellx::clipboard_seq();
            if seq == last { continue; }
            last = seq;
            if !settings::get().clipboard_history || seq == IGNORE_SEQ.load(Ordering::Relaxed) { continue; }
            if crate::shellx::clipboard_is_private() { continue; }   // password managers etc.
            match crate::shellx::clipboard_read() {
                crate::shellx::ClipContent::Files(f) => add_paths(f),
                crate::shellx::ClipContent::Png(png) => add_png(png),
                crate::shellx::ClipContent::Text(t) => add_text(t),
                crate::shellx::ClipContent::Empty => {}
            }
        }
    });
}

/// Keep the shelf's own clipboard writes out of the history
fn mark_own_clipboard() { IGNORE_SEQ.store(crate::shellx::clipboard_seq(), Ordering::Relaxed); }

fn copy_items(items: Vec<Item>) {
    // a single text card as text, everything else as files
    if items.len() == 1 && items[0].kind == "text" {
        let x = &items[0];
        let full = export_path(x).and_then(|p| std::fs::read_to_string(p).ok()).map(|s| s.trim_start_matches('\u{feff}').to_string());
        if crate::shellx::clipboard_set_text(&full.unwrap_or_else(|| x.text.clone())) { mark_own_clipboard(); msg(copied_msg()); }
        return;
    }
    let files: Vec<String> = items.iter().filter_map(export_path).collect();
    if files.is_empty() { return; }
    let _ = crate::util::app().run_on_main_thread(move || {
        if crate::shellx::clipboard_set_files(&files).is_ok() { mark_own_clipboard(); msg(copied_msg()); }
    });
}

fn copied_msg() -> &'static str {
    if cfg!(target_os = "macos") { l("Kopyalandı · ⌘V", "Copied · ⌘V") } else { l("Kopyalandı · Ctrl+V", "Copied · Ctrl+V") }
}

/* ---------- commands ---------- */
#[tauri::command] pub fn shelf_get() { send(); }
#[tauri::command] pub fn shelf_add(paths: Vec<String>) { std::thread::spawn(move || add_paths(paths)); }
#[tauri::command] pub fn shelf_remove(id: String) { remove(&id); send(); }
#[tauri::command] pub fn shelf_clear() {
    let ids: Vec<String> = ITEMS.lock().unwrap().iter().map(|x| x.id.clone()).collect();
    for id in ids { remove(&id); }
    send();
}
#[tauri::command] pub fn shelf_open(id: String) {
    std::thread::spawn(move || { if let Some(p) = ready_paths(&[id]).first() { shell_open(p); } });
}

#[tauri::command]
pub fn shelf_drag(window: WebviewWindow, ids: Vec<String>) {
    // RAM items are written to temp files first (most targets expect files on disk)
    let files = ready_paths(&ids);
    if files.is_empty() { return; }
    // macOS: native NSDraggingSession (Finder, Mail, Messages, Safari uploads ... accept it)
    #[cfg(target_os = "macos")]
    {
        crate::shellx::drag_files(&window, files, || {
            std::thread::spawn(|| { std::thread::sleep(Duration::from_millis(600)); send(); });
        });
    }
    #[cfg(windows)]
    {
    let hwnd = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    let app = window.app_handle().clone();
    let _ = app.run_on_main_thread(move || {
        crate::winutil::DRAGGING.store(true, Ordering::Relaxed);
        let _ = crate::shellx::drag_files(HWND(hwnd as *mut _), &files);
        crate::winutil::DRAGGING.store(false, Ordering::Relaxed);
        std::thread::spawn(|| { std::thread::sleep(Duration::from_millis(600)); send(); });
    });
    }
}

#[tauri::command]
pub fn shelf_copy(ids: Vec<String>) { let items = items_by_ids(&ids); std::thread::spawn(move || copy_items(items)); }

#[tauri::command]
pub fn shelf_paste() {
    std::thread::spawn(|| match crate::shellx::clipboard_read() {
        crate::shellx::ClipContent::Files(f) => add_paths(f),
        crate::shellx::ClipContent::Png(png) => add_png(png),
        crate::shellx::ClipContent::Text(t) => add_text(t),
        crate::shellx::ClipContent::Empty => msg(l("Panoda bir şey yok", "Clipboard is empty")),
    });
}

/* ---------- right-click menu ---------- */
#[tauri::command]
pub fn shelf_menu(window: WebviewWindow, id: String) -> Result<(), String> {
    let app = window.app_handle();
    let item = ITEMS.lock().unwrap().iter().find(|x| x.id == id).cloned();
    let Some(x) = item else { return Ok(()) };
    let e = |x: tauri::Error| x.to_string();
    let menu = Menu::with_items(app, &[
        &MenuItem::with_id(app, format!("sh-open:{}", x.id), settings::tr("sh_open"), !x.copying, None::<&str>).map_err(e)?,
        &MenuItem::with_id(app, format!("sh-copy:{}", x.id), settings::tr("sh_copy"), !x.copying, None::<&str>).map_err(e)?,
        &MenuItem::with_id(app, format!("sh-orig:{}", x.id), settings::tr("sh_orig"), exists(&x.orig), None::<&str>).map_err(e)?,
        &PredefinedMenuItem::separator(app).map_err(e)?,
        &MenuItem::with_id(app, format!("sh-remove:{}", x.id), settings::tr("sh_remove"), true, None::<&str>).map_err(e)?,
    ]).map_err(e)?;
    window.popup_menu(&menu).map_err(e)
}

pub fn on_menu(id: &str) {
    let Some((action, item_id)) = id.split_once(':') else { return };
    let item = ITEMS.lock().unwrap().iter().find(|x| x.id == item_id).cloned();
    let Some(x) = item else { return };
    match action {
        "sh-open" => { std::thread::spawn(move || { if let Some(p) = export_path(&x) { shell_open(&p); } }); }
        "sh-copy" => { std::thread::spawn(move || copy_items(vec![x])); }
        "sh-orig" => show_in_folder(&x.orig),
        "sh-remove" => { remove(&x.id); send(); }
        _ => {}
    }
}

/* ---------- for Settings ---------- */
#[tauri::command]
pub async fn pick_storage_dir() -> Result<serde_json::Value, String> {
    let title = l("Raf dosyalarının kaydedileceği klasör", "Folder for shelf files").to_string();
    let picked = std::thread::spawn(move || {
        crate::shellx::thread_init();
        crate::shellx::pick_folder(&title)
    }).join().ok().flatten();
    match picked {
        Some(p) => settings::set_setting("storageDir".into(), json!(p)),
        None => Ok(settings::snapshot()),
    }
}
