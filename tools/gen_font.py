#!/usr/bin/env python3
"""从 Linux 控制台字体提取点阵，生成 kernel/src/font.rs"""
import gzip
import struct
import sys

FONT = "/usr/share/kbd/consolefonts/cp850-8x16.psfu.gz"
OUT = "kernel/src/font.rs"

with gzip.open(FONT, "rb") as f:
    data = f.read()

# 认格式：看魔数（PSF2: 72 B5 4A 86，PSF1: 36 04）
if data[:4] == b"\x72\xb5\x4a\x86":  # PSF2
    (version, headersize, flags,
     length, charsize, height, width) = struct.unpack("<7I", data[4:32])
    assert width == 8, "不是 8 像素宽的字体"
elif data[:2] == b"\x36\x04":  # PSF1
    mode, charsize = data[2], data[3]
    headersize = 4
else:
    sys.exit("不认识的字体格式")

# 切出前 256 个字形，每个 charsize 字节
glyphs = data[headersize:headersize + 256 * charsize]
assert len(glyphs) == 256 * charsize, "字形数量不足 256"

# 排成 Rust 源码
lines = [
    "//! 字模表：由 tools/gen_font.py 从 cp850-8x16.psfu.gz 生成，勿手改",
    "//! 每字符 16 行 × 1 字节/行（8 像素），最高位 = 最左像素",
    "",
    "pub const GLYPH_WIDTH: usize = 8;",
    "pub const GLYPH_HEIGHT: usize = 16;",
    "",
    "pub const GLYPHS: [[u8; GLYPH_HEIGHT]; 256] = [",
]
for i in range(256):
    rows = glyphs[i * charsize:(i + 1) * charsize]
    hexes = ", ".join(f"0x{b:02X}" for b in rows)
    label = chr(i) if 32 <= i < 127 else ""  # 可打印字符在行尾标注
    lines.append(f"    [{hexes}], // 0x{i:02X} {label}")
lines += ["];", ""]

with open(OUT, "w") as f:
    f.write("\n".join(lines))
print(f"OK: {OUT}（{256 * charsize} 字节字模数据）")
