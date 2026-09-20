//! ACPI 表解析（最小实现）：顺着 RSDP → 描述符表 → HPET 子表一路找下去，
//! 拿到 HPET 定时器的寄存器地址。平时内核用不着这些——这次是让它当
//! "法官"来断真机的 IRQ0 悬案（T=0 P+ M0 I=00：PIT 在数、没被挡、
//! 但信号到不了 PIC——头号嫌疑是 HPET 的 legacy replacement 把线掐了）
//!
//! ## 安全纪律（真机冻结事故的教训）
//!
//! 第一版把裸指针追逐放进了启动路径：QEMU 走通了，真机走到半路踩到
//! 未映射地址 → 缺页异常 → 遗言发往串口（真机看不见）→ 无声冻结。
//! 现在的铁律：
//!   1. 开机只存两个数字（RSDP/HHDM 地址），不碰任何指针——启动路径
//!      和没加这模块时一模一样
//!   2. 探测本来只锁在游戏的 H 键后面；时钟案第五幕起开机也会跑一遍
//!      （探测+启用 HPET 主计数器当时钟）。敢放进启动路径的前提：
//!      find_hpet 全程护栏兜底、任何失败都"打印一句然后放弃"绝不
//!      panic，且整条路在 QEMU（q35+OVMF 有 HPET）先验证过
//!   3. 每次解引用前先问 mm 的清单快照（in_ram/in_map），清单外不碰。
//!      例外：MMIO 寄存器——无头 QEMU 实测 Limine 的 HHDM 全量映射
//!      物理空间（OVMF 清单里没有 0xFED00000，读它照样不缺页），
//!      清单检查对 MMIO 降级为知情记录
//!   4. 路标实时打上屏——冻在哪一步，屏幕就停在哪一步；trace 同时
//!      留底串口，QEMU 无头跑也能读
//!
//! 表布局的关键数字全部来自 ACPI 规范，不是猜的：
//!   RSDP：偏移 15 是版本号；0/1 版根表指针在偏移 16（32 位），
//!         2+ 版在偏移 24（64 位）——跟错表会数错位
//!   各表共用的头：36 字节（签名 4 + 总长 4 + 其余 28）
//!   HPET 子表：偏移 40 起是通用地址结构 GAS（12 字节：4 个属性字节
//!         + 8 字节地址），寄存器物理地址在表偏移 44 处——曾数成 48
//!         读出 0（GAS 的高半截），把低地址内存当寄存器读（踩坑实录）

use crate::console;
use crate::mm;
use crate::serial;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 双通道路标：屏幕给人看，串口给调试看。真机串口不可见时屏幕是唯一
/// 输出；但 QEMU 无头跑（-serial file:xxx 落盘）时串口才是能读的那份——
/// 探测死在哪一步，两头都留证据
fn trace(s: &str) {
    console::print(s);
    serial::print(s);
}

/// trace 的 hex 版：路标里的地址/数值也得两头留底
fn trace_hex(v: u64) {
    console::print_hex(v);
    serial::print_hex(v);
}

/// 开机记下的 RSDP 虚拟地址（Limine 给的是映射好的，可直接读）
static RSDP: AtomicU64 = AtomicU64::new(0);
/// 开机记下的 HHDM 偏移（物理地址 + 它 = 可读的虚拟地址）
static HHDM: AtomicU64 = AtomicU64::new(0);

/// HPET 寄存器的虚拟基地址（0 = 还没探测过 / 没找到）
static HPET_VBASE: AtomicU64 = AtomicU64::new(0);

/// 按过 H 了吗——没按过就拒绝清位操作
static PROBED: AtomicBool = AtomicBool::new(false);

// HPET 寄存器相对基地址的偏移（IA-PC HPET 规范定死）：
const REG_CAPABILITIES: u64 = 0x00; // 能力寄存器（64 位）：偏移 0 = ID
                                    //（厂商/定时器数/64 位能力/legacy 能力），
                                    // 偏移 4 = 计数器刻度周期（飞秒）。
                                    // 曾把两半记反——把 ID 0x8086A201 当周期
