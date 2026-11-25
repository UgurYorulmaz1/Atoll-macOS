// Raw bindings for the C frameworks used by the macOS layer:
// CoreAudio, CoreMediaIO, IOKit, CoreGraphics (window list + event tap), ApplicationServices (AX), libdispatch.
#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use std::ffi::{c_char, c_void};

pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type CFTypeRef = *const c_void;

pub const fn fourcc(s: &[u8; 4]) -> u32 { u32::from_be_bytes(*s) }

/* ---------- CoreAudio ---------- */
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioObjectPropertyAddress { pub selector: u32, pub scope: u32, pub element: u32 }

pub const kAudioObjectSystemObject: AudioObjectID = 1;
pub const kScopeGlobal: u32 = fourcc(b"glob");
pub const kScopeOutput: u32 = fourcc(b"outp");
pub const kScopeInput: u32 = fourcc(b"inpt");
pub const kElementMain: u32 = 0;
pub const kDefaultOutputDevice: u32 = fourcc(b"dOut");
pub const kDefaultSystemOutputDevice: u32 = fourcc(b"sOut");
pub const kDevices: u32 = fourcc(b"dev#");
pub const kVirtualMainVolume: u32 = fourcc(b"vmvc");
pub const kVolumeScalar: u32 = fourcc(b"volm");
pub const kMute: u32 = fourcc(b"mute");
pub const kName: u32 = fourcc(b"lnam");
pub const kStreams: u32 = fourcc(b"stm#");
pub const kTransportType: u32 = fourcc(b"tran");
pub const kDeviceUID: u32 = fourcc(b"uid ");
pub const kIsRunningSomewhere: u32 = fourcc(b"gone");
pub const kTransportBluetooth: u32 = fourcc(b"blue");
pub const kTransportBluetoothLE: u32 = fourcc(b"blea");
pub const kTransportBuiltIn: u32 = fourcc(b"bltn");

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    pub fn AudioObjectHasProperty(id: AudioObjectID, addr: *const AudioObjectPropertyAddress) -> u8;
    pub fn AudioObjectIsPropertySettable(id: AudioObjectID, addr: *const AudioObjectPropertyAddress, out: *mut u8) -> OSStatus;
    pub fn AudioObjectGetPropertyDataSize(id: AudioObjectID, addr: *const AudioObjectPropertyAddress,
        qual_size: u32, qual: *const c_void, out_size: *mut u32) -> OSStatus;
    pub fn AudioObjectGetPropertyData(id: AudioObjectID, addr: *const AudioObjectPropertyAddress,
        qual_size: u32, qual: *const c_void, io_size: *mut u32, out: *mut c_void) -> OSStatus;
    pub fn AudioObjectSetPropertyData(id: AudioObjectID, addr: *const AudioObjectPropertyAddress,
        qual_size: u32, qual: *const c_void, size: u32, data: *const c_void) -> OSStatus;
}
// The virtual main volume ('vmvc') is provided by AudioToolbox's hardware service layer
#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {}

/* ---------- CoreMediaIO (camera in use) ---------- */
#[link(name = "CoreMediaIO", kind = "framework")]
extern "C" {
    pub fn CMIOObjectGetPropertyDataSize(id: u32, addr: *const AudioObjectPropertyAddress,
        qual_size: u32, qual: *const c_void, out_size: *mut u32) -> OSStatus;
    pub fn CMIOObjectGetPropertyData(id: u32, addr: *const AudioObjectPropertyAddress,
        qual_size: u32, qual: *const c_void, size: u32, used: *mut u32, out: *mut c_void) -> OSStatus;
}

/* ---------- IOKit ---------- */
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    pub fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, existing: *mut u32) -> i32;
    pub fn IOIteratorNext(it: u32) -> u32;
    pub fn IORegistryEntryCreateCFProperties(entry: u32, props: *mut *mut c_void, alloc: *const c_void, options: u32) -> i32;
    pub fn IOObjectRelease(o: u32) -> i32;
}

