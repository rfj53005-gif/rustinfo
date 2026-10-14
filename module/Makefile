obj-m += rustinfo_smu.o

KDIR ?= /lib/modules/$(shell uname -r)/build

# Match the target kernel's compiler, including non-running kernels built by DKMS.
# CachyOS kernels use Clang/LTO; plain GCC produces an incompatible module.
-include $(KDIR)/include/config/auto.conf
ifneq ($(filter y,$(CONFIG_CC_IS_CLANG) $(CONFIG_LTO_CLANG)),)
LLVM ?= 1
endif

all:
	$(MAKE) -C "$(KDIR)" M="$(CURDIR)" $(if $(LLVM),LLVM=$(LLVM)) modules

clean:
	$(MAKE) -C "$(KDIR)" M="$(CURDIR)" $(if $(LLVM),LLVM=$(LLVM)) clean
