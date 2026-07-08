# greeter (aarch64) — example mackernel bundle

The same repro as [`greeter.md`](greeter.md), pinned to **arm64**. Run it with:

```bash
./run-kernel.py examples/greeter-aarch64.md
```

The only difference from `greeter.md` is the `arch: arm64` metadata block (active,
at column 0, just below). On an x86_64 host the kernel, module, and userspace
binary are **cross-compiled** with `aarch64-linux-gnu-gcc` (native speed); only the
QEMU boot is emulated (TCG). On an arm64 host it all builds and boots natively.

---
arch: arm64
summary: The greeter demo, pinned to arm64
tag: demo
tag: arm64
---

It builds a tiny userspace program and a kernel module, boots the kernel, loads
the module, and runs the start script in the guest. The program exits with status
`R` (1 here), which becomes `run-kernel.py`'s exit status.

```user:file.c
#include <stdio.h>
#include "file.h"

int main(void) {
    printf("hello from userspace, returning %d\n", R);
    return R;
}
```

```user:file.h
#define R 1
```

```module:greeter.c
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>

static int __init greeter_init(void)
{
    pr_info("greeter: module loaded\n");
    return 0;
}

static void __exit greeter_exit(void)
{
    pr_info("greeter: module unloaded\n");
}

module_init(greeter_init);
module_exit(greeter_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("mackernel bundle example module (arm64)");
```

```init:init.sh
#!/bin/bash
set -e
echo "loaded modules:"; lsmod | grep greeter || true
echo "kernel says:"; sudo dmesg | grep greeter || true
./file
```
