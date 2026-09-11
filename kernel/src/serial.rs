//! 串口输出：内核的第一张"嘴"。
//!
//! COM1 的 UART（型号 16550）寄存器挂在 IO 端口空间，基址 0x3F8。
//! 对应教程：docs/03-串口输出.md

use core::arch::asm;
use core::cell::UnsafeCell;

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

    pub const fn new(base: u16) -> Self {
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
        self.send_bytes(s.as_bytes());
    }

    /// 发送原始字节（'\n' 同样补 '\r'）
    pub fn send_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
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

// ---------- 全局单例：中断处理函数里也要打印日志 ----------

/// 共享包装：让 static 里也能放 SerialPort（模式同 boot.rs 的请求单）。
///
/// unsafe impl Sync 的担保理由：
/// 单核机器 + 中断门进入处理函数时 CPU 自动关中断（见中断一章），
/// 任何时刻最多一个执行流在操作串口
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

/// 全局 COM1：内核任何代码（包括中断处理函数）都能借它说话
static COM1: Shared<SerialPort> = Shared::new(SerialPort::new(SerialPort::COM1));

/// 开机初始化（kmain 里调用一次，替代原来的局部变量写法）
pub fn init() {
    COM1.get().init();
}

/// 打印字符串——内核版 print!
pub fn print(s: &str) {
    COM1.get().send_str(s);
}

/// 按 16 进制打印 u64（no_std 没有 format!，自己排）
pub fn print_hex(value: u64) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut buf = [b'0'; 16];
    let mut v = value;
    for i in (0..16).rev() {
        buf[i] = HEX[(v & 0xF) as usize];
        v >>= 4;
    }
    // 去掉前导零（全零时保留一个 '0'）
    let start = buf.iter().position(|&b| b != b'0').unwrap_or(15);
    COM1.get().send_bytes(&buf[start..]);
}
