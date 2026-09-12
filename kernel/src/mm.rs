//! 物理页帧分配器：位图版。对应教程：docs/08-2-位图分配器.md
//!
//! "页帧"（frame）= 一个 4KB 物理页。分配器只管"哪页空闲、发给谁"，
//! 不管虚拟地址怎么映射——那是页表的事，各管一段。

use crate::boot::{self, MemmapResponse};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

/// x86_64 标准页尺寸
pub const PAGE_SIZE: u64 = 4096;

/// Limine 的物理内存直映射偏移（init 时记下，全内核共用）
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// 物理地址 → 内核可直接访问的虚拟地址（HHDM 直映射）
pub fn phys_to_virt(p: u64) -> *mut u8 {
    (p + HHDM_OFFSET.load(Ordering::Relaxed)) as *mut u8
}

/// 位图分配器：每 4KB 物理页对应 1 比特，1 = 空闲、0 = 已占。
/// 初始全 0（全占），usable 区间逐段点亮——
/// 保留区、MMIO、内核占的区天然不用专门处理
struct FrameAllocator {
    bitmap: *mut u8, // 位图本体（虚拟地址，借住在某段可用内存里）
    frames: u64,     // 物理页总数
    next_hint: u64,  // 上次发到哪（下次从这里接着找，免得从头扫）
}

impl FrameAllocator {
    /// 按内存清单建分配器。返回 None = 找不到放位图的地方
    fn from_memmap(resp: &MemmapResponse) -> Option<Self> {
        // 总页数：只数 usable 区的最高末端。清单里那些高位大段
        // reserved 是 MMIO 洞不是内存条，数进去位图自己就被撑爆了
        let mut top = 0u64;
        for e in resp.entries() {
            if e.kind == boot::MEMMAP_USABLE {
                top = top.max(e.base + e.length);
            }
        }
        let frames = top / PAGE_SIZE;
        let bitmap_len = (frames as usize + 7) / 8; // 每比特一页，向上取整

        // 自举问题：位图自己也是块内存，分配器还没建好，只能手工占——
        // 扫清单找第一段放得下的 usable 区借住
        let mut bitmap_phys = None;
        for e in resp.entries() {
            if e.kind == boot::MEMMAP_USABLE && e.length >= bitmap_len as u64 {
                bitmap_phys = Some(e.base);
                break;
            }
        }
        let bitmap_phys = bitmap_phys?;

        let mut this = Self {
            bitmap: phys_to_virt(bitmap_phys),
            frames,
            next_hint: 0,
        };

        unsafe {
            // 位图清零 = 全部"已占"，接着只把 usable 点亮
            core::ptr::write_bytes(this.bitmap, 0, bitmap_len);

            // usable 区间逐页点亮。头向上取整、尾向下取整——
            // 区间两端不完整的页干脆丢弃（地址不齐的页没法按页管）
            for e in resp.entries() {
                if e.kind != boot::MEMMAP_USABLE {
                    continue;
                }
                let start = (e.base + PAGE_SIZE - 1) / PAGE_SIZE;
                let end = (e.base + e.length) / PAGE_SIZE;
                for pfn in start..end {
                    this.set_free(pfn);
                }
            }

            // 位图自己借住的那几页标回"已占"——自己不能把自己发出去
            let bmp_pfn = bitmap_phys / PAGE_SIZE;
            let bmp_pages = (bitmap_len as u64 + PAGE_SIZE - 1) / PAGE_SIZE;
            for pfn in bmp_pfn..bmp_pfn + bmp_pages {
                this.set_used(pfn);
            }
        }

        Some(this)
    }

    /// 置 1（空闲）：定位到字节，点亮对应位
    fn set_free(&mut self, pfn: u64) {
        unsafe {
            *self.bitmap.add((pfn / 8) as usize) |= 1 << (pfn % 8);
        }
    }

    /// 清 0（占用）
    fn set_used(&mut self, pfn: u64) {
        unsafe {
            *self.bitmap.add((pfn / 8) as usize) &= !(1 << (pfn % 8));
        }
    }

    /// 数一遍空闲页（开机统计用；以后这函数会变得很慢——那是后话）
    fn count_free(&self) -> u64 {
        let bitmap_len = (self.frames as usize + 7) / 8;
        let mut free = 0u64;
        unsafe {
            for i in 0..bitmap_len {
                free += (*self.bitmap.add(i)).count_ones() as u64;
            }
        }
        free
    }
}

// ---------- 全局单例（Shared 模式第三次登场，第 09 章抽成公共模块） ----------

/// 共享包装：UnsafeCell + 手写 Sync（担保理由同 serial/console）
struct Shared<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Shared<T> {}
impl<T> Shared<T> {
    const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }
    fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

static ALLOCATOR: Shared<Option<FrameAllocator>> = Shared::new(None);

/// 建分配器（kmain 调一次）。返回 false = 没地方放位图
pub fn init(resp: &MemmapResponse, hhdm_offset: u64) -> bool {
    HHDM_OFFSET.store(hhdm_offset, Ordering::Relaxed);
    match FrameAllocator::from_memmap(resp) {
        Some(a) => {
            *ALLOCATOR.get() = Some(a);
            true
        }
        None => false,
    }
}

/// 家底统计：（总页数, 空闲页数）
pub fn stats() -> (u64, u64) {
    match ALLOCATOR.get() {
        Some(a) => (a.frames, a.count_free()),
        None => (0, 0),
    }
}

/// 发一页：返回这页的起始物理地址。没空页返回 None
pub fn alloc_frame() -> Option<u64> {
    let a = ALLOCATOR.get().as_mut()?;
    let bitmap_len = (a.frames as usize + 7) / 8;

    // 从游标处往后找第一个非零字节（非零 = 里面有空闲位）。
    // next_hint 记的是页号，一个字节管 8 页——先除以 8 换算成字节下标
    let mut i = (a.next_hint / 8) as usize;
    while i < bitmap_len {
        let byte = unsafe { *a.bitmap.add(i) };
        if byte != 0 {
            // trailing_zeros：最低位的 1 在第几位——顺手的位运算找位法
            let pfn = (i as u64) * 8 + byte.trailing_zeros() as u64;
            a.set_used(pfn);
            a.next_hint = pfn; // 游标跟上
            return Some(pfn * PAGE_SIZE);
        }
        i += 1;
    }
    None // 位图里再没有 1：内存发完了
}

/// 收回一页（参数是 alloc_frame 当时给的物理地址）
pub fn free_frame(phys: u64) {
    if let Some(a) = ALLOCATOR.get().as_mut() {
        let pfn = phys / PAGE_SIZE;
        a.set_free(pfn);
        // 收回的页排在游标前面的话，游标退回去——
        // 空闲页优先重发，位图前段不会越积越稀
        if pfn < a.next_hint {
            a.next_hint = pfn;
        }
    }
}
