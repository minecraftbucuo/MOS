//! GDT 与 TSS：64 位下的段描述符表和任务状态段。
//!
//! 64 位模式下"分段"已基本退役（地址翻译全权归页表），
//! GDT 只剩两件正事：给段寄存器提供合法选择子、挂载 TSS。

use core::arch::asm;
use core::mem::size_of;

/// 段选择子 = 表内行号 × 8（低 3 位是别的标志，我们全 0）。
/// 中断系统要用它填 IDT 表项，所以公开
pub const KERNEL_CODE_SELECTOR: u16 = 1 << 3; // 0x08

const KERNEL_DATA_SELECTOR: u16 = 2 << 3; // 0x10
const TSS_SELECTOR: u16 = 3 << 3; // 0x18

// access 字节 0x9A = 1001 1010：P=1（在内存中）/ S=1（代码或数据段）
//                                  / type=1010（代码、可读）
// 标志位 0x2 << 52 = L=1（64 位模式）
const KERNEL_CODE_DESC: u64 = 0x00209A0000000000;
// access 0x92 = 1001 0010：type=0010（数据、可写）
const KERNEL_DATA_DESC: u64 = 0x0000920000000000;

/// 任务状态段（TSS）：CPU 的"应急信息卡"。
/// 普通中断直接用当前栈；但灾难性故障发生时当前栈可能已坏，
/// CPU 会按 IST1~7 换到备用栈再进处理程序。
/// 所有字段由 CPU 直接读取——Rust 看不见这些读取，与 BOOT_STACK 同理
#[allow(dead_code)]
#[repr(C, align(16))]
pub struct Tss {
    resvd0: u32,     // 布局固定
    rsp: [u64; 3],   // RSP0~2：将来用户态切内核栈用（还没有用户态）
    resvd1: u64,     // 布局固定
    ist: [u64; 7],   // IST1~7：七个备用栈顶（接双重故障时启用）
    resvd2: u32,     // 布局固定
    resvd3: u16,     // 布局固定
    iomap_base: u16, // I/O 权限位图偏移：指向段尾 = "没有位图"
}

impl Tss {
    const fn empty() -> Self {
        Self {
            resvd0: 0,
            rsp: [0; 3],
            resvd1: 0,
            ist: [0; 7],
            resvd2: 0,
            resvd3: 0,
            iomap_base: size_of::<Tss>() as u16,
        }
    }

    /// 设第 n 号备用栈顶（n: 0~6，对应 IST1~7）
    fn set_ist(&mut self, n: usize, stack_top: u64) {
        self.ist[n] = stack_top;
    }
}

/// GDT 表：[null, 代码, 数据, TSS 低, TSS 高]
static mut GDT: [u64; 5] = [0, KERNEL_CODE_DESC, KERNEL_DATA_DESC, 0, 0];
static mut TSS: Tss = Tss::empty();

/// 双故障应急栈：进 #DF 处理函数时当前栈很可能已坏，换这块干净的。
/// 数组本体没人读（CPU 只用它的地址当栈），消除误报
#[repr(align(16))]
#[allow(dead_code)]
struct EmergencyStack([u8; 16 * 1024]);

static mut EMERGENCY_STACK: EmergencyStack = EmergencyStack([0; 16 * 1024]);

/// GDTR：lgdt 指令读的"表在哪、多长"小卡片
#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

/// 装载 GDT 和 TSS。kmain 里、开中断前调用一次
pub fn init() {
    unsafe {
        // IST1 = 应急栈顶（栈向低地址长，顶 = 数组末尾）
        let tss = &raw mut TSS;
        (*tss).set_ist(
            0,
            (&raw const EMERGENCY_STACK as *const EmergencyStack as u64)
                + size_of::<EmergencyStack>() as u64,
        );

        // TSS 描述符：基址 = TSS 的地址，段长 = TSS 大小 - 1。
        // access 0x89 = P=1 / S=0（系统段）/ type=1001（64 位 TSS）
        let base = (&raw const TSS) as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let gdt = &raw mut GDT;
        // 裸指针要先解引用才能下标
        (*gdt)[3] = (limit & 0xFFFF)            // 段长低 16 位
            | ((base & 0xFFFFFF) << 16)     // 基址低 24 位
            | (0x89 << 40)                  // access 字节
            | (((limit >> 16) & 0xF) << 48); // 段长高 4 位
        (*gdt)[4] = base >> 32; // 基址高 32 位（64 位 TSS 特有）

        let gdtr = Gdtr {
            limit: (size_of::<[u64; 5]>() - 1) as u16,
            base: gdt as u64,
        };

        asm!(
            "lgdt [rdi]", // 装表
            // 数据段寄存器直接 mov 装填
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            // CS 不能 mov：压入新段 + 返回地址，远返回时换装。
            // 跳板用 r10——eax 全程装着数据段选择子，不能碰
            "push rsi",
            "lea r10, [rip + 2f]",
            "push r10",
            "retfq",
            "2:",
            "ltr dx", // TSS 装进任务寄存器
            in("rdi") &gdtr as *const Gdtr,
            in("eax") KERNEL_DATA_SELECTOR as u32,
            in("rsi") KERNEL_CODE_SELECTOR as u64,
            in("edx") TSS_SELECTOR as u32,
            out("r10") _,
        );
    }
}
