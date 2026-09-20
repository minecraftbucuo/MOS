//! IDT：中断描述符表——"门铃编号 → 处理函数"的电话簿。
//!
//! 处理函数入口用手写汇编存根（stable 工具链没有 x86-interrupt
//! 调用约定，和当初的 limine crate 同一个处境）。

use crate::acpi;
use crate::console;
use crate::gdt::KERNEL_CODE_SELECTOR;
use crate::keyboard;
use crate::pic;
use crate::serial;
use core::arch::asm;
use core::arch::naked_asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

/// 中断现场的完整布局（字段从低地址到高地址，与存根压栈顺序一致）。
/// 寄存器字段是给调试输出留的，目前只读 rip——CPU 看不见这些，同 BOOT_STACK
#[allow(dead_code)]
#[repr(C)]
pub struct IsrFrame {
    // 通用寄存器（按压栈顺序）
    pub rax: u64, pub rcx: u64, pub rdx: u64, pub rbx: u64,
    pub rbp: u64, pub rsi: u64, pub rdi: u64,
    pub r8: u64, pub r9: u64, pub r10: u64, pub r11: u64,
    pub r12: u64, pub r13: u64, pub r14: u64, pub r15: u64,
    // 错误码（无错误码的异常由存根压 0 占位）
    pub error_code: u64,
    // 以下是 CPU 进入存根前自动压的
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// 生成一个中断存根。
/// $pre：无错误码的异常传 "push 0" 占位；有错误码的传 "nop"
///（CPU 已经压了真错误码，不能再压一份）
macro_rules! stub {
    ($name:ident, $handler:path, $pre:literal) => {
        #[unsafe(naked)]
        extern "C" fn $name() -> ! {
            naked_asm!(
                $pre,
                // 保存全部通用寄存器：被打断的代码不知道自己被中断过，
                // 一个都不能弄脏
                "push r15", "push r14", "push r13", "push r12",
                "push r11", "push r10", "push r9", "push r8",
                "push rdi", "push rsi", "push rbp", "push rbx",
                "push rdx", "push rcx", "push rax",
                // rbp 已保存在栈上，借来记帧位置并摆正栈：
                // Rust 函数要求进入时 rsp 按 16 对齐，而被打断的
                // 代码栈可没这个保证
                "mov rbp, rsp",
                "and rsp, -16",
                "mov rdi, rbp", // 第一个参数 = 帧指针
                "call {h}",
                "mov rsp, rbp", // 回到帧，开始恢复
                "pop rax", "pop rcx", "pop rdx", "pop rbx",
                "pop rbp", "pop rsi", "pop rdi",
                "pop r8", "pop r9", "pop r10", "pop r11",
                "pop r12", "pop r13", "pop r14", "pop r15",
                "add rsp, 8", // 丢弃错误码占位（或 CPU 压的真错误码）
                "iretq",
                h = sym $handler,
            );
        }
    };
}

// ---------- 异常处理函数（纯 Rust，从存根进入） ----------

/// 公用：汇报异常名和出事地址
fn report(name: &str, frame: &IsrFrame) {
    serial::print("\n[exception] ");
    serial::print(name);
    serial::print(" rip=");
    serial::print_hex(frame.rip);
    serial::print("\n");
}

extern "C" fn exc_divide_error(frame: &mut IsrFrame) {
    report("#DE 除零", frame);
}

extern "C" fn exc_breakpoint(frame: &mut IsrFrame) {
    report("#BP 断点", frame);
}

extern "C" fn exc_invalid_opcode(frame: &mut IsrFrame) {
    report("#UD 非法指令", frame);
}

/// 双重故障：异常处理过程中又出了异常。当前栈很可能已坏，
/// 所以它走 IST1 备用栈进门（见 IDT 登记处）。到这里没有"恢复"可言，
/// 打印遗言后停机——它的价值是把"无声复位"变成"留了句话"
extern "C" fn exc_double_fault(frame: &mut IsrFrame) -> ! {
    report("#DF 双重故障", frame);
    loop {
        unsafe { asm!("hlt") };
    }
}

/// 兜底：没登记处理逻辑的异常都到这。不返回——直接停机
extern "C" fn exc_unexpected(frame: &mut IsrFrame) -> ! {
    report("unexpected", frame);
    loop {
        unsafe { asm!("hlt") }; // 休眠等中断，比忙转省电
    }
}

// 存根：除零/断点/非法指令无错误码，压 0 占位；
// 双重故障有错误码（CPU 自己压），用 nop 顶替那行；
// 时钟/键盘中断（IRQ）也无错误码
stub!(stub_divide_error, exc_divide_error, "push 0");
stub!(stub_breakpoint, exc_breakpoint, "push 0");
stub!(stub_invalid_opcode, exc_invalid_opcode, "push 0");
stub!(stub_double_fault, exc_double_fault, "nop");
stub!(stub_unexpected, exc_unexpected, "push 0");
stub!(stub_timer, exc_timer, "push 0");
stub!(stub_keyboard, exc_keyboard, "push 0");

// ---------- 时钟中断 ----------

/// 滴答计数：系统的心跳秒表。
/// 原子变量——"读出来加一写回去"可能被下一次中断拦腰打断
static TICKS: AtomicU64 = AtomicU64::new(0);

/// 一个滴答（10ms）对应多少个 PIT 输入时钟（1193182 ÷ 100）。
/// 注意与"当前除数"解耦：轮询接管后除数会放宽到 65536（见
/// pic::widen_pit_period），但"多少时钟算一拍"永远是 100Hz 的口径
const CLOCKS_PER_TICK: u64 = 11931;

/// 开机以来的滴答数（100 滴答 = 1 秒）。
/// 游戏模块拿它当随机种子和节拍；主循环轮询它，中断里不干活
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// 滴答满 100 报个到（中断版和轮询版共用，免得两处一样的代码）
fn announce_tick(total: u64) {
    if total % 100 == 0 {
        serial::print("[timer] ");
        serial::print_hex(total / 100);
        serial::print("s\n");
    }
}

/// 时钟处理函数：每次滴答 +1，每满 100 次（1 秒）报个到，交回执
extern "C" fn exc_timer(_frame: &mut IsrFrame) {
    announce_tick(TICKS.fetch_add(1, Ordering::Relaxed) + 1);
    // 回执！忘掉这行，时钟只会响一次
    pic::send_eoi(0);
}

// ---------- 备用心跳：轮询 PIT（真机时钟案的救场） ----------
// 实测（华硕天选5 Pro）：IRQ0 一次都不来（T=0 P+ M0 I=00），但 PIT
// 芯片自己在数数（P+）。既然节拍源活着、只是"中断"这条路断了——
// 那就不靠中断，主循环直接读计数器。
//
// 第一版数"数满一圈"这个事件，真机上蛇明显偏慢：事件会丢——CPU 被
// 固件用 SMM 叫走几毫秒，那几圈就白白漏掉。第二版改成记时间：每次
// 轮询读计数器，两次读数的差 = 这段时间走过的 PIT 时钟数（mode 2
// 每时钟正好 -1，重装回到除数值），累计起来除以除数 = 滴答数。
// 漏轮询不要紧——差值把漏掉的时间自动补上，只要两次轮询间隔
// 小于一个周期（10ms）就分毫不差。
//
// 第三幕：这个"只要"在真机上也不成立——每步棋的整屏翻页要把 16MB
// 拷进显存，显存写得慢，一次拷几十上百毫秒，泡在拷贝里时 PIT 早就
// 转了一圈以上，而差值最多只认一个周期的账，多转的时间全丢
//（QEMU 里画布小、"显存"就是内存，怎么测都是准的——教训：要验证
// 前提本身，不只是公式）。两头修：游戏侧改脏矩形翻页（拷贝量缩
// 20 倍，见 game.rs），这里侧接管时把周期放宽到 54.9ms（量程扩容）。
//
// 谁来给 TICKS +1 的分工：QEMU 里 IRQ0 正常，滴答由中断函数加；
// 真滴答落后"应有的滴答数"（= PIT 重装数）3 拍以上，判定这条线
// 不可信（死了/被掐了/活着但被固件定慢了），轮询接管。

/// 上次读到的 PIT 计数值
static LAST_COUNT: AtomicU16 = AtomicU16::new(0);
/// 累计走过的 PIT 输入时钟数（真表：1193182/秒）
static ELAPSED_PIT: AtomicU64 = AtomicU64::new(0);
/// 轮询已接管节拍了吗
static FALLBACK: AtomicBool = AtomicBool::new(false);
/// 开机以来 PIT 重装了几次。周期 10ms（100Hz 口径）下，重装一次 =
/// 本该走一拍——重装数就是"此刻应有的滴答数"，拿它和真滴答比，
/// IRQ0 这条线死了没、跑得快不快，一目了然
static WRAPS: AtomicU64 = AtomicU64::new(0);

// ---------- 第五幕：HPET 主计数器时间账 ----------
// 真机实测（T=0 P+ M0 I=00 L- F0）：IRQ0 在响但节奏不对、PIT 在转但
// 快慢不对——两条线共用同一颗"被改过除数"的 PIT，互相验证永远验不出
// 问题。架构性修法：不再信任何"可编程的"节拍器，学 Linux 用
// free-running 时钟记账。HPET 主计数器是刻在芯片里的：频率出厂定死
//（写在能力寄存器里，SMM 改不了）、64 位只增不减（没有量程问题，
// 轮询间隔多长都不丢时间）。探测成功后时间账搬到它身上，PIT 轮询
// 整个降级为"没有 HPET 的机器"的备用。

/// 上次读到的 HPET 主计数器值
static HPET_LAST: AtomicU64 = AtomicU64::new(0);
/// 累计走过的 HPET 刻度数
static HPET_ELAPSED: AtomicU64 = AtomicU64::new(0);
/// 第一次轮询只记基线不算差值（开机到第一次轮询之间隔了多少，没人知道）
static HPET_INITED: AtomicBool = AtomicBool::new(false);

/// 同上三件套，给 PM 定时器（HPET 探测失败的机器的第二座自由钟）
static PM_LAST: AtomicU64 = AtomicU64::new(0);
static PM_ELAPSED: AtomicU64 = AtomicU64::new(0);
static PM_INITED: AtomicBool = AtomicBool::new(false);

/// 主循环每次迭代调一次：记账时间，必要时替死掉的 IRQ0 打拍子。
/// 记账优先级：HPET 主计数器（刻度出厂定死，最可信）→ PIT 轮询
///（没有 HPET 的机器的备用）
pub fn poll_pit_fallback() {
    // HPET 主计数器活着：时间账记在它身上。两次读数的差 = 走过的
    // 刻度数——64 位自由钟永不重装、永不回绕，差值直接相减就行，
    // 不用像 PIT 那样处理"转到一半重装"的绕圈账，也没有
    // "轮询间隔必须小于一个周期"的前提
    if let (Some(now), Some(per_tick)) = (acpi::hpet_now(), acpi::hpet_per_tick()) {
        let prev = HPET_LAST.swap(now, Ordering::Relaxed);
        if HPET_INITED.swap(true, Ordering::Relaxed) {
            HPET_ELAPSED.fetch_add(now.wrapping_sub(prev), Ordering::Relaxed);
            let want = HPET_ELAPSED.load(Ordering::Relaxed) / per_tick;
            let old = TICKS.fetch_max(want, Ordering::Relaxed);
            if want > old {
                announce_tick(want);
            }
        }
        return;
    }

    // PM 定时器：HPET 没找到时的第二座自由钟（ACPI PM 定时器，
    // 3.579545MHz 出厂定死，IO 端口访问）。它只有 24 位，每 4.68 秒
    // 回绕一圈——差值按掩码取模就绕过去了，前提"两次轮询间隔
    // < 4.68 秒"对主循环来说绰绰有余
    if let Some((now, mask, per_tick)) = acpi::pm_clock() {
        let prev = PM_LAST.swap(now, Ordering::Relaxed);
        if PM_INITED.swap(true, Ordering::Relaxed) {
            let delta = now.wrapping_sub(prev) & mask;
            PM_ELAPSED.fetch_add(delta, Ordering::Relaxed);
            let want = PM_ELAPSED.load(Ordering::Relaxed) / per_tick;
            let old = TICKS.fetch_max(want, Ordering::Relaxed);
            if want > old {
                announce_tick(want);
            }
        }
        return;
    }

    let c = pic::pit_count();
    let prev = LAST_COUNT.swap(c, Ordering::Relaxed);

    // 记时间：mode 2 计数器往下走、数到底重装回除数值。两次读数走过的
    // 时钟数 = prev - c；碰上重装（c 反超 prev）就加一个周期补上。
    // 注意不能拿 u32 回绕减法再取模——2^32 不是除数的倍数，模出来的
    // 是垃圾数（模拟环境实测：每次重装多记约一拍，秒表跳着走）
    let divisor = pic::pit_divisor();
    if divisor > 0 {
        let delta = (prev as u32 + divisor - c as u32) % divisor;
        ELAPSED_PIT.fetch_add(delta as u64, Ordering::Relaxed);
    }

    // 已接管：累计时钟 ÷ 每滴答时钟数 = 现在应该是第几拍，取整后顶进
    // TICKS。用 fetch_max 而不是 fetch_add：万一 IRQ0 哪天复活了（比如
    // 将来按 C 把线接回去），中断和轮询同时动 TICKS，累加会双倍速，
    // 取最大值永远只跟真实时间走。
    // 关键是**每次轮询都同步**，不能攒到重装才同步——量程扩容到
    // 54.9ms 后，TICKS 每 55ms 才跳 5.5 拍，游戏里"步点"的判断整段
    // 整段地漏拍，真机上蛇慢成五分之一（时钟案第四幕：扩容量程和
    // 步点判断打架，两头修才是修）
    if FALLBACK.load(Ordering::Relaxed) {
        let elapsed = ELAPSED_PIT.load(Ordering::Relaxed);
        let want = elapsed / CLOCKS_PER_TICK;
        let old = TICKS.fetch_max(want, Ordering::Relaxed);
        if want > old {
            announce_tick(want);
        }
        return;
    }

    // 还没接管：重装（计数值往上跳）除了记账，还是验铃的节拍器。
    // 每重装一次 = 本该走一拍。真滴答落后重装数 3 拍以上（30ms 的
    // 欠账），说明 IRQ0 这条线不可信——死了、被 SMM 挤掉了、或者
    // 活着但跑得慢（HPET 顶替时节拍是固件定的，50~100Hz 之间的
    // 慢铃，旧判据"连续两圈没动静"抓不到：两圈之间总有铃响）。
    // 落后判据对快慢死活一律成立，正常线最多瞬时落后 1 拍，
    // 永远够不着 3——QEMU 不会被误接管
    if c <= prev {
        return;
    }
    let wraps = WRAPS.fetch_add(1, Ordering::Relaxed) + 1;
    let t = TICKS.load(Ordering::Relaxed);
    if t + 3 <= wraps {
        FALLBACK.store(true, Ordering::Relaxed);
        // 量程扩容：10ms 的周期太脆——主循环被拖住 10ms 以上
        //（慢显存拷贝、SMM），时间就漏。周期放宽到 54.9ms 后，
        // 计数器马上从新除数起数，LAST_COUNT 同步成满量程，
        // 下一轮差值从新量程里算
        pic::widen_pit_period();
        LAST_COUNT.store(0xFFFF, Ordering::Relaxed);
        serial::print("[timer] IRQ0 lagging, poll fallback took over\n");
        console::print("timer: IRQ0 lagging, polling PIT\n");
    }
}

/// 备用心跳接管了吗（游戏的自愈翻页要问：真机轮询路径拷不起整屏，
/// 跳过自愈；QEMU 一类环境显存快，定期整屏重抄愈合撕裂行）
pub fn using_fallback() -> bool {
    FALLBACK.load(Ordering::Relaxed)
}

/// 键盘处理函数：读扫描码翻译上屏，交回执
extern "C" fn exc_keyboard(_frame: &mut IsrFrame) {
    keyboard::on_interrupt();
    pic::send_eoi(1); // IRQ1 的回执
}

// ---------- IDT 表 ----------

/// IDT 表项（64 位模式一项 16 字节）
#[derive(Clone, Copy)]
struct Gate {
    low: u64,  // 低 8 字节
    high: u64, // 高 8 字节
}

impl Gate {
    const fn missing() -> Self {
        Gate { low: 0, high: 0 }
    }

