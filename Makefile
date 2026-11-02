# Kern build. Needs: Rust (with the rust-src component), xorriso, git, make, a C compiler.
# Optional: qemu-system-x86_64 and OVMF to test without real hardware.

LIMINE_VERSION := v11.4.1-binary
PROGRAMS       := init hello spin crash selftest
ISO            := kern.iso
TEST_ISO       := kern-test.iso
# Boot entry tweaks for the test ISO: no menu delay, selftest as pid 1, and
# QEMU exits with its result.
TEST_CMDLINE   := init=selftest qemu-exit

# Lets a stable toolchain use -Z build-std (core/alloc for a bare-metal target).
export RUSTC_BOOTSTRAP := 1

KERNEL   := kernel/target/x86_64-unknown-none/release/kern
USER_DIR := user/target/x86_64-kern-user/release
OVMF     ?= $(firstword $(wildcard /usr/share/ovmf/OVMF.fd /usr/share/OVMF/OVMF_CODE.fd /usr/share/edk2/x64/OVMF.4m.fd /usr/share/OVMF/OVMF_CODE_4M.fd))

.PHONY: all kernel user iso run run-uefi test clean distclean
all: $(ISO)

kernel:
	cd kernel && cargo build --release

user:
	cd user && cargo build --release

limine/limine:
	rm -rf limine
	git clone --depth 1 --branch $(LIMINE_VERSION) https://github.com/limine-bootloader/limine.git limine
	$(MAKE) -C limine

# $(call make_iso,OUTPUT,COMMAND): stage everything in iso_root, run COMMAND
# (to adjust the staged limine.conf), then write a hybrid BIOS/UEFI ISO.
define make_iso
	rm -rf iso_root
	mkdir -p iso_root/boot/limine iso_root/boot/bin iso_root/EFI/BOOT
	cp $(KERNEL) iso_root/boot/kernel
	for p in $(PROGRAMS); do cp $(USER_DIR)/$$p iso_root/boot/bin/$$p.elf; done
	cp limine.conf limine/limine-bios.sys limine/limine-bios-cd.bin limine/limine-uefi-cd.bin iso_root/boot/limine/
	cp limine/BOOTX64.EFI limine/BOOTIA32.EFI iso_root/EFI/BOOT/
	$(2)
	xorriso -as mkisofs -R -r -J -b boot/limine/limine-bios-cd.bin \
		-no-emul-boot -boot-load-size 4 -boot-info-table -hfsplus \
		-apm-block-size 2048 --efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		iso_root -o $(1) 2>/dev/null
	./limine/limine bios-install $(1) >/dev/null 2>&1
endef

$(ISO): kernel user limine/limine limine.conf
	$(call make_iso,$(ISO),true)
	@echo "Built $(ISO). Write it to a USB stick (dd / Rufus in DD mode) to boot real hardware."

$(TEST_ISO): kernel user limine/limine limine.conf
	$(call make_iso,$(TEST_ISO),sed -i -e 's/^timeout:.*/timeout: 0/' \
		-e 's|^\(    path: .*\)|\1\n    cmdline: $(TEST_CMDLINE)|' iso_root/boot/limine/limine.conf)

# Boots both ISOs in QEMU; see tests/run.py.
test: $(ISO) $(TEST_ISO)
	python3 tests/run.py $(ISO) $(TEST_ISO)

# Legacy BIOS boot in QEMU
run: $(ISO)
	qemu-system-x86_64 -M q35 -m 512M -cdrom $(ISO) -serial stdio

# UEFI boot in QEMU (closer to modern laptops)
run-uefi: $(ISO)
	qemu-system-x86_64 -M q35 -m 512M -bios $(OVMF) -cdrom $(ISO) -serial stdio

clean:
	rm -rf iso_root $(ISO) $(TEST_ISO)
	cd kernel && cargo clean
	cd user && cargo clean

distclean: clean
	rm -rf limine
