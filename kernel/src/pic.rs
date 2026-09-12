//! 8259 PIC（中断控制器）与 PIT（定时器）驱动。
//! 对应教程：docs/07-中断与时钟.md

use crate::serial::{inb, outb};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// 主/从 PIC 的端口（两片级联：主的 IRQ2 接着从片）
const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

// 重映射的"新门牌"：IRQ0~7 → 32~39，IRQ8~15 → 40~47
const MASTER_OFFSET: u8 = 32;
const SLAVE_OFFSET: u8 = 40;

/// 重映射 PIC：把 IRQ 0..15 改接到中断号 32..47
pub fn remap() {
    // ICW1（写命令口）：开始初始化，告知"级联 + 后面还有 ICW4"
    outb(MASTER_CMD, 0x11);
    outb(SLAVE_CMD, 0x11);

    // ICW2（写数据口）：各自的新门牌起始号
    outb(MASTER_DATA, MASTER_OFFSET);
    outb(SLAVE_DATA, SLAVE_OFFSET);

    // ICW3：主从怎么级联——从 PIC 挂在主 PIC 的 2 号线上
    outb(MASTER_DATA, 0x04);
    outb(SLAVE_DATA, 0x02);

    // ICW4：8086 模式
    outb(MASTER_DATA, 0x01);
    outb(SLAVE_DATA, 0x01);

    // 掩码（IMR）：先全部挡住。IDT 还没登记的 IRQ 放进来
    // = 没登记的门铃 = 三重故障，所以谁要用谁显式 unmask
    outb(MASTER_DATA, 0xFF);
    outb(SLAVE_DATA, 0xFF);
}

/// 放行某个 IRQ（清掉掩码里的对应位）。
/// 掩码是"要挡谁"，先读回当前值再清位，不能凭空写死
pub fn unmask(irq: u8) {
    let port = if irq < 8 { MASTER_DATA } else { SLAVE_DATA };
    let mask = inb(port);
    outb(port, mask & !(1 << (irq % 8)));
}

/// 中断处理完的回执（EOI）。不回执，总机会一直压着后续的中断
pub fn send_eoi(irq: u8) {
    // 从 PIC 的中断要两边都交（从片 → 主片），主片的交自己就够
    if irq >= 8 {
        outb(SLAVE_CMD, 0x20);
    }
    outb(MASTER_CMD, 0x20);
}

/// 当前除数（轮询备用心跳换算时间用；0 = 还没 init_timer）
static DIVISOR: AtomicU32 = AtomicU32::new(0);

/// 启动 PIT：每秒敲 hz 次铃
pub fn init_timer(hz: u32) {
    const BASE_HZ: u32 = 1193182; // PIT 的出厂节拍
    let divisor = BASE_HZ / hz;
    // 0x34 = 通道 0、除数两字节（先低后高）、mode 2（速率发生器）。
    // mode 2：计数值每个时钟 -1，数到 0 就 OUT 拉低一个时钟再重装除数
    // ——"每个时钟正好走 1"这一点是轮询版换算时间的根基。原来的
    // mode 3（方波）每个时钟 -2 且重装值在 N/N-1 间交替，读数换算
    // 时间就不干净了
    outb(0x43, 0x34);
    outb(0x40, (divisor & 0xFF) as u8);
    outb(0x40, (divisor >> 8) as u8);
    DIVISOR.store(divisor, Ordering::Relaxed);
}

/// 每个滴答对应多少个 PIT 输入时钟（1193182 / 频率）
pub fn pit_divisor() -> u32 {
    DIVISOR.load(Ordering::Relaxed)
}

/// pit_alive 的结果存档：游戏画面里的诊断行随时要查，
/// 不能为了看一眼再忙等几毫秒
static PIT_OK: AtomicBool = AtomicBool::new(false);

/// 开机时 PIT 活体检测的结论（true = 8254 在数数）
pub fn pit_ok() -> bool {
    PIT_OK.load(Ordering::Relaxed)
}

/// 读 PIT 通道 0 的当前计数值（轮询节拍用，见 interrupts 的备用心跳）。
/// 先写"锁存命令"让芯片把正在变的计数值定格一份，再分低/高两字节读——
/// 边跑边读会读花
pub fn pit_count() -> u16 {
    outb(0x43, 0x00); // 锁存通道 0
    let lo = inb(0x40);
    let hi = inb(0x40);
    ((hi as u16) << 8) | lo as u16
}

/// 诊断：PIT 通道 0 还活着吗（真机调试用）。
/// 现代固件可能开 HPET 的 legacy replacement 模式把 8254 停掉——
/// PIT 不数数，IRQ0 永远不来，QEMU 却模拟不出这种状态。
/// 测法：隔一小段读两次计数值，变了 = 活着
pub fn pit_alive() -> bool {
    let a = pit_count();
    // 忙等几毫秒：一个周期 10ms 里计数值要变近万次，等一小拍就够
    for _ in 0..20_000_000 {
        core::hint::spin_loop();
    }
    let b = pit_count();
    let alive = a != b;
    PIT_OK.store(alive, Ordering::Relaxed);
    alive
}

/// 诊断：主 PIC 的掩码（IMR）里 IRQ0 现在是否被挡着。
/// 我们开机明明 unmask 过——如果这里变回 1，说明有东西（典型：SMM）
/// 在背后偷偷改了掩码
pub fn irq0_masked() -> bool {
    inb(MASTER_DATA) & 1 != 0
}

/// 诊断：读主 PIC 的 IRR（中断请求寄存器）——
/// "哪些 IRQ 已经在总机上亮灯、正排队等 CPU 认领"。
/// 先写 OCW3（0x0A）声明"下次读命令口时给我 IRR"，再读
pub fn master_irr() -> u8 {
    outb(MASTER_CMD, 0x0A);
    inb(MASTER_CMD)
}
