//! 串口输出：内核的第一张"嘴"。
//!
//! COM1 的 UART（型号 16550）寄存器挂在 IO 端口空间，基址 0x3F8。
//! 对应教程：docs/03-串口输出.md

use core::arch::asm;

/// 往 IO 端口写一个字节（"递字条"）
///
/// 函数签名是安全的——调用者不需要写 unsafe。
/// 真正的危险被圈在函数体内部这一小块里：编译器看不见 0x3F8 窗口
/// 后面有什么，由我们在这里担保"调用方传来的端口确实是 UART 的"。
pub fn outb(port: u16, value: u8) {
    unsafe {
        // out 指令的固定用法：端口地址放 dx，数据放 al
        asm!("out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// 从 IO 端口读一个字节（"从窗口取条子"，下一章收键盘数据会用到）
pub fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// 一个串口。base 是它的端口基址（COM1 = 0x3F8）
pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    /// COM1 的标准端口号，IBM PC 时代传下来的老门牌
    pub const COM1: u16 = 0x3F8;

    pub fn new(base: u16) -> Self {
        Self { base }
    }

    /// 开机时把旋钮拧到位：关中断 → 约速度 → 定格式 → 开 FIFO
    ///
    /// 注意这里没有 unsafe 块：outb/inb 已在最底层封装了危险，
    /// 上层代码保持干净。unsafe 只应出现在真正 unsafe 的地方。
    pub fn init(&mut self) {
        outb(self.base + 1, 0x00); // 关串口中断（中断是后面章节的事，先静音）
        outb(self.base + 3, 0x80); // DLAB 拨到 1：0/1 号按钮翻面成"波特率除数"
        outb(self.base + 0, 0x03); // 除数低字节 = 3 → 38400 波特
        outb(self.base + 1, 0x00); // 除数高字节
        outb(self.base + 3, 0x03); // DLAB 拨回 0；8 数据位、无校验、1 停止位
        outb(self.base + 2, 0xC7); // 开 FIFO 缓冲并清空
        outb(self.base + 4, 0x0B); // 数据终端就绪 + 请求发送
    }

    /// 递一张字条。递之前先看 LSR 的空闲灯（bit 5）
    fn send_raw(&mut self, data: u8) {
        // 空闲灯没亮就一直看——"往漏水的杯子里续水，先看水位"
        while inb(self.base + 5) & 0x20 == 0 {}
        outb(self.base, data);
    }

    /// 发送字符串。'\n' 自动补 '\r'（电传打字机的老规矩：先回行首再卷纸）
    pub fn send_str(&mut self, s: &str) {
        for &b in s.as_bytes() {
            match b {
                b'\n' => {
                    self.send_raw(b'\r');
                    self.send_raw(b'\n');
                }
                _ => self.send_raw(b),
            }
        }
    }
}
