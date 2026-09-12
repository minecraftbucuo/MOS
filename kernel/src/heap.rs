//! 内核堆（第一版）：bump allocator。对应教程：docs/08-4-内核堆.md
//!
//! 页帧分配器（mm.rs）只按整页 4KB 分配；堆垫在它上面，
//! 按任意字节数分配、随时归还。
//!
//! bump 分配：游标只往上移，分配 = 游标前移。
//! 代价是 free 不回收内存——第一版的诚实缺陷，以后换真正的分配器。

use crate::mm;
use core::sync::atomic::{AtomicUsize, Ordering};

// 三个原子量 = 堆的全部状态。0 表示还没建堆
static START: AtomicUsize = AtomicUsize::new(0); // 堆底（虚拟地址）
static SIZE: AtomicUsize = AtomicUsize::new(0); // 总大小（字节）
static TOP: AtomicUsize = AtomicUsize::new(0); // 游标：已用到的偏移

/// 建堆：从页帧分配器连拿 pages 页。
/// 返回 false = 页不够、或拿到的页物理不连续
///（bump 堆靠 HHDM 直映射用内存，物理连续才在虚拟地址上连成一块）
pub fn init(pages: u64) -> bool {
    let mut first = 0u64;
    for i in 0..pages {
        match mm::alloc_frame() {
            Some(p) => {
                if i == 0 {
                    first = p;
                } else if p != first + i * mm::PAGE_SIZE {
                    return false; // 不连续：简化处理，直接宣布失败
                }
            }
            None => return false,
        }
    }

    START.store(mm::phys_to_virt(first) as usize, Ordering::Relaxed);
    SIZE.store((pages * mm::PAGE_SIZE) as usize, Ordering::Relaxed);
    TOP.store(0, Ordering::Relaxed);
    true
}

/// 分配 len 字节、按 align 对齐。堆满返回 None
///
/// 注：load/store 不是原子读改写，这里默认"单核、且中断处理函数
/// 不碰堆"；真正要并发安全时（多核），这里得换成 CAS 循环
pub fn alloc(len: usize, align: usize) -> Option<*mut u8> {
    let start = START.load(Ordering::Relaxed);
    if start == 0 {
        return None; // 堆还没建
    }
    let size = SIZE.load(Ordering::Relaxed);
    let mut top = TOP.load(Ordering::Relaxed);

    // 对齐：游标不够齐就垫高到 align 的倍数（垫的缝隙白送，不记账）
    top = (top + align - 1) & !(align - 1);
    if top + len > size {
        return None; // 堆满。bump 堆不回收，满 = 完
    }
    TOP.store(top + len, Ordering::Relaxed);
    Some((start + top) as *mut u8)
}

/// 归还。bump 版：什么都不做（下轮接 GlobalAlloc 时它必须在场）
pub fn dealloc(_ptr: *mut u8) {}

// ---------- 接入 Rust 分配世界 ----------
// Box/Vec 不自己分配，全走 GlobalAlloc 这扇门；
// 我们把门后的活指给上面的 bump 堆

use core::alloc::{GlobalAlloc, Layout};

/// 挂在 #[global_allocator] 上的零大小标记类型
struct KernelHeap;

// unsafe：编译器对这里的实现零审查、全盘信任——
// 返回的指针不对齐/重叠，上层 UB，全是我们兜
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // 协议翻译：trait 要"失败 = 空指针"，我们的堆说 Option
        match alloc(layout.size(), layout.align()) {
            Some(ptr) => ptr,
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        dealloc(ptr);
    }
}

#[global_allocator]
static ALLOCATOR: KernelHeap = KernelHeap;
