//! PS/2 键盘驱动：读扫描码，翻译后投递进按键队列。
//! 对应教程：docs/07-中断与时钟.md
//!
//! 输入架构（番外篇起）：中断里只把键塞进队列，谁消费谁处理——
//! 贪吃蛇消费方向键，将来的 shell 消费行编辑。ISR 要短，重活在主循环干。

use crate::serial::inb;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

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

// ---------- 按键队列：环形缓冲，中断生产、主循环消费 ----------
// 单生产者单消费者，读写位置各自只被一方改动，原子变量足够，不需要锁

/// 特殊键码：0x80 以上留给"没有字符对应"的键（方向键这类）
pub const KEY_LEFT: u8 = 0x81;
pub const KEY_RIGHT: u8 = 0x82;
pub const KEY_UP: u8 = 0x83;
pub const KEY_DOWN: u8 = 0x84;

const Q_LEN: usize = 32; // 2 的幂，取模就是位与（这里用 %，意思更直白）
static QUEUE: [AtomicU8; Q_LEN] = [const { AtomicU8::new(0) }; Q_LEN];
static Q_HEAD: AtomicUsize = AtomicUsize::new(0); // 写位置（中断动）
static Q_TAIL: AtomicUsize = AtomicUsize::new(0); // 读位置（主循环动）

/// 中断侧：投一个键进队列。队列满了直接丢——ISR 里不能等
fn push_key(k: u8) {
    let head = Q_HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % Q_LEN;
    if next == Q_TAIL.load(Ordering::Relaxed) {
        return; // 满了。32 格攒不满的，满了说明消费端死了，丢了也不亏
    }
    QUEUE[head].store(k, Ordering::Relaxed);
    Q_HEAD.store(next, Ordering::Relaxed);
}

/// 消费侧：取一个键，队列空返回 None
pub fn pop_key() -> Option<u8> {
    let tail = Q_TAIL.load(Ordering::Relaxed);
    if tail == Q_HEAD.load(Ordering::Relaxed) {
        return None; // head == tail = 队列空
    }
    let k = QUEUE[tail].load(Ordering::Relaxed);
    Q_TAIL.store((tail + 1) % Q_LEN, Ordering::Relaxed);
    Some(k)
}

/// 键盘中断到来：从 0x60 端口取一个扫描码，翻译后投递进队列
pub fn on_interrupt() {
    let sc = inb(0x60); // 键盘控制器的数据口

    // 扩展码后半段：方向键/小键盘区，认得出方向就投递
    if EXTENDED.swap(false, Ordering::Relaxed) {
        let released = sc & 0x80 != 0;
        let code = sc & 0x7F;
        if !released {
            // 扫描码集 1 扩展段：四个方向键的编码
            //（注意别抄成集 2 的 0x72/0x74/0x75——我们全程用集 1）
            match code {
                0x4B => push_key(KEY_LEFT),
                0x4D => push_key(KEY_RIGHT),
                0x48 => push_key(KEY_UP),
                0x50 => push_key(KEY_DOWN),
                _ => {} // 其余扩展键（Home/End 等）暂不认
            }
        }
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
                push_key(ch); // 字符键也走队列，消费方决定拿它干嘛
            }
        }
    }
}
