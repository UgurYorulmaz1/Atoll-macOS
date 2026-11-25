// Windows shell helpers: file icon/preview (PNG), Explorer-compatible data object,
// native drag and drop (SHDoDragDrop), clipboard (file list / image)
use crate::util::{b64, wide};
use std::ffi::c_void;
use windows::core::{Interface, Result, GUID};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{OleFlushClipboard, OleSetClipboard, CF_DIB, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;

/// COM (STA) for worker threads that use shell APIs
pub fn thread_init() {
    unsafe { let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED); }
}

/* ---------- icon / preview ---------- */
/// Returns the file's shell preview (thumbnail for images, icon for others) as a PNG data URL.
/// COM (STA) must be initialized on the calling thread.
pub fn icon_data_url(path: &str, px: i32) -> Option<String> {
    unsafe {
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(&wide(path), None).ok()?;
        let hbmp = f.GetImage(SIZE { cx: px, cy: px }, SIIGBF_BIGGERSIZEOK).ok()?;
        let out = hbitmap_to_png(hbmp);
        let _ = DeleteObject(hbmp.into());
        out.map(|png| format!("data:image/png;base64,{}", b64(&png)))
    }
}

unsafe fn hbitmap_to_png(hbmp: HBITMAP) -> Option<Vec<u8>> {
    let mut bm = BITMAP::default();
    if GetObjectW(hbmp.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 { return None; }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    if w <= 0 || h <= 0 { return None; }
    let mut bi = BITMAPINFO::default();
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w, biHeight: -h, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(dc, hbmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if lines == 0 { return None; }
    bgra_to_png(&mut buf, w as u32, h as u32, true)
}

/// BGRA (top-down) -> PNG. If every alpha is 0 the image is opaque (happens with thumbnails).
fn bgra_to_png(buf: &mut [u8], w: u32, h: u32, premultiplied: bool) -> Option<Vec<u8>> {
    let all_zero_alpha = buf.chunks(4).all(|p| p[3] == 0);
    for p in buf.chunks_mut(4) {
        p.swap(0, 2);
        if all_zero_alpha { p[3] = 255; }
        else if premultiplied && p[3] > 0 && p[3] < 255 {
            let a = p[3] as u32;
            for c in 0..3 { p[c] = ((p[c] as u32 * 255 + a / 2) / a).min(255) as u8; }
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().ok()?;
        wr.write_image_data(buf).ok()?;
    }
    Some(out)
}

/* ---------- data object (a file list Explorer understands) ---------- */
pub fn data_object(paths: &[String]) -> Result<IDataObject> {
    unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::new();
        for p in paths {
            let id = ILCreateFromPathW(&wide(p));
            if !id.is_null() { pidls.push(id); }
        }
        let consts: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| *p as *const _).collect();
        let arr: Result<IShellItemArray> = SHCreateShellItemArrayFromIDLists(&consts);
        let res = arr.and_then(|a| a.BindToHandler::<_, IDataObject>(None::<&windows::Win32::System::Com::IBindCtx>, &BHID_DataObject));
        for p in pidls { ILFree(Some(p)); }
        res
    }
}

/// Starts a native Windows drag operation (must be called on the UI thread with OLE initialized). Blocks until dropped.
pub fn drag_files(hwnd: HWND, paths: &[String]) -> Result<DROPEFFECT> {
    let obj = data_object(paths)?;
    unsafe {
        let _ = ReleaseCapture();
        SHDoDragDrop(Some(hwnd), &obj, None, DROPEFFECT_COPY | DROPEFFECT_LINK)
    }
}

/// Put files on the clipboard (pasteable in Explorer with Ctrl+V). Must be called on the UI thread.
pub fn clipboard_set_files(paths: &[String]) -> Result<()> {
    let obj = data_object(paths)?;
    unsafe { OleSetClipboard(&obj)?; OleFlushClipboard() }
}

pub enum ClipContent { Files(Vec<String>), Png(Vec<u8>), Text(String), Empty }

/// Should the clipboard content be kept out of the history? (password managers flag this)
pub fn clipboard_is_private() -> bool {
    unsafe {
        for name in ["ExcludeClipboardContentFromMonitorProcessing", "Clipboard Viewer Ignore"] {
            let f = RegisterClipboardFormatW(&wide(name));
            if f != 0 && IsClipboardFormatAvailable(f).is_ok() { return true; }
        }
        // skip history if CanIncludeInClipboardHistory = 0
        let f = RegisterClipboardFormatW(&wide("CanIncludeInClipboardHistory"));
        if f != 0 && IsClipboardFormatAvailable(f).is_ok() && OpenClipboard(None).is_ok() {
            let mut private = false;
            if let Ok(h) = GetClipboardData(f) {
                let g = HGLOBAL(h.0);
                let p = GlobalLock(g) as *const u32;
                if !p.is_null() { private = *p == 0; let _ = GlobalUnlock(g); }
            }
            let _ = CloseClipboard();
            return private;
        }
        false
    }
}

pub fn clipboard_seq() -> u32 { unsafe { GetClipboardSequenceNumber() } }

/// Put text on the clipboard
pub fn clipboard_set_text(text: &str) -> bool {
    unsafe {
        if OpenClipboard(None).is_err() { return false; }
        let _ = EmptyClipboard();
        let w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut ok = false;
        if let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
            let p = GlobalLock(g) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                let _ = GlobalUnlock(g);
                ok = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(g.0))).is_ok();
            }
        }
        let _ = CloseClipboard();
        ok
    }
}

