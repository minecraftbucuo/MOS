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
//!   2. 探测只在游戏里按 H 才跑，出事也只坏"按下 H 之后"的世界
//!   3. 每次解引用前先问 mm 的清单快照（in_ram/in_map），清单外不碰
//!   4. 路标实时打上屏——冻在哪一步，屏幕就停在哪一步
//!
//! 表布局的关键数字全部来自 ACPI 规范，不是猜的：
//!   RSDP：偏移 15 是版本号；0/1 版根表指针在偏移 16（32 位），
//!         2+ 版在偏移 24（64 位）——跟错表会数错位
//!   各表共用的头：36 字节（签名 4 + 总长 4 + 其余 28）
//!   HPET 子表：偏移 40 起是通用地址结构（4 个 u8 + 8 字节地址），
//!         寄存器物理地址在表偏移 48 处，天然不按 8 对齐，要小心读

use crate::console;
use crate::mm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// 开机记下的 RSDP 虚拟地址（Limine 给的是映射好的，可直接读）
static RSDP: AtomicU64 = AtomicU64::new(0);
/// 开机记下的 HHDM 偏移（物理地址 + 它 = 可读的虚拟地址）
static HHDM: AtomicU64 = AtomicU64::new(0);

/// HPET 寄存器的虚拟基地址（0 = 还没探测过 / 没找到）
static HPET_VBASE: AtomicU64 = AtomicU64::new(0);

/// 按过 H 了吗——没按过就拒绝清位操作
static PROBED: AtomicBool = AtomicBool::new(false);

// HPET 寄存器相对基地址的偏移（IA-PC HPET 规范定死）：
const REG_GENERAL_CONFIG: u64 = 0x10; // 总配置寄存器
/// bit1 = LEG_RT_CNF：legacy replacement 模式。
/// 置 1 时 8254 的 IRQ0 输出从总线上断开、由 HPET 定时器 0 顶替——
/// 固件若只设位不启用定时器 0，IRQ0 就两头死
const LEG_RT_CNF: u64 = 1 << 1;

/// 开机调用：只存两个数，不碰任何指针（见文件头的安全纪律）
pub fn stash(rsdp: *const u8, hhdm: u64) {
    RSDP.store(rsdp as u64, Ordering::Relaxed);
    HHDM.store(hhdm, Ordering::Relaxed);
}

/// 游戏里按 H 触发：走一遍指针链找 HPET。
/// 返回前 HPET_VBASE 要么是 0（没找到/不敢碰），要么是可读的寄存器地址
pub fn probe() {
    let rsdp = RSDP.load(Ordering::Relaxed) as *const u8;
    let hhdm = HHDM.load(Ordering::Relaxed);
    if rsdp.is_null() {
        console::print("acpi: no rsdp from bootloader\n");
        return;
    }
    let base = find_hpet(rsdp, hhdm);
    if let Some(vbase) = base {
        HPET_VBASE.store(vbase, Ordering::Relaxed);
    }
    PROBED.store(true, Ordering::Relaxed);
}

/// 诊断行用：None = 还没探测/没找到；Some(true) = legacy replacement 开着
pub fn legacy_route() -> Option<bool> {
    let base = HPET_VBASE.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    let cfg = unsafe { ((base + REG_GENERAL_CONFIG) as *const u64).read_volatile() };
    Some(cfg & LEG_RT_CNF != 0)
}

/// 游戏里按 C 触发：清掉 LEG_RT_CNF，8254 的 IRQ0 线理论上当场接回。
/// 前提是 H 探测确认过 L=1；对没设这个位的机器是写回原值，无副作用
pub fn try_clear() {
    if !PROBED.load(Ordering::Relaxed) {
        console::print("acpi: press H first\n");
        return;
    }
    let base = HPET_VBASE.load(Ordering::Relaxed);
    if base == 0 {
        console::print("acpi: no hpet, nothing to clear\n");
        return;
    }
    if legacy_route() != Some(true) {
        console::print("acpi: L is already 0, nothing to clear\n");
        return;
    }
    console::print("acpi: clearing LEG_RT_CNF...\n");
    let reg = (base + REG_GENERAL_CONFIG) as *mut u64;
    let cfg = unsafe { reg.read_volatile() };
    unsafe { reg.write_volatile(cfg & !LEG_RT_CNF) };
    console::print("acpi: cleared. watch T= go\n");
}

