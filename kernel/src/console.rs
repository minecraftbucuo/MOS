//! 帧缓冲文本控制台：把字模"盖"到屏幕上，管理光标与滚屏

use crate::boot::Framebuffer;
use crate::font::{GLYPH_HEIGHT, GLYPHS, GLYPH_WIDTH};

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
    /// 白字黑底。颜色排布是 0x00BBGGRR，同上一章渐变图的算法
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

    /// 整屏涂成背景色
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

    /// 处理一个字节：'\n' 换行，可打印字符盖章右移，其余忽略
    pub fn put_byte(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
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
