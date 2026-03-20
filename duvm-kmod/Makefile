# duvm-kmod: Virtual block device for distributed swap
#
# Build: make
# Install: sudo insmod duvm-kmod.ko [size_mb=4096] [ring_entries=4096]
# Remove: sudo rmmod duvm-kmod
# Clean: make clean

obj-m := duvm-kmod.o
duvm-kmod-objs := src/main.o src/ring.o src/swap.o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD := $(shell pwd)

ccflags-y := -I$(PWD)/include -Wall -Wextra

.PHONY: all clean

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
	rm -f Module.symvers modules.order
