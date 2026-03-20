# duvm-kmod: Thin kernel module for distributed UVM page interception
#
# Build: make
# Install: sudo insmod duvm-kmod.ko
# Remove: sudo rmmod duvm-kmod
# Clean: make clean

obj-m := duvm-kmod.o
duvm-kmod-objs := src/main.o src/ring.o src/swap.o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD := $(shell pwd)

# Extra compiler flags
ccflags-y := -I$(PWD)/include -Wall -Wextra -Werror

.PHONY: all clean

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
	rm -f Module.symvers modules.order
