//! 帧缓冲文本控制台：把字模"盖"到屏幕上，管理光标与滚屏

use crate::boot::Framebuffer;
use crate::font::{GLYPH_HEIGHT, GLYPHS, GLYPH_WIDTH};
use core::cell::UnsafeCell;

/// 文本控制台：屏幕参数 + 光标位置
pub struct Console {
    base: *mut u8, // 显存起始地址
    width: usize,  // 屏幕宽（像素）
    height: usize, // 屏幕高（像素）
    pitch: usize,  // 行距：一行像素实际占的字节数
    cols: usize,   // 一行能放几个字符
    rows: usize,   // 一共几行字符
    col: usize,    // 光标：当前列
    row: usize,   // 光标：当前行
    fg: u32,       // 前景色（笔画颜色）
    bg: u32,       // 背景色
}

impl Console {
    /// 白字黑底。颜色排布 0x00RRGGBB（高字节红、低字节蓝），同第 05 章渐变图
    pub fn new(fb: &Framebuffer) -> Self {
        Self {
            base: fb.address(),
            width: fb.width as usize,
            height: fb.height as usize,
            pitch: fb.pitch as usize,
            cols: fb.width as usize / GLYPH_WIDTH,
            rows: fb.height as usize / GLYPH_HEIGHT,
            col: 0,
            row: 0,
            fg: 0x00FFFFFF,
            bg: 0x00000000,
        }
    }

    /// 整屏涂成背景色（开机时用一次；游戏画面走 blit 双缓冲通道）
    pub fn clear(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                unsafe {
                    let p = self.base.add(y * self.pitch + x * 4) as *mut u32;
                    p.write_volatile(self.bg);
                }
            }
        }
    }

    /// 把字符 ch 盖进 (col, row) 格子：亮点位写前景色，暗点位写背景色
    fn draw_char(&self, col: usize, row: usize, ch: u8) {
        let glyph = &GLYPHS[ch as usize];
        let x0 = col * GLYPH_WIDTH; // 格子左上角的像素坐标
        let y0 = row * GLYPH_HEIGHT;

        for (dy, &bits) in glyph.iter().enumerate() {
            for dx in 0..GLYPH_WIDTH {
                // 0x80 >> dx：从最高位（最左像素）开始逐位检查
                let color = if bits & (0x80 >> dx) != 0 { self.fg } else { self.bg };
                unsafe {
                    let p = self.base.add((y0 + dy) * self.pitch + (x0 + dx) * 4) as *mut u32;
                    p.write_volatile(color);
                }
            }
        }
    }

    /// 双缓冲翻页：把后备缓冲整块拷进显存。
    /// 画面先画在屏外缓冲里，画完一次拷贝上屏——显示刷新要么看到
    /// 旧帧、要么看到新帧，永远逮不到"画到一半"的中间态
    fn blit(&self, buf: *const u8) {
        unsafe {
            core::ptr::copy_nonoverlapping(buf, self.base, self.pitch * self.height);
        }
    }

    /// 屏幕像素尺寸：(宽, 高)
    fn pixel_size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// 滚屏：整屏内容上移一行，最底一行腾出来
    fn scroll(&mut self) {
        let char_row = GLYPH_HEIGHT * self.pitch; // 一个字符行占的字节数
        unsafe {
            // ptr::copy 是 memmove 语义：源和目的重叠也能正确搬运
            core::ptr::copy(
                self.base.add(char_row),   // 源：第 1 行字符开头
                self.base,                  // 目的：第 0 行字符开头
                (self.rows - 1) * char_row, // 搬 (rows-1) 行的量
            );
        }
        self.clear_char_row(self.rows - 1); // 最底行涂黑
    }

    /// 把第 row 行字符格涂成背景色
    fn clear_char_row(&mut self, row: usize) {
        for y in row * GLYPH_HEIGHT..(row + 1) * GLYPH_HEIGHT {
            for x in 0..self.width {
                unsafe {
                    let p = self.base.add(y * self.pitch + x * 4) as *mut u32;
                    p.write_volatile(self.bg);
                }
            }
        }
    }

    /// 处理一个字节：'\n' 换行，退格，可打印字符盖章右移，其余忽略
    pub fn put_byte(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            b'\x08' => self.backspace(),
            0x20..=0x7E => {
                self.draw_char(self.col, self.row, b);
                self.col += 1;
                if self.col >= self.cols {
                    self.newline(); // 行满自动换行
                }
            }
            _ => {} // 其他控制字符（\t、\r 等）暂不支持
        }
    }

    /// 输出字符串
    pub fn write_str(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.put_byte(b);
        }
    }

    /// 退格：光标退一格，把那格涂回背景色
    fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.draw_char(self.col, self.row, b' ');
        }
    }

    /// 换行：光标下移一行；已在最底行则滚屏
    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll();
        }
    }
}

// ---------- 全局化（模式同 serial.rs 的 Shared） ----------
// 键盘处理函数要把字符印上屏，Console 不能再是 kmain 的局部变量

/// 共享包装：UnsafeCell + 手写 Sync（单核、且从不并发写，安全）
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

/// 全局控制台。屏幕拿到之前是 None——那时 print 静默丢弃
///（真机没给 framebuffer 的场合，内核也不至于崩）
static CONSOLE: Shared<Option<Console>> = Shared::new(None);

/// 拿到屏幕后调用（kmain）：建控制台并清屏
pub fn init(fb: &Framebuffer) {
    let mut con = Console::new(fb);
    con.clear();
    *CONSOLE.get() = Some(con);
}

/// 打印到屏幕（全局入口，谁都能调）
pub fn print(s: &str) {
    if let Some(con) = CONSOLE.get().as_mut() {
        con.write_str(s);
    }
}

/// 按 16 进制打印 u64 到屏幕（serial::print_hex 的屏幕对应物）。
/// no_std 没有 format!，手工排：16 个字符位从低位往高位填
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
    // buf 里只可能被填进 HEX 表的字符，UTF-8 校验必然通过
    print(unsafe { core::str::from_utf8_unchecked(&buf[start..]) });
}

/// 全局：屏幕像素尺寸（宽, 高）
pub fn pixel_size() -> (usize, usize) {
    match CONSOLE.get() {
        Some(con) => con.pixel_size(),
        None => (0, 0),
    }
}

/// 全局：后备缓冲画布的参数（行距字节数, 总字节数）
pub fn canvas() -> (usize, usize) {
    match CONSOLE.get() {
        Some(con) => (con.pitch, con.pitch * con.height),
        None => (0, 0),
    }
}

/// 全局：双缓冲翻页——后备缓冲整块拷上屏
pub fn blit(buf: *const u8) {
    if let Some(con) = CONSOLE.get() {
        con.blit(buf);
    }
}
