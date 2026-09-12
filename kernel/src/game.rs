//! 内核贪吃蛇（番外篇）。状态与逻辑全在本模块，不碰其他子系统的行为。
//!
//! 结构是所有游戏循环的通形，也是 09 章调度器的雏形：
//!   时钟中断 → 只数节拍
//!   键盘中断 → 只收按键
//!   主循环   → 每个 hlt 醒来调一次 on_tick，到步点才走棋
//! update/draw 都在主循环里干，中断里绝不跑游戏逻辑——中断要短。
//!
//! 渲染用双缓冲：画面先画进屏外的一块内存（后备缓冲，堆上分配），
//! 画完一口气拷上屏。直接往显存里画，显示刷新会逮到"画到一半"
//! 的中间态——蛇走过的地方一闪而过、自己消失的细线渣就是它。

use crate::console;
use crate::font::{GLYPHS, GLYPH_HEIGHT, GLYPH_WIDTH};
use crate::interrupts;
use crate::keyboard;
use crate::sync::Shared;
use alloc::alloc::alloc;
use alloc::vec::Vec;
use core::alloc::Layout;

/// 棋盘格子的边长（像素）。正方形，蛇和食物都占一格
const CELL: usize = 16;

/// 颜色。显存像素排布 0x00RRGGBB——红在最高字节
///（第 05 章渐变图 (r << 16) 验证过；console.rs 的旧注释写反过，已纠正）
const FOOD_COLOR: u32 = 0x00CC0000; // 红：食物，也是 GAME OVER 的标题色
const SNAKE_COLOR: u32 = 0x0000CC00; // 绿
const BG_COLOR: u32 = 0x00000000; // 黑
const TEXT_COLOR: u32 = 0x00FFFFFF; // 白
const DIM_COLOR: u32 = 0x00888888; // 灰：提示文字

/// 步频分频：时钟 100Hz，每 10 滴答走一步 = 每秒 10 步
const STEP_INTERVAL: u64 = 10;

/// 游戏进行到哪个阶段了
enum Phase {
    Play,
    Over,
}

/// 游戏状态。棋盘 = 屏幕按 CELL 切成的正方形网格
struct Game {
    cols: usize,
    rows: usize,
    /// 蛇身：格子坐标列表，**末尾是蛇头**（push 进头、remove 出尾）
    body: Vec<(usize, usize)>,
    dir: (i32, i32),      // 前进方向 (列增量, 行增量)
    food: (usize, usize), // 食物所在格子
    rng: u64,             // 伪随机数状态（种子 = 开机滴答数）
    phase: Phase,
    score: usize,
    buf: *mut u8,   // 后备缓冲：屏外画布（堆上的一整块）
    pitch: usize,   // 画布行距（字节），与显存一致
    size: usize,    // 画布总字节数（整屏涂黑用）
}

impl Game {
    /// 线性同余生成器（LCG）：一次乘法一次加法，输出"看着像随机"的数。
    /// 裸机没有现成随机源，拿开机以来的滴答数当种子——每次开机都不同
    fn next_rand(&mut self) -> u64 {
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.rng
    }

    /// 把一个格子画进后备缓冲
    fn paint(&self, col: usize, row: usize, color: u32) {
        for yy in 0..CELL {
            // 一行的起点 = 基址 + 像素行 × 行距 + 格子左边缘
            let row_base = (row * CELL + yy) * self.pitch + col * CELL * 4;
            for xx in 0..CELL {
                unsafe {
                    let p = self.buf.add(row_base + xx * 4) as *mut u32;
                    p.write_volatile(color);
                }
            }
        }
    }

    /// 把一个像素画进后备缓冲
    fn px(&self, x: usize, y: usize, color: u32) {
        unsafe {
            let p = self.buf.add(y * self.pitch + x * 4) as *mut u32;
            p.write_volatile(color);
        }
    }

