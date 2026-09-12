# MOS 构建脚本
# 用法：make check（只编译）| make iso（打包）| make run（QEMU 开机）

# ---------- 路径配置 ----------

KERNEL_ELF := kernel/target/x86_64-unknown-none/debug/mos-kernel
LIMINE_DIR := limine

# 内核源文件清单：make 靠时间戳决定要不要重编
KERNEL_SRC := $(shell find kernel/src -type f -name '*.rs')

# OVMF：UEFI 固件的模拟实现（CODE 只读剧本 + VARS 可写笔记本）
OVMF_CODE  := /usr/share/ovmf/x64/OVMF_CODE.4m.fd
OVMF_VARS  := OVMF_VARS.4m.fd

# ---------- 目标 ----------

# 只编译内核，不打包不开机（改代码后快速验证用）。
# 注意必须 cd 进 kernel 再编：cargo 只从"当前目录"向上找 .cargo/config.toml
check:
	cd kernel && cargo build

# 告诉 make：内核 ELF 由这些文件生成，谁比 ELF 新就重编
$(KERNEL_ELF): $(KERNEL_SRC) kernel/Cargo.toml kernel/.cargo/config.toml kernel/linker.ld
	cd kernel && cargo build

# 编译内核 + 装箱 + 写 BIOS 引导记录，产出 mos.iso
iso: $(KERNEL_ELF)
	mkdir -p iso_root/EFI/BOOT
	cp $(KERNEL_ELF) iso_root/mos-kernel
	cp limine.conf iso_root/
	cp $(LIMINE_DIR)/limine-bios.sys $(LIMINE_DIR)/limine-bios-cd.bin \
	   $(LIMINE_DIR)/limine-uefi-cd.bin iso_root/
	cp $(LIMINE_DIR)/BOOTX64.EFI iso_root/EFI/BOOT/
	xorriso -as mkisofs -R -r -J \
	    -b limine-bios-cd.bin -no-emul-boot -boot-load-size 4 -boot-info-table \
	    --efi-boot limine-uefi-cd.bin -efi-boot-part --efi-boot-image \
	    --protective-msdos-label \
	    iso_root -o mos.iso
	$(LIMINE_DIR)/limine bios-install mos.iso

# 开机！串口直连当前终端，Ctrl+A 再按 X 退出 QEMU
run: iso
	@test -f $(OVMF_VARS) || cp /usr/share/ovmf/x64/OVMF_VARS.4m.fd $(OVMF_VARS)
	qemu-system-x86_64 -M q35 -m 512M \
	    -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
	    -drive if=pflash,format=raw,file=$(OVMF_VARS) \
	    -cdrom mos.iso \
	    -serial stdio \
	    -no-reboot -no-shutdown

# 模拟真机复现（华硕天选5 Pro 同款三件套）：
#   16G 内存——位图 600KB+，内存布局形状和实机一致
#   2560×1600 屏——画布 16MB，验证明明是 QEMU 屏 4 倍大的画布逻辑。
#     分辨率靠 EDID：让 QEMU 显卡向固件广播"我支持 2560×1600"。
#     （试过 fw_cfg 的 opt/ovmf/X-Resolution——Arch 的 edk2-ovmf 202608
#      不认那个参数，分辨率原地不动，串口证据：heap up: 5E8 而非 11A0）
#   KVM 直跑真 CPU——页表缓存属性等硬件行为是真的。看图形窗口
run-real: iso
	@test -f $(OVMF_VARS) || cp /usr/share/ovmf/x64/OVMF_VARS.4m.fd $(OVMF_VARS)
	qemu-system-x86_64 -M q35 -m 16G -enable-kvm -cpu host \
	    -vga none -device VGA,edid=on,xres=2560,yres=1600 \
	    -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
	    -drive if=pflash,format=raw,file=$(OVMF_VARS) \
	    -cdrom mos.iso \
	    -serial stdio \
	    -no-reboot -no-shutdown

# 刻 U 盘：make burn DEV=/dev/sdX  （sdX 千万别写错！）
burn: iso
	sudo dd if=mos.iso of=$(DEV) bs=4M status=progress oflag=direct && sync

# 把 Limine 引导器装进仓库（一次性，见 README）
limine:
	wget https://github.com/limine-bootloader/limine/releases/download/v12.8.0/limine-binary.zip
	mkdir -p $(LIMINE_DIR)
	bsdtar -xf limine-binary.zip -C $(LIMINE_DIR) --strip-components=1
	$(MAKE) -C $(LIMINE_DIR)

clean:
	cd kernel && cargo clean
	rm -rf iso_root mos.iso

.PHONY: check iso run run-real burn limine clean
