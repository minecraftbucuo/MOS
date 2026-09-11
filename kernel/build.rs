//! 构建脚本：cargo 在编译 crate 前会先运行它。
//!
//! 唯一的使命：声明 linker.ld 也是"源文件"之一。
//! 否则 cargo 只跟踪 .rs 文件的变化，改了链接脚本它也不会重编——
//! 我们刚才就踩过这个坑。

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
}