    /// 把一行 ASCII 文字画进后备缓冲（逐字取字模，亮点描 fg、暗点描 bg）。
    /// col_px/row_px 是像素坐标。font.rs 的字模表本来就是公开的，直接取用
    fn text(&self, col_px: usize, row_px: usize, s: &[u8], fg: u32, bg: u32) {
        for (i, &b) in s.iter().enumerate() {
            let glyph = &GLYPHS[b as usize];
            let x0 = col_px + i * GLYPH_WIDTH;
            for (dy, &bits) in glyph.iter().enumerate() {
                for dx in 0..GLYPH_WIDTH {
                    let c = if bits & (0x80 >> dx) != 0 { fg } else { bg };
                    self.px(x0 + dx, row_px + dy, c);
                }
            }
        }
    }

    /// 左上角的分数 HUD。每步都重画——蛇从它底下钻过时，
    /// 文字永远压在蛇上面，画面不会花
    fn paint_score(&self) {
        let mut line = [0u8; 6 + 20];
        line[..6].copy_from_slice(b"SCORE ");
        let digits = fmt_num(self.score, &mut line[6..]);
        self.text(8, 8, &line[..6 + digits], TEXT_COLOR, BG_COLOR);
    }

    /// 把食物放到随机格子（避开蛇身占着的格子），画进缓冲
    fn place_food(&mut self) {
        loop {
            let r = self.next_rand();
            let p = ((r as usize) % self.cols, ((r >> 32) as usize) % self.rows);
            if !self.body.contains(&p) {
                self.food = p;
                self.paint(p.0, p.1, FOOD_COLOR);
                return;
            }
        }
    }

    /// 蛇走一步：判死、擦尾、长头、吃食物，最后翻页上屏
    fn tick(&mut self) {
        let (hx, hy) = *self.body.last().unwrap(); // 蛇头现在在哪
        // 撞墙判死：头走出棋盘就是终点（不再穿墙）
        let nx = hx as i32 + self.dir.0;
        let ny = hy as i32 + self.dir.1;
        if nx < 0 || ny < 0 || nx >= self.cols as i32 || ny >= self.rows as i32 {
            self.game_over();
            return;
        }
        let (nx, ny) = (nx as usize, ny as usize);

        let ate = (nx, ny) == self.food;
        if !ate {
            // 尾巴先出列，那格画回背景色——
            // 漏了这步就是"蛇走一路掉一节"，蛇身会越拖越长
            let tail = self.body.remove(0);
            self.paint(tail.0, tail.1, BG_COLOR);
        }
        // 咬到自己判死（判断的是出列尾巴之后的蛇身）
        if self.body.contains(&(nx, ny)) {
            self.game_over();
            return;
        }

        self.body.push((nx, ny));
        self.paint(nx, ny, SNAKE_COLOR);
        if ate {
            self.score += 1;
            self.place_food();
        }
        self.paint_score();
        console::blit(self.buf);
    }

    /// 终局画面：清屏、居中三行字、翻页。此后 tick 不再走，等空格重开
    fn game_over(&mut self) {
        self.phase = Phase::Over;
        unsafe { core::ptr::write_bytes(self.buf, 0, self.size) };

        let cx = self.cols * CELL / 2; // 屏幕中心的像素坐标
        let cy = self.rows * CELL / 2;
        let (w, h) = (GLYPH_WIDTH, GLYPH_HEIGHT);

        self.text(cx - 9 * w / 2, cy - 3 * h, b"GAME OVER", FOOD_COLOR, BG_COLOR);

        let mut line = [0u8; 6 + 20];
        line[..6].copy_from_slice(b"SCORE ");
        let digits = fmt_num(self.score, &mut line[6..]);
        let n = 6 + digits;
        self.text(cx - n * w / 2, cy, &line[..n], TEXT_COLOR, BG_COLOR);

        self.text(
            cx - 16 * w / 2,
            cy + 2 * h,
            b"SPACE TO RESTART",
            DIM_COLOR,
            BG_COLOR,
        );
        console::blit(self.buf);
    }

