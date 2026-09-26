//! 共享包装：UnsafeCell + 手写 Sync。
//! serial/console/mm 各有一份同样的定义——第 09 章统一到这个模块

use core::arch::asm;
use core::cell::UnsafeCell;

pub struct Shared<T>(UnsafeCell<T>);

unsafe impl<T> Sync for Shared<T> {}

impl<T> Shared<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }
    pub fn get(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

/// 在"关中断临界区"里执行 f，结束后恢复进入前的中断状态。
/// 单核下这就是最可靠的锁：临界区内不会有任何代码插入执行。
pub fn without_interrupts<T>(f: impl FnOnce() -> T) -> T {
    let mut flags: u64;
    unsafe {
        // pushfq 把 RFLAGS 压栈，pop 读入 flags——保存中断标志原值
        asm!("pushfq", "pop {flags}", "cli",
             flags = out(reg) flags, options(nomem));
    }
    let r = f(); // 临界区：关着中断执行
    unsafe {
        // 写回保存的 RFLAGS（若本来中断就关着，这里不会误开）
        asm!("push {flags}", "popfq", flags = in(reg) flags, options(nomem));
    }
    r
}
