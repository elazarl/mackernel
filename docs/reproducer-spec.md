# Reproducer bundle — spec

A **reproducer** is a single self-contained Markdown file describing a complete
kernel repro: an optional kernel source, userspace programs, kernel modules,
extra Kconfig, and a start script. It is run with:

```bash
./run-kernel.py repro.md
```

`run-kernel.py` builds everything, boots the kernel under QEMU against an Ubuntu
cloud image, runs the start script in the guest over SSH, streams its output, and
**exits with the guest script's status**. A reproducer is "reproduced" when that
exit status is non-zero (or whatever the repro defines as failure).

## File format

A bundle is normal Markdown. Two kinds of content are meaningful; everything else
is prose and ignored.

### 1. Metadata block (optional)

A `---`-delimited block, at column 0 and outside any code fence, holding
`key: value` lines. Recognized keys (others ignored):

| Key      | Meaning                                                              |
|----------|---------------------------------------------------------------------|
| `url`    | git remote to fetch the kernel from                                 |
| `commit` | commit / tag / branch to build (e.g. `v6.12`)                       |
| `patch`  | URL or path to a patch applied on top of `commit`                   |
| `arch`   | target arch (`x86_64` / `arm64`); overrides `ARCH` env and host     |

With no metadata block, the kernel at `LINUX_SRC` (default `~/linux`) is built
as-is. Arch precedence: frontmatter `arch:` > `ARCH` env > host arch.

```
---
url: git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v6.12
patch: https://example.com/fix.patch
arch: x86_64
---
```

### 2. Role-tagged code fences

Fenced blocks whose info string is `role:filename` contribute files to the build.
Multiple blocks per role are allowed.

| Role      | Purpose                                                        |
|-----------|----------------------------------------------------------------|
| `user`    | userspace source/headers, compiled to a guest binary           |
| `module`  | out-of-tree kernel module source, built against the kernel     |
| `kconf`   | extra Kconfig fragment, merged into the kernel config          |
| `init`    | start script run in the guest; its exit status is the result   |

````
```user:file.c
int main(void) { return 1; }
```

```module:greeter.c
/* ... module source ... */
```

```kconf:extra.config
CONFIG_DEBUG_INFO=y
```

```init:init.sh
#!/bin/bash
set -e
./file          # compiled userspace binary
```
````

## Execution model

1. Resolve kernel tree — `LINUX_SRC` as-is, or a cached git worktree at `commit`
   (fetching `url`) with `patch` applied.
2. Merge `kconf:` fragments and build the kernel + any `module:` sources.
3. Compile `user:` files into guest binaries.
4. Boot the kernel under QEMU (HVF/KVM/TCG) against the Ubuntu cloud image.
5. Copy the artifacts in and run `init:` over SSH, streaming output.
6. Exit with the `init:` script's status.

Guest network egress is restricted by default (override with `GUEST_NET=open`);
the QEMU process can be further confined with `MK_SANDBOX`.

See `examples/greeter.md` for a complete working bundle.