/// Folder picker dialog (must be called on an STA thread)
pub fn pick_folder(title: &str) -> Option<String> {
    unsafe {
        let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = d.GetOptions().ok()?;
        d.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = d.SetTitle(&wide(title));
        d.Show(None).ok()?;
        let item = d.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// Physical RAM (bytes)
pub fn total_ram() -> u64 {
    unsafe {
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if GlobalMemoryStatusEx(&mut m).is_ok() { m.ullTotalPhys } else { 8 << 30 }
    }
}

/// Read the file list, image or text from the clipboard
pub fn clipboard_read() -> ClipContent {
    unsafe {
        let mut opened = false;
        for _ in 0..4 {
            if OpenClipboard(None).is_ok() { opened = true; break; }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        if !opened { return ClipContent::Empty; }
        let res = (|| {
            if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
                if let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) {
                    let drop = HDROP(h.0);
                    let n = DragQueryFileW(drop, u32::MAX, None);
                    let mut files = Vec::new();
                    for i in 0..n {
                        let len = DragQueryFileW(drop, i, None) as usize;
                        let mut b = vec![0u16; len + 1];
                        DragQueryFileW(drop, i, Some(&mut b));
                        files.push(String::from_utf16_lossy(&b[..len]));
                    }
                    if !files.is_empty() { return ClipContent::Files(files); }
                }
            }
            if IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok() {
                if let Ok(h) = GetClipboardData(CF_DIB.0 as u32) {
                    if let Some(png) = dib_to_png(h) { return ClipContent::Png(png); }
                }
            }
            if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
                if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
                    let g = HGLOBAL(h.0);
                    let p = GlobalLock(g) as *const u16;
                    if !p.is_null() {
                        let max = GlobalSize(g) / 2;
                        let mut n = 0usize;
                        while n < max && *p.add(n) != 0 { n += 1; }
                        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                        let _ = GlobalUnlock(g);
                        if !s.trim().is_empty() { return ClipContent::Text(s); }
                    }
                }
            }
            ClipContent::Empty
        })();
        let _ = CloseClipboard();
        res
    }
}

unsafe fn dib_to_png(h: HANDLE) -> Option<Vec<u8>> {
    let g = HGLOBAL(h.0);
    let ptr = GlobalLock(g) as *const u8;
    if ptr.is_null() { return None; }
    let size = GlobalSize(g);
    let out = (|| {
        let hdr = &*(ptr as *const BITMAPINFOHEADER);
        let (w, hh, bpp) = (hdr.biWidth, hdr.biHeight, hdr.biBitCount);
        if w <= 0 || hh == 0 || (bpp != 32 && bpp != 24) { return None; }
        let h_abs = hh.unsigned_abs();
        let mut off = hdr.biSize as usize;
        if hdr.biCompression == BI_BITFIELDS.0 { off += 12; }
        let stride = ((w as usize * bpp as usize + 31) / 32) * 4;
        if off + stride * h_abs as usize > size { return None; }
        let mut rgba = vec![0u8; w as usize * h_abs as usize * 4];
        for y in 0..h_abs as usize {
            let src_row = if hh > 0 { h_abs as usize - 1 - y } else { y };   // may be stored bottom-up
            let src = ptr.add(off + src_row * stride);
            for x in 0..w as usize {
                let s = src.add(x * (bpp as usize / 8));
                let d = (y * w as usize + x) * 4;
                rgba[d] = *s; rgba[d + 1] = *s.add(1); rgba[d + 2] = *s.add(2);
                rgba[d + 3] = if bpp == 32 { *s.add(3) } else { 255 };
            }
        }
        bgra_to_png(&mut rgba, w as u32, h_abs, false)
    })();
    let _ = GlobalUnlock(g);
    out
}

#[allow(dead_code)]
const _UNUSED: GUID = GUID::zeroed();
#[allow(dead_code)]
fn _iface_check(x: &IDataObject) -> windows::core::IUnknown { x.cast().unwrap() }
