//! Limine Boot Protocol 请求标记（手写实现）。
//!
//! 机制：内核把"需求单"放进 `.limine_requests` 节区（见 main.rs 的
//! #[link_section]），Limine 加载内核时扫描整个 ELF 找到这个节，
//! 按单备货，再把"回执"指针写回每张单子的 response 字段。
//!
//! 魔数与结构布局照抄 Limine 协议规范（参考了 limine crate 0.6.5 的
//! 实现，该 crate 需要 nightly，我们手写以保持 stable 工具链）。

use core::cell::UnsafeCell;

/// 所有请求开头共用的接头暗号
const COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];

/// 帧缓冲请求。
///
/// response 字段启动前是空的；Limine 备好货后写进回执指针。
/// UnsafeCell：这个 static 会被引导器"从外部"修改，
/// 是内核里第一个合法的"可变静态"。
#[repr(C)]
pub struct FramebufferRequest {
    magic: [u64; 2],
    id: [u64; 2],
    revision: u64,
    response: UnsafeCell<*mut FramebufferResponse>,
}

// Sync = "可以被多个上下文共享引用"。含 UnsafeCell 的类型默认不是
// Sync；这里由我们担保：启动阶段单线程，读回执用 volatile，安全
unsafe impl Sync for FramebufferRequest {}

impl FramebufferRequest {
    /// 帧缓冲请求的 ID，Limine 协议规范写死的双 u64
    pub const fn new() -> Self {
        Self {
            magic: COMMON_MAGIC,
            id: [0x9d5827dcd881dd75, 0xa3148604f6fab11b],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null_mut()),
        }
    }

    /// 读回执。read_volatile：Limine 在运行期写入过这里，
    /// volatile 禁止编译器把"没人写过"的读取优化成缓存值
    pub fn response(&self) -> Option<&'static FramebufferResponse> {
        let ptr = unsafe { self.response.get().read_volatile() };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }
}

/// 帧缓冲响应：Limine 的"取件回执"
#[repr(C)]
pub struct FramebufferResponse {
    revision: u64,
    framebuffer_count: u64,
    framebuffers: *const *const Framebuffer,
}

impl FramebufferResponse {
    /// 所有屏幕（一般就一块）
    pub fn framebuffers(&self) -> &[&Framebuffer] {
        unsafe {
            core::slice::from_raw_parts(
                self.framebuffers as *const &Framebuffer,
                self.framebuffer_count as usize,
            )
        }
    }
}

/// 一块屏幕的描述：地址、分辨率、像素格式
#[repr(C)]
pub struct Framebuffer {
    address: *mut u8,
    pub width: u64,
    pub height: u64,
    /// 一行占多少字节（可能大于 宽×4，行尾有填充）
    pub pitch: u64,
    /// 每个像素多少位（一般 32）
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
    _resvd0: [u8; 7],
    edid_size: u64,
    edid: *const u8,
}

impl Framebuffer {
    pub fn address(&self) -> *mut u8 {
        self.address
    }
}

/// 内存映射请求：整机物理内存的区段清单。
/// 结构与帧缓冲请求同构（Limine 所有请求都长这样）
#[repr(C)]
pub struct MemmapRequest {
    magic: [u64; 2],
    id: [u64; 2],
    revision: u64,
    response: UnsafeCell<*mut MemmapResponse>,
}

unsafe impl Sync for MemmapRequest {}

impl MemmapRequest {
    pub const fn new() -> Self {
        Self {
            magic: COMMON_MAGIC,
            id: [0x67cf3d9d378a806f, 0xe304acdfc50c3c62],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null_mut()),
        }
    }

    pub fn response(&self) -> Option<&'static MemmapResponse> {
        let ptr = unsafe { self.response.get().read_volatile() };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { &*ptr })
        }
    }
}

/// 内存映射响应：区段指针数组的"取件回执"
#[repr(C)]
pub struct MemmapResponse {
    revision: u64,
    entry_count: u64,
    entries: *const *const MemmapEntry,
}

impl MemmapResponse {
    pub fn entries(&self) -> &[&MemmapEntry] {
        unsafe {
            core::slice::from_raw_parts(
                self.entries as *const &MemmapEntry,
                self.entry_count as usize,
            )
        }
    }
}

/// 一段连续的物理内存区段。
/// 协议里第三个字段名叫 type（Rust 关键字），这里改名 kind
#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,   // 区段起始的物理地址
    pub length: u64, // 字节数
    pub kind: u64,   // 类型：0=可用，其余见 kind_name
}

/// "这段内存是什么"——协议定死的类型值
pub const MEMMAP_USABLE: u64 = 0;

impl MemmapEntry {
    /// 类型值的可读名字（打印日志用）
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            0 => "usable",
            1 => "reserved",
            2 => "ACPI reclaimable",
            3 => "ACPI NVS",
            4 => "bad memory",
            5 => "bootloader reclaimable",
            6 => "kernel and modules",
            7 => "framebuffer",
            _ => "unknown",
        }
    }
}

/// HHDM（Higher Half Direct Map）请求：问"物理内存镜像在高地址的偏移量"。
///
/// Limine 把全部物理内存按固定偏移镜像进虚拟地址空间：
/// 物理地址 + 偏移 = 内核可直接读写的虚拟地址。第 05 章写显存
/// 用的 framebuffer 地址走的就是这条镜像，当时没点名
#[repr(C)]
pub struct HhdmRequest {
    magic: [u64; 2],
    id: [u64; 2],
    revision: u64,
    response: UnsafeCell<*mut HhdmResponse>,
}

unsafe impl Sync for HhdmRequest {}

impl HhdmRequest {
    pub const fn new() -> Self {
        Self {
            magic: COMMON_MAGIC,
            id: [0x48dcf1cb8ad2b852, 0x63984e959a98244b],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null_mut()),
        }
    }

    /// 回执就一个数：偏移量（一般 0xFFFF800000000000）
    pub fn response(&self) -> Option<u64> {
        let ptr = unsafe { self.response.get().read_volatile() };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { (*ptr).offset })
        }
    }
}

/// HHDM 响应：协议里最简的回执，只有一个偏移量
#[repr(C)]
struct HhdmResponse {
    revision: u64,
    offset: u64,
}
