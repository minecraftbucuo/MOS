//! 8259 PIC（中断控制器）与 PIT（定时器）驱动。
//! 对应教程：docs/07-中断与时钟.md

use crate::serial::{inb, outb};

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

/// 启动 PIT：每秒敲 hz 次铃
pub fn init_timer(hz: u32) {
    const BASE_HZ: u32 = 1193182; // PIT 的出厂节拍
    let divisor = BASE_HZ / hz;
    outb(0x43, 0x36); // 通道 0、除数两字节（先低后高）、方波模式
    outb(0x40, (divisor & 0xFF) as u8);
    outb(0x40, (divisor >> 8) as u8);
}
