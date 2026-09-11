# MOS 构建脚本
# 用法：make check（只编译）| make iso（打包）| make run（QEMU 开机）

# ---------- 路径配置 ----------

KERNEL_ELF := kernel/target/x86_64-unknown-none/debug/mos-kernel
LIMINE_DIR := limine

# OVMF：UEFI 固件的模拟实现（CODE 只读剧本 + VARS 可写笔记本）
OVMF_CODE  := /usr/share/ovmf/x64/OVMF_CODE.4m.fd
OVMF_VARS  := OVMF_VARS.4m.fd

# ---------- 目标 ----------

# 只编译内核，不打包不开机（改代码后快速验证用）
check:
	$(MAKE) -C kernel

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
	$(MAKE) -C kernel clean
	rm -rf iso_root mos.iso

.PHONY: check iso run burn limine clean