    /// 登记处理函数。ist=0 用当前栈；1~7 用 TSS 第 n 个备用栈
    fn set_handler(&mut self, stub: usize, ist: u8) {
        let addr = stub as u64;
        self.low = (addr & 0xFFFF)                     // 函数地址 0..15 位
            | ((KERNEL_CODE_SELECTOR as u64) << 16)   // 用哪个代码段
            | (((ist & 0x7) as u64) << 32)            // 备用栈编号
            | (0x8E << 40)                             // 类型字节：中断门
            | (((addr >> 16) & 0xFFFF) << 48);         // 函数地址 16..31 位
        self.high = addr >> 32;                        // 函数地址 32..63 位
    }
}

static mut IDT: [Gate; 256] = [Gate::missing(); 256];

/// IDTR：lidt 读的"表在哪、多长"（和 GDTR 一个意思）
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// 建表并装载。kmain 里、开中断前调用一次
pub fn init() {
    unsafe {
        let idt = &raw mut IDT;

        // 有正经处理逻辑的异常
        (*idt)[0].set_handler(stub_divide_error as *const () as usize, 0);
        (*idt)[3].set_handler(stub_breakpoint as *const () as usize, 0);
        (*idt)[6].set_handler(stub_invalid_opcode as *const () as usize, 0);
        // 双重故障：走 IST1 应急栈（登记里的 1 就是表项里的 IST 编号）
        (*idt)[8].set_handler(stub_double_fault as *const () as usize, 1);

        // 其余 CPU 异常（1~31 号中没专门登记的）全部兜底
        for i in 0..32 {
            if (*idt)[i].low == 0 {
                (*idt)[i].set_handler(stub_unexpected as *const () as usize, 0);
            }
        }

        // IRQ0（时钟）→ 中断号 32（PIC 重映射后的新门牌）
        (*idt)[32].set_handler(stub_timer as *const () as usize, 0);
        // IRQ1（键盘）→ 中断号 33
        (*idt)[33].set_handler(stub_keyboard as *const () as usize, 0);

        let idtr = Idtr {
            limit: (size_of::<[Gate; 256]>() - 1) as u16,
            base: idt as u64,
        };
        asm!("lidt [rdi]", in("rdi") &idtr as *const Idtr);
    }
}
