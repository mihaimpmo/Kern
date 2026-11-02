//! Limine boot protocol requests (see https://github.com/limine-bootloader/limine-protocol).
//! The bootloader scans the `.limine_requests` section, fills in the
//! `response` pointers, then jumps to `kmain`.

use core::cell::UnsafeCell;
use core::ffi::{c_char, CStr};
use core::sync::atomic::AtomicU64;

const COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];

#[repr(C)]
pub struct Request<T> {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const T>,
}

unsafe impl<T> Sync for Request<T> {}

impl<T> Request<T> {
    const fn new(a: u64, b: u64) -> Self {
        Self { id: [COMMON_MAGIC[0], COMMON_MAGIC[1], a, b], revision: 0, response: UnsafeCell::new(core::ptr::null()) }
    }

    pub fn response(&self) -> Option<&'static T> {
        let ptr = unsafe { core::ptr::read_volatile(self.response.get()) };
        unsafe { ptr.as_ref() }
    }
}

#[repr(C)]
pub struct BaseRevision(UnsafeCell<[u64; 3]>);
unsafe impl Sync for BaseRevision {}

impl BaseRevision {
    /// The bootloader sets the last word to 0 if it supports our revision.
    pub fn is_supported(&self) -> bool {
        unsafe { core::ptr::read_volatile(self.0.get())[2] == 0 }
    }
}

// ----------------------------------------------------------------- responses

#[repr(C)]
pub struct BootloaderInfo {
    revision: u64,
    name: *const c_char,
    version: *const c_char,
}

impl BootloaderInfo {
    pub fn name(&self) -> &str {
        unsafe { CStr::from_ptr(self.name) }.to_str().unwrap_or("?")
    }
    pub fn version(&self) -> &str {
        unsafe { CStr::from_ptr(self.version) }.to_str().unwrap_or("?")
    }
}

#[repr(C)]
pub struct HhdmResponse {
    revision: u64,
    pub offset: u64,
}

#[repr(C)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

#[repr(C)]
pub struct FramebufferResponse {
    revision: u64,
    count: u64,
    framebuffers: *const *const Framebuffer,
}

impl FramebufferResponse {
    pub fn first(&self) -> Option<&'static Framebuffer> {
        if self.count == 0 {
            return None;
        }
        unsafe { (*self.framebuffers).as_ref() }
    }
}

#[allow(dead_code)]
pub mod memmap_type {
    pub const USABLE: u64 = 0;
    pub const RESERVED: u64 = 1;
    pub const ACPI_RECLAIMABLE: u64 = 2;
    pub const ACPI_NVS: u64 = 3;
    pub const BAD_MEMORY: u64 = 4;
    pub const BOOTLOADER_RECLAIMABLE: u64 = 5;
    pub const EXECUTABLE_AND_MODULES: u64 = 6;
    pub const FRAMEBUFFER: u64 = 7;
}

#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

#[repr(C)]
pub struct MemmapResponse {
    revision: u64,
    count: u64,
    entries: *const *const MemmapEntry,
}

impl MemmapResponse {
    pub fn entries(&self) -> impl Iterator<Item = &'static MemmapEntry> {
        let entries = self.entries;
        (0..self.count as usize).map(move |i| unsafe { &**entries.add(i) })
    }
}

#[repr(C)]
pub struct RsdpResponse {
    revision: u64,
    pub address: u64,
}

#[repr(C)]
pub struct CmdlineResponse {
    revision: u64,
    cmdline: *const c_char,
}

impl CmdlineResponse {
    pub fn cmdline(&self) -> &'static str {
        if self.cmdline.is_null() {
            return "";
        }
        unsafe { CStr::from_ptr(self.cmdline) }.to_str().unwrap_or("")
    }
}

/// One CPU as reported by the MP request. Limine parks it until something
/// is stored in `goto_address`; it then jumps there on its own 64 KiB stack,
/// with a pointer to this struct in rdi.
#[repr(C)]
pub struct MpInfo {
    pub processor_id: u32,
    pub lapic_id: u32,
    _reserved: u64,
    pub goto_address: AtomicU64,
    pub extra_argument: AtomicU64,
}

