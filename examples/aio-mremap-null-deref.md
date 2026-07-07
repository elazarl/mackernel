---
url: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v6.2-rc2
patch-compare: true
arch: x86_64
search-dmesg: aio_ring_mremap
search-dmesg: kernel NULL pointer dereference
---

# aio: NULL-pointer deref in `aio_ring_mremap()` after `fork()`

Reproducer for commit **81e9d6f86476** ("aio: fix mremap after fork
null-deref"), which fixes a kernel NULL-pointer dereference that has been
present since e4a0d3e720e7 ("aio: Make it possible to remap aio ring").

## The bug

`aio_ring_mremap()` (`fs/aio.c`) is the `vm_ops->mremap` handler for the
per-context aio ring mapping that `io_setup(2)` creates. It does:

```c
table = rcu_dereference(mm->ioctx_table);
for (i = 0; i < table->nr; i++) { ... }
```

Aio contexts are **not** inherited across `fork()`: `dup_mmap()` /
`mm_init_aio()` (`kernel/fork.c`) leaves the child's `mm->ioctx_table`
NULL. The `[aio]` VMA *is* inherited (it is `MAP_SHARED` and lacks
`VM_DONTCOPY`), so it keeps its `aio_ring_vm_ops` in the child. A child
that `mremap()`s the inherited `[aio]` mapping therefore drives
`aio_ring_mremap()` to dereference the NULL `ioctx_table`
(`table->nr` at offset 0x10) -> kernel oops.

## The fix

The patch adds `if (!table) goto out_unlock;` so `aio_ring_mremap()`
returns `-EINVAL` instead of dereferencing NULL. `move_vma()` then unwinds
the move and `mremap()` returns `MAP_FAILED` (`errno=EINVAL`); the child
continues normally.

## How the reproducer triggers it

1. `io_setup(1, &ctx)` creates the ring; the ring's base address and size
   are read back from the `[aio]` line of `/proc/self/maps`.
2. `fork()`.
3. In the child (whose `mm->ioctx_table` is NULL), call
   `mremap(base, size, size, MREMAP_FIXED | MREMAP_MAYMOVE, slot)` — a pure
   **move** (no resize). `new_len == old_len` is deliberate: in
   `vma_to_resize()` that case returns the vma early, skipping the
   `VM_DONTEXPAND` grow check that would otherwise reject the aio ring, so
   execution reaches `move_vma()` -> `vma->vm_ops->mremap()` =
   `aio_ring_mremap()`.

## Why the parent can't just `waitpid()` (the baseline trap)

On the unfixed kernel the child's `mremap()` oopses. The oops path
(`oops_end` -> `rewind_stack_and_make_dead` -> `do_exit` -> `exit_mm` ->
`__mmput` -> `exit_mmap`) calls `mmap_read_lock(mm)` (`mm/mmap.c`), but
the child **still holds the `mmap` write lock** acquired by the `mremap`
syscall. The rwsem is non-recursive, so `down_read` blocks on a lock the
same task already holds — a self-deadlock. The child parks in
`TASK_UNINTERRUPTIBLE` inside `exit_mmap()` and is never reaped, so a
plain `waitpid()` blocks **forever** and the runner eventually kills the
guest with the `timeout` exit code **124** (exactly what the previous
bundle produced).

The reproducer therefore wraps `waitpid()` in a `SIGALRM` watchdog: if
the child has not been reaped within a few seconds, the oops+deadlock is
inferred and the run reports "bug reproduced" with a clean non-zero exit.
The watchdog's only purpose is to *detect* the stuck child; it does not
affect the patched path, where `mremap()` returns `MAP_FAILED` and the
child exits in milliseconds.

Additionally, the child redirects its stdio to `/dev/null` before the
faulting `mremap()`. A child stuck in the kernel keeps its inherited file
descriptors open; without this detach it would hold the SSH output pipe,
so the host side would never see EOF and would time out waiting for the
channel to close — even after the parent had already exited with the
right status. Detaching the child's stdio lets the parent's exit close
the channel promptly.

## Expected result (`patch-compare`)

| variant  | child fate                                              | repro exit | meaning        |
|----------|---------------------------------------------------------|------------|----------------|
| baseline | oops -> `exit_mmap` mmap_lock self-deadlock (stuck)     | 1          | bug reproduced |
| patched  | `mremap` returns `MAP_FAILED` (`-EINVAL`) -> child exit 0 | 0        | bug fixed      |

`panic_on_oops` is `0` by default on `v6.2-rc2`
(`CONFIG_PANIC_ON_OOPS_VALUE`), so the baseline oops only wedges the
child — it does not panic the guest, and the parent's watchdog can still
report the result. The init script still *attempts* to force
`panic_on_oops=0` best-effort, but `/proc/sys` is read-only inside the
runner's guest, so the write is wrapped so its failure is silent (this
fixes the `./init.sh: line 3: /proc/sys/kernel/panic_on_oops: Permission
denied` noise from the previous bundle). The oops backtrace
(`BUG: kernel NULL pointer dereference, address: 0x10` at
`aio_ring_mremap+...`) is additionally surfaced via `search-dmesg`.