/* ---------- CoreGraphics ---------- */
pub type CGEventRef = *mut c_void;
pub type CGEventTapProxy = *mut c_void;
pub type CGEventTapCallBack = unsafe extern "C" fn(CGEventTapProxy, u32, CGEventRef, *mut c_void) -> CGEventRef;
pub const kCGSessionEventTap: u32 = 1;
pub const kCGHeadInsertEventTap: u32 = 0;
pub const kCGEventTapOptionDefault: u32 = 0;
pub const NX_SYSDEFINED: u32 = 14;
pub const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
pub const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;
pub const kCGEventFlagMaskShift: u64 = 0x0002_0000;
pub const kCGEventFlagMaskAlternate: u64 = 0x0008_0000;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *const c_void;
    pub fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, cb: CGEventTapCallBack, user: *mut c_void) -> *mut c_void;
    pub fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    pub fn CGEventGetFlags(ev: CGEventRef) -> u64;
    pub fn CGPreflightListenEventAccess() -> bool;
}

/* ---------- CoreFoundation run loop ---------- */
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub static kCFRunLoopCommonModes: CFTypeRef;
    pub fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
    pub fn CFRunLoopGetCurrent() -> *mut c_void;
    pub fn CFRunLoopAddSource(rl: *mut c_void, src: *mut c_void, mode: CFTypeRef);
    pub fn CFRunLoopRun();
    pub fn CFRelease(cf: CFTypeRef);
}

/* ---------- Accessibility ---------- */
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXIsProcessTrusted() -> bool;
}

/* ---------- libdispatch ---------- */
extern "C" {
    pub fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
}

/* ---------- helpers ---------- */

pub fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { selector, scope, element: kElementMain }
}

/// Reads a plain-old-data CoreAudio property
pub unsafe fn ca_get<T: Copy + Default>(id: AudioObjectID, a: AudioObjectPropertyAddress) -> Option<T> {
    if AudioObjectHasProperty(id, &a) == 0 { return None; }
    let mut v = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let st = AudioObjectGetPropertyData(id, &a, 0, std::ptr::null(), &mut size, &mut v as *mut T as *mut c_void);
    (st == 0).then_some(v)
}

pub unsafe fn ca_set<T: Copy>(id: AudioObjectID, a: AudioObjectPropertyAddress, v: T) -> bool {
    if AudioObjectHasProperty(id, &a) == 0 { return false; }
    let mut settable = 0u8;
    if AudioObjectIsPropertySettable(id, &a, &mut settable) != 0 || settable == 0 { return false; }
    AudioObjectSetPropertyData(id, &a, 0, std::ptr::null(), std::mem::size_of::<T>() as u32, &v as *const T as *const c_void) == 0
}

/// Reads an array-valued CoreAudio property (e.g. device list)
pub unsafe fn ca_get_vec<T: Copy + Default>(id: AudioObjectID, a: AudioObjectPropertyAddress) -> Vec<T> {
    let mut size = 0u32;
    if AudioObjectGetPropertyDataSize(id, &a, 0, std::ptr::null(), &mut size) != 0 || size == 0 { return vec![]; }
    let n = size as usize / std::mem::size_of::<T>();
    let mut v = vec![T::default(); n];
    if AudioObjectGetPropertyData(id, &a, 0, std::ptr::null(), &mut size, v.as_mut_ptr() as *mut c_void) != 0 { return vec![]; }
    v.truncate(size as usize / std::mem::size_of::<T>());
    v
}

/// Reads a CFString-valued CoreAudio property
pub unsafe fn ca_get_string(id: AudioObjectID, a: AudioObjectPropertyAddress) -> Option<String> {
    if AudioObjectHasProperty(id, &a) == 0 { return None; }
    let mut s: CFTypeRef = std::ptr::null();
    let mut size = std::mem::size_of::<CFTypeRef>() as u32;
    if AudioObjectGetPropertyData(id, &a, 0, std::ptr::null(), &mut size, &mut s as *mut CFTypeRef as *mut c_void) != 0 || s.is_null() {
        return None;
    }
    cfstring_take(s)
}

/// Takes ownership of a +1 CFStringRef and converts it
pub unsafe fn cfstring_take(s: CFTypeRef) -> Option<String> {
    let ns: objc2::rc::Retained<objc2_foundation::NSString> = objc2::rc::Retained::from_raw(s as *mut _)?;
    Some(ns.to_string())
}
