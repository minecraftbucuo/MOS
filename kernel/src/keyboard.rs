//! PS/2 键盘驱动：读扫描码，翻译成字符上屏。
//! 对应教程：docs/07-中断与时钟.md

use crate::console;
use crate::serial::inb;
use core::sync::atomic::{AtomicBool, Ordering};

/// 扫描码集 1 的 ASCII 对照表：下标 = 扫描码，值 = 字符（0 = 无对应字符）。
/// 按行对应键盘物理排布，对照着看很直观
static NORMAL: [u8; 59] = [
    0, 0x1B, b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9', b'0', b'-', b'=',
    b'\x08', b'\t', b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', b'o', b'p', b'[', b']',
    b'\n', 0, b'a', b's', b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', b'\'',
    b'`', 0, b'\\', b'z', b'x', b'c', b'v', b'b', b'n', b'm', b',', b'.', b'/',
    0, b'*', 0, b' ', 0,
];
/// Shift 按下时的对照表（同行位置，大小写和符号切换）
static SHIFTED: [u8; 59] = [
    0, 0x1B, b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'(', b')', b'_', b'+',
    b'\x08', b'\t', b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', b'O', b'P', b'{', b'}',
    b'\n', 0, b'A', b'S', b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', b'"',
    b'~', 0, b'|', b'Z', b'X', b'C', b'V', b'B', b'N', b'M', b'<', b'>', b'?',
    0, b'*', 0, b' ', 0,
];

/// Shift 按着吗（两边的 Shift 键共用一个状态就够了）
static SHIFT: AtomicBool = AtomicBool::new(false);
/// 上一码是扩展前缀 0xE0 吗（方向键等会先发一个 0xE0）
static EXTENDED: AtomicBool = AtomicBool::new(false);

/// 键盘中断到来：从 0x60 端口取一个扫描码，翻译上屏
pub fn on_interrupt() {
    let sc = inb(0x60); // 键盘控制器的数据口

    // 扩展码：0xE0 后面跟的码（方向键/小键盘区）先忽略
    if EXTENDED.swap(false, Ordering::Relaxed) {
        return;
    }
    if sc == 0xE0 {
        EXTENDED.store(true, Ordering::Relaxed);
        return;
    }

    let released = sc & 0x80 != 0; // 最高位 = 松开
    let code = sc & 0x7F;
    match code {
        0x2A | 0x36 => SHIFT.store(!released, Ordering::Relaxed), // 左/右 Shift
        _ if released => {} // 其他键的松开码直接忽略
        _ => {
            let table = if SHIFT.load(Ordering::Relaxed) {
                &SHIFTED
            } else {
                &NORMAL
            };
            if let Some(&ch) = table.get(code as usize).filter(|&&c| c != 0) {
                // 单字节 ASCII 一定是合法 UTF-8，from_utf8 不会失败
                let buf = [ch];
                if let Ok(s) = core::str::from_utf8(&buf) {
                    console::print(s); // '\n'、退格、可打印字符都由 put_byte 处理
                }
            }
        }
    }
}
