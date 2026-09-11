//! IDT：中断描述符表——"门铃编号 → 处理函数"的电话簿。
//!
//! 处理函数入口用手写汇编存根（stable 工具链没有 x86-interrupt
//! 调用约定，和当初的 limine crate 同一个处境）。

use crate::gdt::KERNEL_CODE_SELECTOR;
use crate::keyboard;
use crate::pic;
use crate::serial;
use core::arch::asm;
use core::arch::naked_asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

/// 中断现场的完整布局（字段从低地址到高地址，与存根压栈顺序一致）。
/// 寄存器字段是给调试输出留的，目前只读 rip——CPU 看不见这些，同 BOOT_STACK
#[allow(dead_code)]
#[repr(C)]
pub struct IsrFrame {
    // 通用寄存器（按压栈顺序）
    pub rax: u64, pub rcx: u64, pub rdx: u64, pub rbx: u64,
    pub rbp: u64, pub rsi: u64, pub rdi: u64,
    pub r8: u64, pub r9: u64, pub r10: u64, pub r11: u64,
    pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
    // 错误码（无错误码的异常由存根压 0 占位）
    pub error_code: u64,
    // 以下是 CPU 进入存根前自动压的
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// 生成一个中断存根。
/// $pre：无错误码的异常传 "push 0" 占位；有错误码的传 "nop"
///（CPU 已经压了真错误码，不能再压一份）
macro_rules! stub {
    ($name:ident, $handler:path, $pre:literal) => {
        #[unsafe(naked)]
        extern "C" fn $name() -> ! {
            naked_asm!(
                $pre,
                // 保存全部通用寄存器：被打断的代码不知道自己被中断过，
                // 一个都不能弄脏
                "push r15", "push r14", "push r13", "push r12",
                "push r11", "push r10", "push r9", "push r8",
                "push rdi", "push rsi", "push rbp", "push rbx",
                "push rdx", "push rcx", "push rax",
                // rbp 已保存在栈上，借来记帧位置并摆正栈：
                // Rust 函数要求进入时 rsp 按 16 对齐，而被打断的
                // 代码栈可没这个保证
                "mov rbp, rsp",
                "and rsp, -16",
                "mov rdi, rbp", // 第一个参数 = 帧指针
                "call {h}",
                "mov rsp, rbp", // 回到帧，开始恢复
                "pop rax", "pop rcx", "pop rdx", "pop rbx",
                "pop rbp", "pop rsi", "pop rdi",
                "pop r8", "pop r9", "pop r10", "pop r11",
                "pop r12", "pop r13", "pop r14", "pop r15",
                "add rsp, 8", // 丢弃错误码占位（或 CPU 压的真错误码）
                "iretq",
                h = sym $handler,
            );
        }
    };
}

// ---------- 异常处理函数（纯 Rust，从存根进入） ----------

/// 公用：汇报异常名和出事地址
fn report(name: &str, frame: &IsrFrame) {
    serial::print("\n[exception] ");
    serial::print(name);
    serial::print(" rip=");
    serial::print_hex(frame.rip);
    serial::print("\n");
}

extern "C" fn exc_divide_error(frame: &mut IsrFrame) {
    report("#DE 除零", frame);
}

extern "C" fn exc_breakpoint(frame: &mut IsrFrame) {
    report("#BP 断点", frame);
}

extern "C" fn exc_invalid_opcode(frame: &mut IsrFrame) {
    report("#UD 非法指令", frame);
}

/// 双重故障：异常处理过程中又出了异常。当前栈很可能已坏，
/// 所以它走 IST1 备用栈进门（见 IDT 登记处）。到这里没有"恢复"可言，
/// 打印遗言后停机——它的价值是把"无声复位"变成"留了句话"
extern "C" fn exc_double_fault(frame: &mut IsrFrame) -> ! {
    report("#DF 双重故障", frame);
    loop {
        unsafe { asm!("hlt") };
    }
}

/// 兜底：没登记处理逻辑的异常都到这。不返回——直接停机
extern "C" fn exc_unexpected(frame: &mut IsrFrame) -> ! {
    report("unexpected", frame);
    loop {
        unsafe { asm!("hlt") }; // 休眠等中断，比忙转省电
    }
}

// 存根：除零/断点/非法指令无错误码，压 0 占位；
// 双重故障有错误码（CPU 自己压），用 nop 顶替那行；
// 时钟/键盘中断（IRQ）也无错误码
stub!(stub_divide_error, exc_divide_error, "push 0");
stub!(stub_breakpoint, exc_breakpoint, "push 0");
stub!(stub_invalid_opcode, exc_invalid_opcode, "push 0");
stub!(stub_double_fault, exc_double_fault, "nop");
stub!(stub_unexpected, exc_unexpected, "push 0");
stub!(stub_timer, exc_timer, "push 0");
stub!(stub_keyboard, exc_keyboard, "push 0");

// ---------- 时钟中断 ----------

/// 滴答计数：系统的心跳秒表。
/// 原子变量——"读出来加一写回去"可能被下一次中断拦腰打断
static TICKS: AtomicU64 = AtomicU64::new(0);

/// 时钟处理函数：每次滴答 +1，每满 100 次（1 秒）报个到，交回执
extern "C" fn exc_timer(_frame: &mut IsrFrame) {
    let ticks = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ticks % 100 == 0 {
        serial::print("[timer] ");
        serial::print_hex(ticks / 100);
        serial::print("s\n");
    }
    // 回执！忘掉这行，时钟只会响一次
    pic::send_eoi(0);
}

/// 键盘处理函数：读扫描码翻译上屏，交回执
extern "C" fn exc_keyboard(_frame: &mut IsrFrame) {
    keyboard::on_interrupt();
    pic::send_eoi(1); // IRQ1 的回执
}

// ---------- IDT 表 ----------

/// IDT 表项（64 位模式一项 16 字节）
#[derive(Clone, Copy)]
struct Gate {
    low: u64,  // 低 8 字节
    high: u64, // 高 8 字节
}

impl Gate {
    const fn missing() -> Self {
        Gate { low: 0, high: 0 }
    }

