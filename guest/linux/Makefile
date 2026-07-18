# Build the infinigpu guest kernel module (out-of-tree).
#
#   make                 # build against the running kernel's headers
#   make KDIR=/path/...   # build against a specific kernel build tree
#   make clean
#
# The module loads in any guest running the SAME kernel version it was built for.
obj-m := infinigpu.o

# The render-node uAPI header (drm_infinigpu_submit3d + ioctl numbers) lives with the other
# shared headers in guest/include and is included by both the driver and userspace.
ccflags-y += -I$(src)/../include

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
