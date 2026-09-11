// no_std: 不链接 Rust 标准库——没有操作系统给我们托底
#![no_std]
// no_main: 没有 main 函数——入口符号由链接脚本的 ENTRY(_start) 决定
#![no_main]

use core::arch::naked_asm;
use core::panic::PanicInfo;

mod boot;
mod serial;

/// 贴在 .limine_requests 节区里的"需求单"。
///
/// #[used]：告诉编译器"这个 static 没人用也要保留"（防止被删）；
/// 链接脚本里还有 KEEP 双保险。
#[used]
#[link_section = ".limine_requests"]
static FRAMEBUFFER_REQUEST: boot::FramebufferRequest = boot::FramebufferRequest::new();

/// 内核启动栈。
/// #[repr(align(16))]: x86-64 ABI 要求栈按 16 字节对齐，否则
/// 某些指令（如 SSE 的 movaps）会触发对齐错误——这是裸机开发
/// 最经典的隐形坑之一。
#[repr(align(16))]
// 字段只在汇编里通过符号地址使用，Rust 看不到"读取"，消除误报
#[allow(dead_code)]
struct BootStack([u8; 64 * 1024]); // 64 KiB，够内核启动阶段用

// 放在 .bss 段：不占内核文件体积，Limine 加载时清零
static BOOT_STACK: BootStack = BootStack([0; 64 * 1024]);

/// 内核的第一条指令。
///
/// #[naked]: "裸函数"——告诉编译器这个函数不要生成任何前置代码
/// （不保存寄存器、不建栈帧），函数体里的汇编就是函数的全部内容。
/// 没有它，编译器生成的函数序言会在 rsp 还无效时就开始用栈 → 当场崩溃。
///
/// extern "C": 用 C 调用约定（System V ABI），保证 Limine / 链接脚本
/// 找得到这个符号、且函数行为符合约定。
#[unsafe(no_mangle)] // 保留符号名 _start 不被 Rust 改名混淆（危险属性需显式 unsafe）
#[link_section = ".text.entry"] // 放进链接脚本里的入口节（排在代码段最前）
#[unsafe(naked)]
pub extern "C" fn _start() -> ! {
    naked_asm!(
        // 先把栈的起始地址装进 rax。movabs 支持 64 位立即数，
        // 装得下高地址（0xFFFFFFFF80000000 一带）——lea 的 32 位寻址装不下
        "movabs rax, {stack}",
        // 栈从高地址向低地址生长，所以 rsp 指向栈数组的"末尾"
        "lea rsp, [rax + 64*1024]",
        // 清零帧指针，栈回溯到此为止（表示"没有上层调用者"）
        "xor ebp, ebp",
        // 进入 Rust 世界。call 会把返回地址压栈，Rust 函数从此有栈可用
        "call {kmain}",
        // 万一 kmain 返回了（不该发生），停机死循环兜底
        "2:",
        "hlt",   // 让 CPU 休眠直到下一个中断，比忙循环省电
        "jmp 2b",
        stack = sym BOOT_STACK, // sym: 把符号地址嵌进汇编
        kmain = sym kmain,
    );
}

/// Rust 世界的入口。先让串口说话，再向 Limine 领屏幕。
#[no_mangle]
pub extern "C" fn kmain() -> ! {
    let mut com1 = serial::SerialPort::new(serial::SerialPort::COM1);
    com1.init();
    com1.send_str("\n=== MOS booting ===\n");

    match FRAMEBUFFER_REQUEST.response() {
        Some(resp) if !resp.framebuffers().is_empty() => {
            let fb = resp.framebuffers()[0];
            com1.send_str("framebuffer acquired\n");
            draw_test_pattern(fb);
            com1.send_str("test pattern drawn\n");
        }
        _ => {
            com1.send_str("no framebuffer!\n");
        }
    }

    loop {
        core::hint::spin_loop();
    }
}

/// 往屏幕上画测试图：红绿双色渐变，一眼就能看出"画对了没有"。
fn draw_test_pattern(fb: &boot::Framebuffer) {
    let width = fb.width as usize;
    let height = fb.height as usize;
    let pitch = fb.pitch as usize;
    let base = fb.address();

    for y in 0..height {
        for x in 0..width {
            // x 越右越红，y 越下越绿，蓝固定 —— 出来是张漂亮的渐变
            let r = ((x * 255) / (width - 1)) as u32;
            let g = ((y * 255) / (height - 1)) as u32;
            let pixel = (r << 16) | (g << 8) | 0x40;

            unsafe {
                // 像素 (x, y) 的地址 = 基址 + 行距×y + 4字节×x
                // write_volatile：这块显存没人跟编译器打招呼，别让它优化掉
                let p = base.add(y * pitch + x * 4) as *mut u32;
                p.write_volatile(pixel);
            }
        }
    }
}

/// no_std 环境必须自己提供 panic 处理函数（标准库里那个没了）。
/// 以后这里会升级成：打印 panic 位置到串口和屏幕，然后停机。
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
