// no_std: 不链接 Rust 标准库——没有操作系统给我们托底
#![no_std]
// no_main: 没有 main 函数——入口符号由链接脚本的 ENTRY(_start) 决定
#![no_main]
// alloc：标准库的"分配件"（Box/Vec 等）单独成库，no_std 也能用，
// 前提是有人提供 GlobalAlloc——heap.rs 干的就是这个
extern crate alloc;

use core::arch::naked_asm;
use core::panic::PanicInfo;

mod acpi;
mod boot;
mod console;
mod font;
mod gdt;
mod game;
mod heap;
mod interrupts;
mod keyboard;
mod mm;
mod pic;
mod serial;
mod sync;

/// 贴在 .limine_requests 节区里的"需求单"。
///
/// #[used]：告诉编译器"这个 static 没人用也要保留"（防止被删）；
/// 链接脚本里还有 KEEP 双保险。
#[used]
#[unsafe(link_section = ".limine_requests")]
static FRAMEBUFFER_REQUEST: boot::FramebufferRequest = boot::FramebufferRequest::new();

/// 内存清单的需求单（08 章：盘点家底）
#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP_REQUEST: boot::MemmapRequest = boot::MemmapRequest::new();

/// HHDM 偏移的需求单（物理→虚拟的翻译捷径）
#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM_REQUEST: boot::HhdmRequest = boot::HhdmRequest::new();