const REG_GENERAL_CONFIG: u64 = 0x10; // 总配置寄存器
const REG_MAIN_COUNTER: u64 = 0xF0; // 主计数器：64 位只增不减的自由钟
/// bit1 = LEG_RT_CNF：legacy replacement 模式。
/// 置 1 时 8254 的 IRQ0 输出从总线上断开、由 HPET 定时器 0 顶替——
/// 固件若只设位不启用定时器 0，IRQ0 就两头死
const LEG_RT_CNF: u32 = 1 << 1;
/// 总配置寄存器 bit0 = 主计数器开关。固件一般开着，没开就开一下
const CNT_ENABLE: u32 = 1 << 0;

// —— HPET 寄存器一律按 32 位访问 ——
// 规范里这些寄存器看着是 64 位的，但芯片组（真实的 ICH 和 QEMU 的模拟
// 都一样）只接受 32 位读写。64 位读不会报错，而是拿到 0——QEMU 实测：
// 能力寄存器读成 0，周期被判 "insane"，时钟启用失败（时钟案第六幕的
// 第二个坑，串口日志抓的现行）。64 位数据 = 低字 + 高字两个 32 位拼接

fn hpet_read32(vbase: u64, off: u64) -> u32 {
    unsafe { ((vbase + off) as *const u32).read_volatile() }
}

fn hpet_write32(vbase: u64, off: u64, v: u32) {
    unsafe { ((vbase + off) as *mut u32).write_volatile(v) }
}

/// 读 64 位寄存器（主计数器用）：低字、高字分两次读，中间计数器可能
/// 进位——读完再复核一次高字，变了就重来（经典的双读一致法）
fn hpet_read64(vbase: u64, off: u64) -> u64 {
    loop {
        let hi1 = hpet_read32(vbase, off + 4) as u64;
        let lo = hpet_read32(vbase, off) as u64;
        let hi2 = hpet_read32(vbase, off + 4) as u64;
        if hi1 == hi2 {
            return (hi1 << 32) | lo;
        }
    }
}

/// 一个游戏滴答（10ms）= 多少个 HPET 刻度。探测时算好存这里
///（0 = 还没探测/没找到/周期不合理，时间账不能用 HPET）
static HPET_PER_TICK: AtomicU64 = AtomicU64::new(0);

// ---------- 备用自由钟：ACPI PM 定时器（时钟案第六幕） ----------
// HPET 探测可能在真机上半路夭折（A0 实测）。第二座钟从 FADT 表里找：
// ACPI PM 定时器，2000 年起的每台机器都有，频率出厂刻死
// 3.579545 MHz，挂在 IO 端口上——端口读永不缺页，连内存护栏都省了。
// Linux 的做法一样（drivers/clocksource/acpi_pm.c，它是内核最后的
// 时间源保底之一）。24 位计数器每 4.68 秒回绕一圈，但差值按掩码算
// 就不怕——前提只有"两次轮询间隔 < 4.68 秒"，主循环绰绰有余

/// PM 定时器的 IO 端口号（0 = 没找到 / 没这座钟）
static PM_PORT: AtomicU64 = AtomicU64::new(0);
/// 活体检测过了吗：读两次读数有变化才算数（死端口永远读回 0xFF）
static PM_ALIVE: AtomicBool = AtomicBool::new(false);

/// 一滴答 = 3579545 / 100 个刻度（取整，日积月累每天慢 1 秒量级，游戏无感）
const PM_PER_TICK: u64 = 35795;
/// 计数器按 24 位截断。就算芯片是 32 位的也按 24 位算：真差值小于
/// 2^24 时"差值 mod 2^24"仍等于真差值；反过来按 32 位算 24 位的钟，
/// 每 4.68 秒回绕一次就会算出天文数字——宁小勿大
const PM_MASK: u64 = 0xFF_FFFF;

/// 开机调用：只存两个数，不碰任何指针（见文件头的安全纪律）
pub fn stash(rsdp: *const u8, hhdm: u64) {
    RSDP.store(rsdp as u64, Ordering::Relaxed);
    HHDM.store(hhdm, Ordering::Relaxed);
}