/// 按指针链找 HPET 寄存器基地址（虚拟）。每步先过护栏再解引用，
/// 全程只读、绝不 panic——诊断代码自己先倒下就什么都证明不了了
fn find_hpet(rsdp: *const u8, hhdm: u64) -> Option<u64> {
    console::print("acpi: rsdp @");
    console::print_hex(rsdp as u64);
    console::print("\n");

    // 护栏 1：RSDP 自己得住在 RAM 里。Limine 给的虚拟地址倒推回物理
    //（减去 HHDM 偏移），不在清单里就到此为止
    let rsdp_phys = rsdp as u64 - hhdm;
    if !mm::in_ram(rsdp_phys, 36) {
        console::print("acpi: rsdp not in RAM map, STOP\n");
        return None;
    }

    unsafe {
        let sig = core::slice::from_raw_parts(rsdp, 8);
        if sig != b"RSD PTR " {
            console::print("acpi: bad rsdp sig, STOP\n");
            return None;
        }

        // 版本决定跟哪张描述符表、指针多宽
        let rev = rsdp.add(15).read_volatile();
        let (sdt_phys, ptr_size) = if rev >= 2 {
            console::print("acpi: rev 2+\n");
            (read_u64(rsdp as u64 + 24), 8)
        } else {
            console::print("acpi: rev 0/1\n");
            (read_u32(rsdp as u64 + 16) as u64, 4)
        };

        // 护栏 2：描述符表在 RAM 里吗（先按表头大小验）
        if !mm::in_ram(sdt_phys, 36) {
            console::print("acpi: sdt ");
            console::print_hex(sdt_phys);
            console::print(" not in RAM map, STOP\n");
            return None;
        }
        let sdt = sdt_phys + hhdm;

        // 表头里的总长。防御：固件的表一般 1KB 以内，读出天文数字
        // 说明指针已经错了，再走下去只会踩雷
        let len = read_u32(sdt + 4) as usize;
        if !(36..=0x2000).contains(&len) {
            console::print("acpi: table len ");
            console::print_hex(len as u64);
            console::print(" insane, STOP\n");
            return None;
        }
        // 整张表都在 RAM 里吗
        if !mm::in_ram(sdt_phys, len as u64) {
            console::print("acpi: table crosses out of RAM, STOP\n");
            return None;
        }

        // 逐个走访子表指针，看签名找 HPET
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
            if !mm::in_ram(phys, 56) {
                console::print("acpi: entry ");
                console::print_hex(phys);
                console::print(" not in RAM map, skip\n");
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
            console::print("acpi: ");
            console::print(core::str::from_utf8(&s).unwrap_or("????"));
            console::print("\n");

            if s == *b"HPET" {
                // 通用地址结构的第一个字节是空间类型，0 = 系统内存。
                // HPET 芯片只会挂在内存上，别的值说明解析错位了
                if read_u8(table + 40) != 0 {
                    console::print("acpi: hpet addr space != mem, STOP\n");
                    return None;
                }
                let base_phys = read_u64(table + 48);
                console::print("acpi: hpet regs @");
                console::print_hex(base_phys);
                console::print("\n");

                // 护栏 3：寄存器是 MMIO，常不在 RAM 清单里（记成
                // reserved 或者压根不记）。读它 = 冒险，所以这是
                // 最后一步：先声明再动手，冻住也知道冻在哪
                if !mm::in_map(base_phys, 0x1000) {
                    console::print("acpi: hpet regs not in memmap, too risky, STOP\n");
                    return None;
                }
                console::print("acpi: reading hpet cfg (risk point)...\n");
                return Some(base_phys + hhdm);
            }
        }
    }
    console::print("acpi: no HPET table\n");
    None
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
