//! 内核堆（第二版）：空闲链表分配器。对应教程：docs/08-6-内核堆v2.md
//!
//! 每块内存前有 16 字节头部记录本块大小；空闲块串成按地址排序的链；
//! 分配走首次适配并切块，归还挂链并合并相邻空闲块。
//! 并发保护：所有操作包在 without_interrupts 里。
//!
//! 对齐处理：返回地址先向上对齐到 align，头部放在返回地址前面
//! 16 字节。align ≤ 16 时头部恰好落在块的原起点；align > 16 时
//! 头部之前会垫出一段对齐缝隙，成为不可复用的空洞（每个对齐分配
//! 最多损失 align-16 字节）——大对齐请求（如画布的 4096 对齐）
//! 整个生命周期只发生几次，代价可忽略。

use crate::mm;
use crate::sync;
use core::sync::atomic::{AtomicUsize, Ordering};

const HDR: usize = 16; // 头部大小
const MIN_BLOCK: usize = 32; // 最小块 = 头部 + 16B 载荷

/// 头部。已分配块：只有 size 有意义；空闲块：还挂在链上，next 生效
#[repr(C)]
struct Header {
    size: usize,       // 本块总字节数（含头部），恒为 16 的倍数
    next: *mut Header, // 空闲链上的下一块
}

static START: AtomicUsize = AtomicUsize::new(0);
static SIZE: AtomicUsize = AtomicUsize::new(0);
static FREE_HEAD: AtomicUsize = AtomicUsize::new(0); // 链头，0 = 空链

/// 向上取整到 a 的倍数（取整载荷、对齐返回地址用的是同一个运算）
fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

pub fn init(pages: u64) -> bool {
    let first = match mm::alloc_frame_contig(pages) {
        Some(p) => p,
        None => return false,
    };
    let start = mm::phys_to_virt(first) as usize;
    let size = (pages * mm::PAGE_SIZE) as usize;
    START.store(start, Ordering::Relaxed);
    SIZE.store(size, Ordering::Relaxed);

    // 链上只有一块：整堆
    unsafe {
        (*(start as *mut Header)).size = size;
        (*(start as *mut Header)).next = core::ptr::null_mut();
    }
    FREE_HEAD.store(start, Ordering::Relaxed);
    true
}

/// 分配 len 字节、按 align 对齐。堆里找不到就返回 None
pub fn alloc(len: usize, align: usize) -> Option<*mut u8> {
    let len = len.max(1);
    sync::without_interrupts(|| {
        let r = align_up(len, 16); // 载荷按 16 取整

        let mut prev: *mut Header = core::ptr::null_mut();
        let mut cur = FREE_HEAD.load(Ordering::Relaxed) as *mut Header;
        while !cur.is_null() {
            let base = cur as usize;
            let cur_end = base + unsafe { (*cur).size };
            // 返回地址向上对齐；头部在返回地址前面 16 字节
            let payload = align_up(base + HDR, align);
            let header = payload - HDR;
            let end = payload + r;

            if end <= cur_end {
                // 这块放得下。剩余够构成完整块就切块，否则整段吃下
                if cur_end - end >= MIN_BLOCK {
                    let rest = end as *mut Header;
                    unsafe {
                        (*rest).size = cur_end - end;
                        (*rest).next = (*cur).next; // rest 顶替 cur 的链位
                    }
                    if prev.is_null() {
                        FREE_HEAD.store(rest as usize, Ordering::Relaxed);
                    } else {
                        unsafe { (*prev).next = rest; }
                    }
                } else {
                    // 剩余不足以自立成块，并进本次分配（头部按延伸后的大小记）
                    if prev.is_null() {
                        unsafe { FREE_HEAD.store((*cur).next as usize, Ordering::Relaxed); }
                    } else {
                        unsafe { (*prev).next = (*cur).next; }
                    }
                    unsafe { (*(header as *mut Header)).size = cur_end - header; }
                    return Some(payload as *mut u8);
                }
                unsafe { (*(header as *mut Header)).size = end - header; }
                return Some(payload as *mut u8);
            }
            prev = cur;
            cur = unsafe { (*cur).next };
        }
        None // 扫完整条链：确实没有可满足的空间
    })
}

/// 归还。从头部恢复块大小，按地址顺序挂回链上，并合并相邻空闲块
pub fn dealloc(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    sync::without_interrupts(|| {
        let mut block = (ptr as usize - HDR) as *mut Header;
        let size = unsafe { (*block).size };

        // 按地址顺序找插入点（prev/next 是物理上的左右邻居；
        // 对齐缝隙会造成"隔空邻居"，合并条件不满足就自然跳过）
        let mut prev: *mut Header = core::ptr::null_mut();
        let mut cur = FREE_HEAD.load(Ordering::Relaxed) as *mut Header;
        while !cur.is_null() && (cur as usize) < (block as usize) {
            prev = cur;
            cur = unsafe { (*cur).next };
        }

        unsafe {
            // 先挂上链
            if prev.is_null() {
                FREE_HEAD.store(block as usize, Ordering::Relaxed);
            } else {
                (*prev).next = block;
            }
            (*block).next = cur;

            // 合并前邻：prev 的末尾正好接上本块的开头
            if !prev.is_null() && (prev as usize) + (*prev).size == block as usize {
                (*prev).size += size;
                (*prev).next = (*block).next;
                block = prev; // 合并后的块以 prev 为代表
            }
            // 合并后邻：本块的末尾正好接上 cur 的开头
            if (block as usize) + (*block).size == cur as usize {
                (*block).size += (*cur).size;
                (*block).next = (*cur).next;
            }
        }
    })
}

/// (空闲总量, 空闲块数)。分配/归还应当守恒——验证全靠它
pub fn stats() -> (usize, usize) {
    let mut total = 0;
    let mut count = 0;
    let mut cur = FREE_HEAD.load(Ordering::Relaxed) as *mut Header;
    while !cur.is_null() {
        total += unsafe { (*cur).size - HDR }; // 头部不计入空闲量
        count += 1;
        cur = unsafe { (*cur).next };
    }
    (total, count)
}

// ---------- 接入 Rust 分配接口 ----------
// Box/Vec 统一走 GlobalAlloc 接口，我们把接口的请求转给上面的堆

use core::alloc::{GlobalAlloc, Layout};

/// 挂在 #[global_allocator] 上的零大小标记类型
struct KernelHeap;

// unsafe：编译器对这里的实现零审查、全盘信任——
// 返回的指针对齐/重叠是否正确，全由实现者负责
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // 约定转换：接口要求"失败 = 空指针"，我们的堆返回 Option
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
