# Repository Guidelines

## Project Structure & Module Organization

MOS is an operating-system project written in C, with assembly for architecture-specific code. Use:

- `boot/` for UEFI boot integration, Limine configuration, and early entry code
- `kernel/arch/x86_64/` for startup, descriptors, interrupts, context switching, and assembly
- `kernel/mm/` for frame allocation, paging, virtual memory, and heap
- `kernel/sched/` for scheduler and process management
- `kernel/drivers/` for timer, serial, keyboard, and block devices
- `kernel/fs/` for VFS and file systems
- `kernel/syscall/` for user-kernel interfaces
- `userland/` for init, shell, and utilities
- `tools/` for linker and build scripts
- `docs/` and `tests/` for design notes and tests

Keep machine-dependent code under `kernel/arch/`; keep other kernel modules architecture-neutral where practical.

## Build, Test, and Development Commands

Use GNU Make and document these commands in the root `Makefile`:

```sh
make build   # Build the kernel image or bootable ISO
make run     # Boot the UEFI image in QEMU/OVMF with serial output
make debug   # Launch QEMU paused with a GDB connection
make test    # Run all tests
make clean   # Remove generated build outputs
```

The default architecture is `x86_64`.

## Toolchain & Environment

Use a freestanding `x86_64-elf` GCC/binutils cross toolchain, NASM, GNU Make, QEMU with OVMF, GDB, and `xorriso`/`mtools` for UEFI image creation. Use Limine as the default UEFI bootloader unless the project later switches to a custom EDK2 entry point. Record versions and install steps in `docs/toolchain.md`. Use strict freestanding flags such as `-Wall -Wextra -ffreestanding -fno-stack-protector -nostdlib`.

## Coding Style & Naming Conventions

- Write kernel code in C17 with four-space indentation and a 100-column limit.
- Use assembly only where C cannot express required CPU operations.
- Use `snake_case` for functions and variables, and `UPPER_SNAKE_CASE` for constants and macros.
- Prefer fixed-width types such as `uint32_t` and `uintptr_t` in hardware and memory code.
- Do not call host libc; implement needed routines in `kernel/lib/`.

## Testing Guidelines

Every build must boot in QEMU. Unit-test pure logic like data structures and memory bookkeeping. Boot-test startup, serial output, interrupts, and basic system calls. Name tests after modules, such as `tests/mm/frame_allocator_test.c`.

## Commit & Pull Request Guidelines

No commit convention is established yet; use concise imperative subjects such as `Add physical frame allocator`. Pull requests should describe the design, test results, architecture assumptions, and a QEMU boot log for boot-path changes.

## Agent-Specific Instructions

Never assume hosted libc, dynamic linking, or an existing filesystem. Keep changes minimal and verify kernel or boot changes by building and booting in QEMU when possible.