#[repr(C)]
pub struct MpResponse {
    revision: u64,
    flags: u32,
    pub bsp_lapic_id: u32,
    count: u64,
    cpus: *const *const MpInfo,
}

impl MpResponse {
    pub fn cpus(&self) -> impl Iterator<Item = &'static MpInfo> {
        let cpus = self.cpus;
        (0..self.count as usize).map(move |i| unsafe { &**cpus.add(i) })
    }
}

/// The MP request is the only one with a field after `response`.
#[repr(C)]
pub struct MpRequest {
    request: Request<MpResponse>,
    flags: u64,
}

impl MpRequest {
    pub fn response(&self) -> Option<&'static MpResponse> {
        self.request.response()
    }
}

#[repr(C)]
pub struct File {
    revision: u64,
    pub address: *const u8,
    pub size: u64,
    path: *const c_char,
    string: *const c_char,
}

impl File {
    pub fn path(&self) -> &'static str {
        unsafe { CStr::from_ptr(self.path) }.to_str().unwrap_or("?")
    }
    pub fn data(&self) -> &'static [u8] {
        unsafe { core::slice::from_raw_parts(self.address, self.size as usize) }
    }
}

#[repr(C)]
pub struct ModuleResponse {
    revision: u64,
    count: u64,
    modules: *const *const File,
}

impl ModuleResponse {
    pub fn modules(&self) -> impl Iterator<Item = &'static File> {
        let modules = self.modules;
        (0..self.count as usize).map(move |i| unsafe { &**modules.add(i) })
    }
}

// ------------------------------------------------------------------ requests

#[used]
#[link_section = ".limine_requests_start"]
static START_MARKER: [u64; 4] = [0xf6b8f4b39de7d1ae, 0xfab91a6940fcb9cf, 0x785c6ed015d3e316, 0x181e920a7852b9d9];

/// Base revision 5: well-defined APIC state, ACPI tables mapped in the HHDM.
#[used]
#[link_section = ".limine_requests"]
pub static BASE_REVISION: BaseRevision = BaseRevision(UnsafeCell::new([0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, 5]));

#[used]
#[link_section = ".limine_requests"]
pub static BOOTLOADER_INFO: Request<BootloaderInfo> = Request::new(0xf55038d8e2a1202f, 0x279426fcf5f59740);

#[used]
#[link_section = ".limine_requests"]
pub static HHDM: Request<HhdmResponse> = Request::new(0x48dcf1cb8ad2b852, 0x63984e959a98244b);

#[used]
#[link_section = ".limine_requests"]
pub static FRAMEBUFFER: Request<FramebufferResponse> = Request::new(0x9d5827dcd881dd75, 0xa3148604f6fab11b);

#[used]
#[link_section = ".limine_requests"]
pub static MEMMAP: Request<MemmapResponse> = Request::new(0x67cf3d9d378a806f, 0xe304acdfc50c3c62);

#[used]
#[link_section = ".limine_requests"]
pub static RSDP: Request<RsdpResponse> = Request::new(0xc5e77b6b397e7b43, 0x27637845accdcf3c);

#[used]
#[link_section = ".limine_requests"]
pub static MODULES: Request<ModuleResponse> = Request::new(0x3e7e279702be32af, 0xca1c4f3bd1280cee);

/// Start the other CPUs parked (flags = 0: leave the APIC mode as is).
#[used]
#[link_section = ".limine_requests"]
pub static MP: MpRequest = MpRequest { request: Request::new(0x95a67b819a1b857e, 0xa0b61b723b6a73e0), flags: 0 };

/// The `cmdline:` line of the boot entry in limine.conf.
#[used]
#[link_section = ".limine_requests"]
pub static CMDLINE: Request<CmdlineResponse> = Request::new(0x4b161536e598651e, 0xb390ad4a2f1f303a);

#[used]
#[link_section = ".limine_requests_end"]
static END_MARKER: [u64; 2] = [0xadc0e0531bb10d03, 0x9572709f31764c62];
