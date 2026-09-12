//! 共享包装：UnsafeCell + 手写 Sync。
//! serial/console/mm 各有一份同样的定义——第 09 章统一到这个模块

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