    /// 登记处理函数。ist=0 用当前栈；1~7 用 TSS 第 n 个备用栈
    fn set_handler(&mut self, stub: usize, ist: u8) {
        let addr = stub as u64;
        self.low = (addr & 0xFFFF)                     // 函数地址 0..15 位
            | ((KERNEL_CODE_SELECTOR as u64) << 16)   // 用哪个代码段
            | (((ist & 0x7) as u64) << 32)            // 备用栈编号
            | (0x8E << 40)                             // 类型字节：中断门
            | (((addr >> 16) & 0xFFFF) << 48);         // 函数地址 16..31 位
        self.high = addr >> 32;                        // 函数地址 32..63 位
    }
}

static mut IDT: [Gate; 256] = [Gate::missing(); 256];

/// IDTR：lidt 读的"表在哪、多长"（和 GDTR 一个意思）
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// 建表并装载。kmain 里、开中断前调用一次
pub fn init() {
    unsafe {
        let idt = &raw mut IDT;

        // 有正经处理逻辑的异常
        (*idt)[0].set_handler(stub_divide_error as *const () as usize, 0);
        (*idt)[3].set_handler(stub_breakpoint as *const () as usize, 0);
        (*idt)[6].set_handler(stub_invalid_opcode as *const () as usize, 0);
        // 双重故障：走 IST1 应急栈（登记里的 1 就是表项里的 IST 编号）
        (*idt)[8].set_handler(stub_double_fault as *const () as usize, 1);

        // 其余 CPU 异常（1~31 号中没专门登记的）全部兜底
        for i in 0..32 {
            if (*idt)[i].low == 0 {
                (*idt)[i].set_handler(stub_unexpected as *const () as usize, 0);
            }
        }

        // IRQ0（时钟）→ 中断号 32（PIC 重映射后的新门牌）
        (*idt)[32].set_handler(stub_timer as *const () as usize, 0);
        // IRQ1（键盘）→ 中断号 33
        (*idt)[33].set_handler(stub_keyboard as *const () as usize, 0);

        let idtr = Idtr {
            limit: (size_of::<[Gate; 256]>() - 1) as u16,
            base: idt as u64,
        };
        asm!("lidt [rdi]", in("rdi") &idtr as *const Idtr);
    }
}