```patch:aio-null-deref-fix.patch
diff --git a/fs/aio.c b/fs/aio.c
index 562916d85c..e85ba0b77f 100644
--- a/fs/aio.c
+++ b/fs/aio.c
@@ -361,6 +361,9 @@ static int aio_ring_mremap(struct vm_area_struct *vma)
 	spin_lock(&mm->ioctx_lock);
 	rcu_read_lock();
 	table = rcu_dereference(mm->ioctx_table);
+	if (!table)
+		goto out_unlock;
+
 	for (i = 0; i < table->nr; i++) {
 		struct kioctx *ctx;
 
@@ -374,6 +377,7 @@ static int aio_ring_mremap(struct vm_area_struct *vma)
 		}
 	}
 
+out_unlock:
 	rcu_read_unlock();
 	spin_unlock(&mm->ioctx_lock);
 	return res;
```

```kconf:extra.config
CONFIG_AIO=y
```

```user:aio_mremap_repro.c
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>

typedef unsigned long aio_context_t;

static int io_setup(unsigned nr, aio_context_t *ctxp)
{
	return (int)syscall(SYS_io_setup, nr, ctxp);
}

static int find_aio_mapping(unsigned long *base, unsigned long *size)
{
	FILE *f = fopen("/proc/self/maps", "r");
	char line[512];

	if (!f)
		return -1;
	while (fgets(line, sizeof(line), f)) {
		unsigned long start, end;

		if (!strstr(line, "[aio]"))
			continue;
		if (sscanf(line, "%lx-%lx", &start, &end) == 2) {
			*base = start;
			*size = end - start;
			fclose(f);
			return 0;
		}
	}
	fclose(f);
	return -1;
}

static volatile sig_atomic_t alarmed;

static void alarm_handler(int sig)
{
	(void)sig;
	alarmed = 1;
}

int main(void)
{
	aio_context_t ctx = 0;
	unsigned long base, size;
	pid_t pid;
	int status;
	struct sigaction sa;

	if (io_setup(1, &ctx) < 0) {
		perror("io_setup");
		return 2;
	}
	if (find_aio_mapping(&base, &size) < 0) {
		fprintf(stderr, "could not find [aio] mapping in /proc/self/maps\n");
		return 2;
	}
	printf("aio ring: base=0x%lx size=%lu ioctx=0x%lx\n", base, size, ctx);
	fflush(stdout);

	pid = fork();
	if (pid < 0) {
		perror("fork");
		return 2;
	}
	if (pid == 0) {
		int devnull = open("/dev/null", O_RDWR);
		void *slot;
		void *ret;

		if (devnull >= 0) {
			dup2(devnull, STDIN_FILENO);
			dup2(devnull, STDOUT_FILENO);
			dup2(devnull, STDERR_FILENO);
			close(devnull);
		}
		slot = mmap(NULL, size, PROT_NONE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		if (slot == MAP_FAILED)
			_exit(4);
		ret = mremap((void *)base, size, size,
			     MREMAP_FIXED | MREMAP_MAYMOVE, slot);
		if (ret == MAP_FAILED)
			_exit(0);
		_exit(0);
	}

	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = alarm_handler;
	sigemptyset(&sa.sa_mask);
	sa.sa_flags = 0;
	sigaction(SIGALRM, &sa, NULL);

	alarm(5);
	pid_t r = waitpid(pid, &status, 0);
	int timed_out = alarmed;
	alarm(0);

	if (r < 0) {
		if (timed_out) {
			printf("BUG REPRODUCED: child stuck in kernel after oops "
			       "(NULL ioctx_table deref in aio_ring_mremap -> "
			       "exit_mmap mmap_lock self-deadlock)\n");
			return 1;
		}
		perror("waitpid");
		return 2;
	}

	if (WIFSIGNALED(status)) {
		printf("BUG REPRODUCED: child killed by signal %d "
		       "(kernel oops: NULL ioctx_table deref in aio_ring_mremap)\n",
		       WTERMSIG(status));
		return 1;
	}
	if (WIFEXITED(status)) {
		int ec = WEXITSTATUS(status);

		if (ec == 0) {
			printf("OK: mremap returned failure gracefully - bug is fixed\n");
			return 0;
		}
		printf("child exited with unexpected status %d\n", ec);
		return 3;
	}
	printf("unexpected child status 0x%x\n", status);
	return 3;
}
```

```init:init.sh
#!/bin/bash
set -e
{ echo 0 > /proc/sys/kernel/panic_on_oops; } 2>/dev/null || true
cd "$(dirname "$0")"
./aio_mremap_repro
```
