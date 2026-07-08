---
summary: Minimal userspace + module + init demo bundle
tag: demo
---

# greeter — example mackernel bundle

A self-contained repro you can run with:

```bash
./run-kernel.py examples/greeter.md
```

It builds a tiny userspace program and a kernel module, boots the kernel, loads
the module, and runs the start script in the guest. The program exits with status
`R` (1 here), which becomes `run-kernel.py`'s exit status.

This bundle has no metadata block, so it builds the kernel at `LINUX_SRC`
(`~/linux`). To pin a kernel instead, add a block anywhere in the file:

    ---
    url: git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
    commit: v6.12
    patch: https://example.com/some.patch
    compiler: 14
    ---

`compiler:` picks the gcc version of the build container (13, 14, or 15;
default 15). Use 13/14 for older kernels — gcc-15 defaults to C23 and fails to
compile pre-~6.7 trees' realmode/boot code ("cannot use keyword 'false'").

Extra Kconfig can be supplied with a `kconf:` block (merged into the build).

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
MODULE_DESCRIPTION("mackernel bundle example module");
```

```init:init.sh
#!/bin/bash
set -e
echo "loaded modules:"; lsmod | grep greeter || true
echo "kernel says:"; sudo dmesg | grep greeter || true
./file
```