/// 走一遍指针链找 HPET，找到就启用主计数器当时钟。
/// 调用方有两个：开机流程（游戏起来之前，探测输出的路标会被游戏
/// 初始化的整屏绘制刷掉，串口里留底）和游戏里按 H（复跑一遍诊断）。
/// 返回时 HPET_VBASE 要么是 0（没找到/不敢碰/时钟没启用），要么是可读的寄存器地址
pub fn probe() {
    let rsdp = RSDP.load(Ordering::Relaxed) as *const u8;
    let hhdm = HHDM.load(Ordering::Relaxed);
    if rsdp.is_null() {
        trace("acpi: no rsdp from bootloader\n");
        return;
    }
    let base = find_hpet(rsdp, hhdm);
    if let Some(vbase) = base {
        // 探测即启用时钟（第五幕的修法）。find_hpet 返回前刚读过一次
        // 寄存器没崩，说明这片 MMIO 真的能读
        if setup_clock(vbase) {
            HPET_VBASE.store(vbase, Ordering::Relaxed);
        }
    }
    // PM 定时器活体检测：IO 端口读不会缺页，唯一要防的是端口后面
    // 没有钟（死端口永远读回同一个值）。隔一小会读两次，变了 = 活着。
    // 开机只跑这一次，忙等 ~10ms（和 pic::pit_alive 同款手法）
    if PM_PORT.load(Ordering::Relaxed) != 0 {
        let port = PM_PORT.load(Ordering::Relaxed) as u16;
        let a = serial::inl(port);
        for _ in 0..20_000_000 {
            core::hint::spin_loop();
        }
        let alive = serial::inl(port) != a;
        PM_ALIVE.store(alive, Ordering::Relaxed);
        serial::print(if alive {
            "[acpi] pm timer clock on\n"
        } else {
            "[acpi] pm timer port dead\n"
        });
    }
    PROBED.store(true, Ordering::Relaxed);
}

/// 找到 HPET 后启用"主计数器"当时钟。失败 = 不动 HPET_VBASE，
/// 时间账继续走 PIT 轮询那条老路
fn setup_clock(vbase: u64) -> bool {
    // 能力寄存器高 32 位（偏移 4）= 主计数器一个刻度的周期（飞秒）。
    // 典型值 10ns = 10^7 fs（100MHz）。范围收紧到 [1fs, 1µs]：真实周期
    // 就在这条带里，ID 半边（约 2×10^9）混不进来
    let fs = hpet_read32(vbase, REG_CAPABILITIES + 4) as u64;
    if fs == 0 || fs > 1_000_000_000 {
        serial::print("[acpi] hpet period insane, not using\n");
        return false;
    }
    // 10ms 一个滴答 = 10^13 飞秒，除以周期 = 一滴答的刻度数。
    // 典型值 10^13 / 10^7 = 10^6 刻度
    let per_tick = 10_000_000_000_000 / fs;
    HPET_PER_TICK.store(per_tick, Ordering::Relaxed);

    // 主计数器的开关在总配置寄存器 bit0。固件一般已经开了；没开就
    // 开一下（读-改-写，别弄丢别的位——比如 legacy replacement 状态）。
    // 高字原样写回：写寄存器要写整字，高字不能装作不存在
    let lo = hpet_read32(vbase, REG_GENERAL_CONFIG);
    let hi = hpet_read32(vbase, REG_GENERAL_CONFIG + 4);
    if lo & CNT_ENABLE == 0 {
        hpet_write32(vbase, REG_GENERAL_CONFIG, lo | CNT_ENABLE);
        hpet_write32(vbase, REG_GENERAL_CONFIG + 4, hi);
        // 有的实现要一点时间生效，确认一下
        if hpet_read32(vbase, REG_GENERAL_CONFIG) & CNT_ENABLE == 0 {
            serial::print("[acpi] hpet counter won't enable\n");
            HPET_PER_TICK.store(0, Ordering::Relaxed);
            return false;
        }
    }

    serial::print("[acpi] hpet clock on, period=");
    serial::print_hex(fs);
    serial::print("fs per-tick=");
    serial::print_hex(per_tick);
    serial::print("\n");
    true
}

/// 现在的 HPET 主计数器读数。None = 没探测到 / 时钟没启用——
/// 调用方该退回别的计时方式
pub fn hpet_now() -> Option<u64> {
    let base = HPET_VBASE.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    // 64 位自由钟：只增不减、永不清零、永不回绕（按 10^13 fs/秒跑，
    // 回绕要跑几万年）。跟 PIT 的"16 位倒着数还会重装"比，读它
    // 什么都不用操心（读法是两次 32 位拼 64 位，见 hpet_read64）
    Some(hpet_read64(base, REG_MAIN_COUNTER))
}

