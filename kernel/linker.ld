/*
 * 内核链接脚本 —— 告诉链接器"内核的各个节放在哪些虚拟地址"
 *
 * 我们采用"高地址内核"布局：内核链接在 0xFFFFFFFF80000000。
 * 为什么不在低地址（如 0x100000）？
 *   低地址区域将来要留给用户程序，内核自己搬到地址空间的最顶端，
 *   用户态(低)和内核态(高)就永远不打架，这是主流操作系统的做法。
 *   Limine 会负责建立页表，把这个虚拟地址映射到实际的物理内存。
 */

ENTRY(_start)                  /* 入口符号：Limine 跳转到 _start（汇编入口） */

KERNEL_BASE = 0xFFFFFFFF80000000;

SECTIONS
{
    /* '.' 是"当前位置计数器"，把它拨到内核基址，
       后面所有节从这个虚拟地址开始排布 */
    . = KERNEL_BASE;

    /* 代码段：第一个放 .text.entry（内核入口），保证它在最前面 */
    .text : ALIGN(4K) {
        KEEP(*(.text.entry))   /* KEEP: 防止链接器以为没人用而把它删掉 */
        *(.text .text.*)       /* 其余所有代码 */
    }

    /* 只读数据：字符串常量等。
       .gcc_except_table 是异常处理表，也是只读的，一并收编 */
    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
        *(.gcc_except_table .gcc_except_table.*)
    }

    /* 已初始化的可写数据。
       注意 .got（全局偏移表）、.init_array 这些节脚本里若不点名，
       链接器会当"孤儿节"随手塞进相邻节后面、不做页对齐——
       曾导致可写 .got 与只读 .rodata 同页，Limine 拒绝加载。
       显式收编到可写区（且整节页对齐），杜绝此类问题 */
    .data : ALIGN(4K) {
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.*)
        *(.init_array .init_array.*)
        *(.fini_array .fini_array.*)
    }

    /* 未初始化数据（如我们的内核栈）：
       不占内核文件体积，Limine 加载时把这段内存清零 */
    .bss : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        __bss_end = .;
    }

    . = ALIGN(4K);
    KERNEL_END = .;            /* 内核结束地址，以后内存管理要用 */

    /* 丢弃用不到的节，减小体积 */
    /DISCARD/ : {
        *(.eh_frame*)
        *(.note*)
        *(.comment*)
    }
}