/// ACPI 根表的需求单（找 HPET 定时器用）
#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP_REQUEST: boot::RsdpRequest = boot::RsdpRequest::new();

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
#[unsafe(link_section = ".text.entry")] // 放进链接脚本里的入口节（排在代码段最前）
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
#[unsafe(no_mangle)] // 同上：危险属性显式 unsafe（edition 2024 强制）
pub extern "C" fn kmain() -> ! {
    // 串口全局化：从这以后任何代码（包括中断处理函数）都能 serial::print
    serial::init();
    serial::print("\n=== MOS booting ===\n");

    // 先点亮屏幕（诊断版启动顺序）：真机没有串口，屏幕是唯一输出通道，
    // 而第 05 章已证明真机屏幕本身是通的——把它挪到一切初始化之前，
    // 后面每一步都上屏报一行，死在哪一步一眼可见。
    // 此时还跑在 Limine 留下的 GDT 上，纯写显存不依赖任何我们自己建的东西
    match FRAMEBUFFER_REQUEST.response() {
        Some(resp) if !resp.framebuffers().is_empty() => {
            let fb = resp.framebuffers()[0];
            console::init(fb);
            console::print("=== MOS booting ===\n");
            console::print("screen up\n");
            // 诊断：把 framebuffer 真实尺寸报上屏。QEMU 窗口会缩放，
            // 窗口大小不等于分辨率——信内核自己报的数
            let (sw, sh) = console::pixel_size();
            console::print("screen: ");
            console::print_hex(sw as u64);
            console::print("x");
            console::print_hex(sh as u64);
            console::print("\n");
        }
        _ => serial::print("no framebuffer!\n"),
    }

    // 08 章：打印物理内存清单——分配器的原材料目录。
    // 实机诊断：清单直接打上屏幕（真机串口不可见，屏幕才是终端）——
    // 卡在哪一行、哪个区段，当场可见；QEMU 里照样能看到
    match MEMMAP_REQUEST.response() {
        Some(resp) => {
            console::print("memory map:\n");
            let mut usable_total: u64 = 0;
            for e in resp.entries() {
                console::print("  ");
                console::print_hex(e.base);
                console::print(" +");
                console::print_hex(e.length);
                console::print(" ");
                console::print(e.kind_name());
                console::print("\n");
                if e.kind == boot::MEMMAP_USABLE {
                    usable_total += e.length;
                }
            }
            console::print("usable total: ");
            console::print_hex(usable_total / 1024 / 1024);
            console::print(" MiB\n");

            // 位图分配器上线（HHDM 偏移 = 物理地址翻译成虚拟地址的加数）
            if let Some(offset) = HHDM_REQUEST.response() {
                console::print("hhdm ok\n");
                console::print("mm init...\n");
                if mm::init(resp, offset) {
                    let (total, free) = mm::stats();
                    // 诊断期：这行也走屏幕——真机上串口是黑箱，
                    // 诊断路径里不让它出现，冻结点才唯一归因
                    console::print("mm up: ");
                    console::print_hex(total);
                    console::print(" frames, ");
                    console::print_hex(free);
                    console::print(" free\n");

                    // 现场实验：发页 → 写读验证 → 收回重发
                    let p1 = mm::alloc_frame().unwrap();
                    let p2 = mm::alloc_frame().unwrap();
                    let p3 = mm::alloc_frame().unwrap();
                    console::print("mm: alloc3 ok\n");
                    serial::print("alloc 3 frames: ");
                    serial::print_hex(p1);
                    serial::print(" ");
                    serial::print_hex(p2);
                    serial::print(" ");
                    serial::print_hex(p3);
                    serial::print("\n");

                    // 往第一页写个魔数再读回来——证明这页真的能用
                    unsafe {
                        let v = mm::phys_to_virt(p1) as *mut u64;
                        v.write_volatile(0x1234_5678_9ABC_DEF0);
                        let r = (v as *const u64).read_volatile();
                        if r == 0x1234_5678_9ABC_DEF0 {
                            serial::print("page write/read ok\n");
                        } else {
                            serial::print("page write/read FAILED!\n");
                        }
                    }
                    console::print("mm: rw ok\n");

                    // 收回 p2 再发：该拿回同一页（free 生效 + 游标回退的证据）
                    mm::free_frame(p2);
                    let p4 = mm::alloc_frame().unwrap();
                    console::print("mm: realloc ok\n");
                    serial::print("free 2nd frame, realloc got: ");
                    serial::print_hex(p4);
                    serial::print("\n");

                    // 建堆：大小跟着屏幕走——贪吃蛇的双缓冲画布和屏幕一样大
                    //（QEMU 1280×800 是 4MB；真机 2560×1600 是 16MB），
                    // 堆必须比画布大一截。屏幕还没点亮就拿不到尺寸，给个保守值
                    let (_pitch, canvas_size) = console::canvas();
                    // canvas 给的是 usize，堆这边按 u64 记账——
                    // Rust 没有隐式转换，同一宽度的 usize→u64 也得手写 as
                    let heap_pages: u64 = if canvas_size > 0 {
                        (canvas_size / 4096 + 512) as u64
                    } else {
                        1080
                    };
                    if heap::init(heap_pages) {
                        serial::print("heap up: ");
                        serial::print_hex(heap_pages);
                        serial::print(" pages\n");

                        // 内核里第一次动态分配
                        let b = alloc::boxed::Box::new(0x1234_5678u64);
                        let mut v = alloc::vec::Vec::new();
                        for i in 0..5u64 {
                            v.push(i * 111);
                        }
                        serial::print("box = ");
                        serial::print_hex(*b);
                        serial::print(", vec len = ");
                        serial::print_hex(v.len() as u64);
                        serial::print(", vec[4] = ");
                        serial::print_hex(v[4]);
                        serial::print("\n");
                        // 作用域结束：Drop 自动调 dealloc（bump 堆不回收，没有实际效果）
                        console::print("heap: boxvec ok\n");
                        console::print("heap up\n");
                    } else {
                        serial::print("heap init failed!\n");
                        console::print("heap init FAILED!\n");
                    }
                } else {
                    serial::print("frame allocator init failed!\n");
                    console::print("mm init FAILED!\n");
                }
            } else {
                serial::print("no hhdm!\n");
            }
        }
        None => serial::print("no memory map!\n"),
    }

    // 换上自己的 GDT/TSS（中断系统的地基，必须在开中断之前）
    gdt::init();
    serial::print("gdt loaded\n");
    console::print("gdt up\n");

    // 立起 IDT 电话簿，然后自测：手动按响 3 号门铃（断点）。
    // 处理函数打印完会"若无其事"地回来——中断系统的第一次往返
    interrupts::init();
    serial::print("idt loaded, ringing int3...\n");
    unsafe { core::arch::asm!("int3") };
    serial::print("returned from int3, interrupts work\n");
    console::print("idt up\n");

    // 外设中断三件套：重映射 PIC → 放行时钟和键盘 → 开中断
    pic::remap();
    pic::unmask(0); // IRQ0：时钟
    pic::unmask(1); // IRQ1：键盘
    pic::init_timer(100);
    // 诊断：PIT 活体检测（真机实测 P+：芯片活着，在数数）
    console::print(if pic::pit_alive() {
        "pit counting\n"
    } else {
        "pit STUCK!\n"
    });

    // HPET 悬案的工具箱：这里只记下两个地址（纯存数，不碰任何指针），
    // 启动路径与没有 HPET 这回事时一字不差。真正的探测锁在游戏里的
    // H 键后面——出事也只坏"按下 H 之后"的世界，开机永远安全
    if let (Some(rsdp), Some(hhdm)) = (RSDP_REQUEST.response(), HHDM_REQUEST.response()) {
        acpi::stash(rsdp, hhdm);
    }

    unsafe { core::arch::asm!("sti") };
    serial::print("interrupts enabled, clock ticking\n");
    console::print("interrupts on\n");
    console::print("keyboard ready - type something!\n");

    // 番外篇：贪吃蛇开场——先放一颗食物上屏（蛇与输入在后面步骤接上）
    game::init();

    // 主循环。原来用 hlt 躺下等中断——但真机的时钟中断一次都不来
    //（实测 T=0：PIT 芯片活着、没人拦，信号却送不到），hlt 就永远
    // 只能靠键盘叫醒，蛇没节拍。现在改成盯梢模式：CPU 主动盯着 PIT
    // 计数器转圈，中断正常时轮询只旁观（QEMU 行为不变），中断死了
    // 20ms 它就自己打拍子接管（真机蛇活过来）。
    // 这就是操作系统主循环的雏形（以后会进化成调度器）
    let mut last_tick = 0u64;
    loop {
        interrupts::poll_pit_fallback();
        let t = interrupts::ticks();
        if t != last_tick {
            last_tick = t;
            game::on_tick(t);
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