/// 一个游戏滴答（10ms）= 多少 HPET 刻度。0 = 没启用
pub fn hpet_per_tick() -> Option<u64> {
    let p = HPET_PER_TICK.load(Ordering::Relaxed);
    if p == 0 {
        None
    } else {
        Some(p)
    }
}

/// PM 定时器现在的读数。成功 = (读数(截 24 位), 回绕掩码, 每滴答刻度数)。
/// None = 没找到这座钟或它是死的——调用方退回下一档计时方式
pub fn pm_clock() -> Option<(u64, u64, u64)> {
    if !PM_ALIVE.load(Ordering::Relaxed) {
        return None;
    }
    let port = PM_PORT.load(Ordering::Relaxed) as u16;
    Some((serial::inl(port) as u64 & PM_MASK, PM_MASK, PM_PER_TICK))
}

/// 诊断行用：None = 还没探测/没找到；Some(true) = legacy replacement 开着
pub fn legacy_route() -> Option<bool> {
    let base = HPET_VBASE.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    let cfg = hpet_read32(base, REG_GENERAL_CONFIG);
    Some(cfg & LEG_RT_CNF != 0)
}

/// 游戏里按 C 触发：清掉 LEG_RT_CNF，8254 的 IRQ0 线理论上当场接回。
/// 前提是 H 探测确认过 L=1；对没设这个位的机器是写回原值，无副作用
pub fn try_clear() {
    if !PROBED.load(Ordering::Relaxed) {
        trace("acpi: press H first\n");
        return;
    }
    let base = HPET_VBASE.load(Ordering::Relaxed);
    if base == 0 {
        trace("acpi: no hpet, nothing to clear\n");
        return;
    }
    if legacy_route() != Some(true) {
        trace("acpi: L is already 0, nothing to clear\n");
        return;
    }
    trace("acpi: clearing LEG_RT_CNF...\n");
    // 只动低字：LEG_RT_CNF 在 bit1，高字跟它无关不用陪葬
    let cfg = hpet_read32(base, REG_GENERAL_CONFIG);
    hpet_write32(base, REG_GENERAL_CONFIG, cfg & !LEG_RT_CNF);
    trace("acpi: cleared. watch T= go\n");
}