    /// 原地重开：重置状态、重画第一帧。注意不能重新 init——
    /// bump 堆不回收，每死一次就分配一块新画布的话，两局堆就见底了
    fn reset(&mut self) {
        self.body.clear();
        let y = self.rows / 2;
        for i in 0..4 {
            self.body.push((self.cols / 4 + i, y));
        }
        self.dir = (1, 0);
        self.score = 0;
        self.phase = Phase::Play;
        unsafe { core::ptr::write_bytes(self.buf, 0, self.size) };
        for &(x, yy) in &self.body {
            self.paint(x, yy, SNAKE_COLOR);
        }
        self.place_food();
        self.paint_score();
        console::blit(self.buf);
    }
}

/// 数字转十进制 ASCII（没有 printf，自己动手）。
/// 除法取余得到的是从个位往高位倒着的数，先填进局部数组再
/// 正着拷给调用方——填写方向和读取方向必须一致，不然就是空枪
fn fmt_num(mut n: usize, out: &mut [u8]) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if n == 0 {
        i -= 1;
        tmp[i] = b'0';
    }
    while n > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let len = tmp.len() - i;
    out[..len].copy_from_slice(&tmp[i..]);
    len
}

static GAME: Shared<Option<Game>> = Shared::new(None);

/// 改方向。禁止一切 180 度调头：新方向走一步不许落在脖子的格子上。
/// 只查"是否与当前方向相反"不够——一步之内连按两个键（如 w、a）
/// 能绕过检查拼出反向，蛇头就撞进自己脖子，蛇身里出现同格双份
fn set_dir(dc: i32, dr: i32) {
    let g = GAME.get();
    if let Some(g) = g.as_mut() {
        if (dc, dr) == (-g.dir.0, -g.dir.1) {
            return;
        }
        let n = g.body.len();
        if n >= 2 {
            let (hx, hy) = g.body[n - 1]; // 蛇头
            let (nx, ny) = g.body[n - 2]; // 脖子
            if (hx as i32 + dc, hy as i32 + dr) == (nx as i32, ny as i32) {
                return; // 新方向的第一步踩在脖子上 = 调头，拒绝
            }
        }
        g.dir = (dc, dr);
    }
}

/// 主循环每个滴答调一次。先消化按键队列（中断攒下的事件），
/// 再按阶段处理：Play 里到步点走棋；Over 里等空格重开
pub fn on_tick(t: u64) {
    // 方向键走扩展码（0xE0 前缀），WASD 走普通字符码——两条路都通
    let mut restart = false;
    while let Some(k) = keyboard::pop_key() {
        match k {
            keyboard::KEY_UP => set_dir(0, -1),
            keyboard::KEY_DOWN => set_dir(0, 1),
            keyboard::KEY_LEFT => set_dir(-1, 0),
            keyboard::KEY_RIGHT => set_dir(1, 0),
            b'w' => set_dir(0, -1),
            b's' => set_dir(0, 1),
            b'a' => set_dir(-1, 0),
            b'd' => set_dir(1, 0),
            b' ' => restart = true,
            _ => {}
        }
    }

    if let Some(g) = GAME.get().as_mut() {
        match g.phase {
            Phase::Over => {
                if restart {
                    g.reset();
                }
            }
            Phase::Play => {
                if t % STEP_INTERVAL == 0 {
                    g.tick();
                }
            }
        }
    }
}

/// 开局：要画布、建状态、reset 摆好第一帧。console 就绪后调一次
pub fn init() {
    let (w, h) = console::pixel_size();
    let (cols, rows) = (w / CELL, h / CELL);
    let (pitch, size) = console::canvas();

    // 后备缓冲：整块屏外画布，从堆里要（堆此刻已扩到 4MB+）
    let buf = unsafe { alloc(Layout::from_size_align(size, 4096).unwrap()) };
    if buf.is_null() {
        return; // 堆不够用就不开局——不该发生，但别拿空指针画画
    }

    let mut g = Game {
        cols,
        rows,
        body: Vec::new(),
        dir: (1, 0),
        food: (0, 0),
        rng: interrupts::ticks() | 1, // |1：种子为 0 的 LCG 永远出 0，避开
        phase: Phase::Play,
        score: 0,
        buf,
        pitch,
        size,
    };
    g.reset(); // 摆初始蛇、放食物、第一帧上屏，全在 reset 里
    *GAME.get() = Some(g);
}