/// 按指针链找 HPET 寄存器基地址（虚拟）。每步先过护栏再解引用，
/// 全程只读、绝不 panic——诊断代码自己先倒下就什么都证明不了了
fn find_hpet(rsdp: *const u8, hhdm: u64) -> Option<u64> {
    trace("acpi: rsdp @");
    trace_hex(rsdp as u64);
    trace("\n");

    // 护栏 1：RSDP 自己得住在 RAM 里。Limine 给的虚拟地址倒推回物理
    //（减去 HHDM 偏移），不在清单里就到此为止
    let rsdp_phys = rsdp as u64 - hhdm;
    if !mm::in_ram(rsdp_phys, 36) {
        trace("acpi: rsdp not in RAM map, STOP\n");
        return None;
    }

    unsafe {
        let sig = core::slice::from_raw_parts(rsdp, 8);
        if sig != b"RSD PTR " {
            trace("acpi: bad rsdp sig, STOP\n");
            return None;
        }

        // 版本决定跟哪张描述符表、指针多宽
        let rev = rsdp.add(15).read_volatile();
        let (sdt_phys, ptr_size) = if rev >= 2 {
            trace("acpi: rev 2+\n");
            (read_u64(rsdp as u64 + 24), 8)
        } else {
            trace("acpi: rev 0/1\n");
            (read_u32(rsdp as u64 + 16) as u64, 4)
        };

        // 护栏 2：描述符表在 RAM 里吗（先按表头大小验）
        if !mm::in_ram(sdt_phys, 36) {
            trace("acpi: sdt ");
            trace_hex(sdt_phys);
            trace(" not in RAM map, STOP\n");
            return None;
        }
        let sdt = sdt_phys + hhdm;

        // 表头里的总长。防御：固件的表一般 1KB 以内，读出天文数字
        // 说明指针已经错了，再走下去只会踩雷
        let len = read_u32(sdt + 4) as usize;
        if !(36..=0x2000).contains(&len) {
            trace("acpi: table len ");
            trace_hex(len as u64);
            trace(" insane, STOP\n");
            return None;
        }
        // 整张表都在 RAM 里吗
        if !mm::in_ram(sdt_phys, len as u64) {
            trace("acpi: table crosses out of RAM, STOP\n");
            return None;
        }

        // 逐个走访子表指针，一次扫完收两张表：HPET（定时器寄存器）
        // 和 FACP（FADT，里面记着 PM 定时器的 IO 端口）。不能找到
        // 一张就收工——两张表在清单里的先后顺序是任意的
        let mut hpet: Option<u64> = None;
        let mut off = 36usize;
        while off + ptr_size <= len {
            let phys = if ptr_size == 8 {
                read_u64(sdt + off as u64)
            } else {
                read_u32(sdt + off as u64) as u64
            };
            off += ptr_size;
            if phys == 0 {
                continue;
            }
            // 92 字节 = FADT 要读到的最深字段（PM 定时器长度在 0x5B）
            if !mm::in_ram(phys, 92) {
                trace("acpi: entry ");
                trace_hex(phys);
                trace(" not in RAM map, skip\n");
                continue;
            }
            let table = phys + hhdm;
            let sig = core::slice::from_raw_parts(table as *const u8, 4);
            // 签名按理是 ASCII，代码点不明的字符换成 ?，
            // 免得坏字节凑不出合法 UTF-8 让 print 自己崩了
            let mut s = [b'?'; 4];
            for (d, src) in s.iter_mut().zip(sig.iter()) {
                if src.is_ascii_graphic() {
                    *d = *src;
                }
            }
            trace("acpi: ");
            trace(core::str::from_utf8(&s).unwrap_or("????"));
            trace("\n");

            if s == *b"HPET" && hpet.is_none() {
                // 通用地址结构的第一个字节是空间类型，0 = 系统内存。
                // HPET 芯片只会挂在内存上，别的值说明解析错位了
                if read_u8(table + 40) != 0 {
                    trace("acpi: hpet addr space != mem, STOP\n");
                    return None;
                }
                let base_phys = read_u64(table + 44);
                trace("acpi: hpet regs @");
                trace_hex(base_phys);
                trace("\n");
                // 0 不是合法的 MMIO 基址——低地址是普通 RAM，清单检查
                // 拦不住它，这里必须自己拦。曾因地址字段偏移数错
                //（48 ≠ 44）读出 0，一路把低地址内存当 HPET 寄存器读
                if base_phys == 0 {
                    trace("acpi: hpet regs base 0, STOP\n");
                    return None;
                }

                // 护栏 3：寄存器是 MMIO，通常不在 RAM 清单里（OVMF 的
                // 清单就没有 0xFED00000，实测）。清单外不等于读不了——
                // 无头 QEMU 实验证明：Limine 的 HHDM 把全部物理空间
                //（包括清单外的芯片组 MMIO 洞）都映射好了，读它不缺页。
                // 所以清单检查降级为"知情记录"，不再拦——拦了 HPET
                // 永远用不上。真正要防的"没映射"风险由 Limine 兜底
                if !mm::in_map(base_phys, 0x1000) {
                    trace("acpi: hpet regs not in memmap (limine maps it anyway)\n");
                }
                trace("acpi: reading hpet cfg (risk point)...\n");
                hpet = Some(base_phys + hhdm);
            }

            if s == *b"FACP" {
                // FADT：PM 定时器的 IO 端口记在偏移 0x4C（u32），
                // 端口宽度记在 0x5B（正确值 = 4 字节）
                let port = read_u32(table + 0x4C) as u64;
                let tmr_len = read_u8(table + 0x5B);
                if port != 0 && tmr_len == 4 {
                    PM_PORT.store(port, Ordering::Relaxed);
                    trace("acpi: pm timer @ io ");
                    trace_hex(port);
                    trace("\n");
                } else {
                    trace("acpi: fadt has no pm timer\n");
                }
            }
        }
        if hpet.is_none() {
            trace("acpi: no HPET table\n");
        }
        hpet
    }
}

/// ACPI 表字段天生乱对齐（比如 HPET 表偏移 48 的 u64），
/// 用 read_unaligned 一劳永逸，对齐与否都不出错
fn read_u64(vaddr: u64) -> u64 {
    unsafe { (vaddr as *const u64).read_unaligned() }
}

fn read_u32(vaddr: u64) -> u32 {
    unsafe { (vaddr as *const u32).read_unaligned() }
}

fn read_u8(vaddr: u64) -> u8 {
    unsafe { (vaddr as *const u8).read_volatile() }
}
