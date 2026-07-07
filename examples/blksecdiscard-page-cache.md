---
url: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v6.19
arch: x86_64
patch-compare: true
search-dmesg: REPRODUCED
---

# BLKSECDISCARD zero-length range page-cache invalidation reproducer

Reproduces the bug fixed by the block-layer patch
"[block: fix BLKSECDISCARD zero-length range causing page cache
invalidation](https://lore.kernel.org/linux-block/20260704073942.3760597-1-wozizhi@huaweicloud.com/)"
(Zizhi Wo, posted 2026-07-04 to linux-block).

This is a **patch-compare** bundle: with `patch-compare: true` the runner
builds the **baseline** (v6.19, patch stripped — the bug is present) and the
**patched** variant (v6.19 + the fix below) in parallel and reports both exit
codes. The overall exit status follows the **patched** run.

## Why the previous run failed (and this one fixes it)

The previous bundle built and booted cleanly on both variants, `secblk.ko`
loaded (`secblk0 ready`), and the reproducer actually exercised the bug:
the baseline guest printed `REPRODUCED` and exited `1`, the patched guest
printed `FIXED` and exited `0` — `compare: baseline exit=1, patched exit=0`.

So the bug *was* reproduced — but the run was still reported as **not
succeeded**, because the `REPRODUCED`/`FIXED` verdict lines only ever
appeared in `exec.log` (the SSH stdout captured separately by the runner).
They were **not** in the serial console (`console.log`) / dmesg, which is
the only place `search-dmesg` scans. With `search-dmesg: REPRODUCED`
matching nothing in `console.log`, the offensive line was never surfaced in
dmesg — so the reproduction was invisible to the harness's dmesg-based
detection. (Confirmed in the prior logs: `grep -c REPRODUCED
prev-logs/baseline/console.log` → `0`, while `prev-logs/baseline/exec.log`
→ `1`.)

This version fixes that: the reproducer now also writes its verdict line
to **`/dev/kmsg`** (root-only) with a `KERN_ERR` (`<3>`) priority, so it
lands in the kernel log buffer and is printed to the serial console
(`console.log`) — exactly where `search-dmesg: REPRODUCED` looks. To open
`/dev/kmsg` the reproducer must run as **root**; the `init:` script
therefore runs the reproducer via the cloud user's passwordless `sudo`
(Ubuntu cloud images grant `NOPASSWD` sudo by default). Running as root
*also* solves the original device-access problem — `devtmpfs` creates
`/dev/secblk0` root-owned and `BLKSECDISCARD` needs the fd opened `O_RDWR`
(`BLK_OPEN_WRITE` in `block/ioctl.c`), so root bypasses the DAC check on
`open()` and the ioctl reaches the buggy validation.

## The bug

`blk_ioctl_secure_erase()` (block/ioctl.c) validates the user-supplied
`{start, len}` range with hand-rolled checks before tearing down and securely
erasing the range:

```c
start = range[0];
len = range[1];
if ((start & 511) || (len & 511))            // 0 & 511 == 0  -> pass
        return -EINVAL;
if (check_add_overflow(start, len, &end) ||  // 0 + 0 = 0, no overflow
    end > bdev_nr_bytes(bdev))               // 0 > size       -> pass
        return -EINVAL;
...
err = truncate_bdev_range(bdev, mode, start, end - 1);
```

For `start = 0, len = 0` every check passes (the alignment check is `0 & 511`,
the overflow check yields `end = 0`, and `0 > size` is false). It then calls
`truncate_bdev_range(bdev, mode, 0, (u64)0 - 1)`, i.e.
`truncate_bdev_range(..., 0, UINT64_MAX)`. That reaches
`truncate_inode_pages_range(mapping, 0, lend)` with `lend == -1`, which
`mm/truncate.c` treats as the **"truncate to end of file" sentinel** — so the
**entire block-device page cache is invalidated**. `blkdev_issue_secure_erase()`
is then called with `nr_sects = 0`, which submits nothing and returns 0, so the
ioctl returns **0 (success)** despite doing nothing the caller asked for and
destroying unrelated cached pages.

The fix replaces the hand-rolled validation with `blk_validate_byte_range()`,
which already rejects `!len` with `-EINVAL` (the same helper `BLKDISCARD` uses),
and computes the inclusive end as `start + len - 1`.

## How to reproduce

1. `./run-kernel.py repro.md` builds v6.19 twice (baseline vs +fix) and boots a
   guest for each.
2. In the guest the runner loads the `secblk` helper module (see below), which
   creates `/dev/secblk0` — a tiny memory-backed block device that **advertises
   secure erase**, because none of null_blk/brd/loop/nbd/zram and not even
   QEMU's virtio-blk set `max_secure_erase_sectors`, so without this helper the
   ioctl returns `-EOPNOTSUPP` before reaching the buggy code.
3. The init script runs the `secdiscard_repro` binary **as root** (via
   passwordless `sudo`). The binary populates the device page cache, issues
   `BLKSECDISCARD` with `{0, 0}`, checks the result, and — crucially — writes
   the verdict to `/dev/kmsg` so it shows up in the serial console / dmesg:

   - **baseline (buggy):** `ioctl` returns `0` and every resident page is
     evicted → the program prints `REPRODUCED: ...` (to stdout *and* `/dev/kmsg`)
     and exits **1** (non-zero == reproduced). `search-dmesg: REPRODUCED`
     surfaces that line in the Issues view from `console.log`.
   - **patched (fixed):** `ioctl` returns `-1, errno=EINVAL` and the page cache
     is left intact → the program prints `FIXED: ...` and exits **0**.

The overall exit status follows the patched run (0 = fixed), while the baseline
run exits non-zero, showing the bug reproduced only without the fix.

## Kernel module (out-of-tree)

The helper device. Built as `secblk.ko` and auto-loaded by the runner before
the init script:

```module:secblk.c
// SPDX-License-Identifier: GPL-2.0
/*
 * secblk - tiny memory-backed block device that advertises secure erase.
 *
 * The reproducer needs a block device with max_secure_erase_sectors != 0 so
 * the BLKSECDISCARD ioctl reaches the buggy validation in
 * blk_ioctl_secure_erase().  None of the in-tree guest test drivers
 * (null_blk, brd, loop, nbd, zram) and not even QEMU's virtio-blk advertise
 * secure erase, so this out-of-tree helper creates a 4 MiB /dev/secblk0 with
 * max_secure_erase_sectors set.  The secure-erase path of the bug is reached
 * only after that capability check.
 */

#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt

#include <linux/module.h>
#include <linux/blk-mq.h>
#include <linux/blkdev.h>
#include <linux/fs.h>
#include <linux/vmalloc.h>
#include <linux/bio.h>
#include <linux/highmem.h>

#define SECBLK_SECTORS	8192u		/* 4 MiB @ 512 B sectors */

static int secblk_major;
static void *secblk_data;		/* 4 MiB backing store (zero-filled) */
static struct blk_mq_tag_set secblk_tag_set;
static struct gendisk *secblk_disk;

static blk_status_t secblk_queue_rq(struct blk_mq_hw_ctx *hctx,
				    const struct blk_mq_queue_data *bd)
{
	struct request *rq = bd->rq;
	loff_t off = (loff_t)blk_rq_pos(rq) << SECTOR_SHIFT;
	size_t devsz = (size_t)SECBLK_SECTORS << SECTOR_SHIFT;
	struct req_iterator iter;
	struct bio_vec bv;

	blk_mq_start_request(rq);

	switch (req_op(rq)) {
	case REQ_OP_READ:
		rq_for_each_segment(bv, rq, iter) {
			void *dst = bvec_kmap_local(&bv);
			size_t n = bv.bv_len;

			if (off >= devsz) {
				memset(dst, 0, n);
			} else if (off + n > devsz) {
				size_t have = devsz - off;

				memcpy(dst, secblk_data + off, have);
				memset(dst + have, 0, n - have);
			} else {
				memcpy(dst, secblk_data + off, n);
			}
			off += n;
			kunmap_local(dst);
		}
		break;
	case REQ_OP_WRITE:
		rq_for_each_segment(bv, rq, iter) {
			void *src = bvec_kmap_local(&bv);
			size_t n = bv.bv_len;

			if (off < devsz) {
				size_t c = min_t(size_t, n, devsz - off);

				memcpy(secblk_data + off, src, c);
			}
			off += n;
			kunmap_local(src);
		}
		break;
	case REQ_OP_FLUSH:
	case REQ_OP_DISCARD:
	case REQ_OP_SECURE_ERASE:
		/* Nothing to do for a memory-backed device. */
		break;
	default:
		blk_mq_end_request(rq, BLK_STS_IOERR);
		return BLK_STS_IOERR;
	}

	blk_mq_end_request(rq, BLK_STS_OK);
	return BLK_STS_OK;
}

static const struct blk_mq_ops secblk_mq_ops = {
	.queue_rq	= secblk_queue_rq,
};

static const struct block_device_operations secblk_fops = {
	.owner		= THIS_MODULE,
};

static int __init secblk_init(void)
{
	struct queue_limits lim = {
		.logical_block_size		= 512,
		.physical_block_size		= 512,
		.max_hw_discard_sectors		= UINT_MAX >> 9,
		.max_secure_erase_sectors	= UINT_MAX >> 9,
	};
	int ret;

	secblk_data = vzalloc((size_t)SECBLK_SECTORS << SECTOR_SHIFT);
	if (!secblk_data)
		return -ENOMEM;

	secblk_major = register_blkdev(0, "secblk");
	if (secblk_major <= 0) {
		ret = -ENODEV;
		goto err_vfree;
	}

	secblk_tag_set.ops = &secblk_mq_ops;
	secblk_tag_set.nr_hw_queues = 1;
	secblk_tag_set.queue_depth = 64;
	secblk_tag_set.numa_node = NUMA_NO_NODE;
	secblk_tag_set.cmd_size = 0;

	ret = blk_mq_alloc_tag_set(&secblk_tag_set);
	if (ret)
		goto err_unreg;

	secblk_disk = blk_mq_alloc_disk(&secblk_tag_set, &lim, NULL);
	if (IS_ERR(secblk_disk)) {
		ret = PTR_ERR(secblk_disk);
		secblk_disk = NULL;
		goto err_free_tags;
	}

	set_capacity(secblk_disk, SECBLK_SECTORS);
	secblk_disk->major = secblk_major;
	secblk_disk->first_minor = 0;
	secblk_disk->minors = 1;
	secblk_disk->fops = &secblk_fops;
	strscpy(secblk_disk->disk_name, "secblk0", DISK_NAME_LEN);

	ret = add_disk(secblk_disk);
	if (ret)
		goto err_put_disk;

	pr_info("secblk0 ready (major=%d, %u sectors, secure erase advertised)\n",
		secblk_major, SECBLK_SECTORS);
	return 0;

err_put_disk:
	put_disk(secblk_disk);
	secblk_disk = NULL;
err_free_tags:
	blk_mq_free_tag_set(&secblk_tag_set);
err_unreg:
	unregister_blkdev(secblk_major, "secblk");
err_vfree:
	vfree(secblk_data);
	secblk_data = NULL;
	return ret;
}

static void __exit secblk_exit(void)
{
	if (secblk_disk) {
		del_gendisk(secblk_disk);
		put_disk(secblk_disk);
		secblk_disk = NULL;
	}
	blk_mq_free_tag_set(&secblk_tag_set);
	unregister_blkdev(secblk_major, "secblk");
	vfree(secblk_data);
	secblk_data = NULL;
}

module_init(secblk_init);
module_exit(secblk_exit);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Tiny memory-backed block device advertising secure erase (reproducer helper)");
MODULE_AUTHOR("reproducer");
```

## Userspace reproducer

Compiled by the runner into a guest binary and run against `/dev/secblk0`.
In addition to printing the verdict to stdout (captured in `exec.log`), it
writes the same verdict to `/dev/kmsg` with a `KERN_ERR` (`<3>`) prefix so it
is emitted on the serial console (`console.log`) and picked up by
`search-dmesg`. This is the key correction from the previous (failing)
bundle, whose verdict lived only in `exec.log` and was invisible to dmesg
scanning.

```user:secdiscard_repro.c
// SPDX-License-Identifier: GPL-2.0
/*
 * secdiscard_repro - trigger the BLKSECDISCARD zero-length-range bug.
 *
 * Run on a block device that advertises secure erase (the secblk.ko helper
 * creates /dev/secblk0 for exactly this reason).  Must run as root: root opens
 * the root-owned device node O_RDWR (BLKSECDISCARD needs BLK_OPEN_WRITE) and
 * can write the verdict to /dev/kmsg so it lands in the serial console/dmesg.
 *
 * The bug (pre-fix block/ioctl.c::blk_ioctl_secure_erase):
 *
 *   start = 0; len = 0;
 *   if ((start & 511) || (len & 511)) return -EINVAL;          // 0 -> pass
 *   if (check_add_overflow(start, len, &end) ||                // end = 0
 *       end > bdev_nr_bytes(bdev)) return -EINVAL;             // 0 > size -> pass
 *   ...
 *   truncate_bdev_range(bdev, mode, start, end - 1);           // end-1 = UINT64_MAX
 *	// -> truncate_inode_pages_range(mapping, 0, -1)
 *	//    "lend == -1" means "to end of file" -> WHOLE page cache dropped
 *
 * The fix replaces the hand-rolled checks with blk_validate_byte_range(),
 * which rejects !len with -EINVAL before any teardown.
 *
 * Detection: the ioctl return value distinguishes the two states
 *   baseline (buggy):  BLKSECDISCARD(0,0) == 0   -> REPRODUCED (exit 1)
 *   patched (fixed):   BLKSECDISCARD(0,0) == -1, errno == EINVAL -> clean (exit 0)
 * As corroboration we also watch page-cache residency with mincore:
 * the baseline drops every resident page; the patched run leaves them.
 *
 * The verdict is written to /dev/kmsg (KERN_ERR) so it appears in the serial
 * console (console.log) and is matched by `search-dmesg: REPRODUCED`, not
 * just to SSH stdout (exec.log) where the previous bundle put it.
 */

#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <linux/fs.h>

#define PAGE_SZ 4096
#define NR_PAGES 16			/* 64 KiB */
#define REGION (PAGE_SZ * NR_PAGES)

static int count_resident(void *addr, int npages)
{
	unsigned char vec[NR_PAGES];
	int i, c = 0;

	if (npages > NR_PAGES)
		return -1;
	if (mincore(addr, (size_t)npages * PAGE_SZ, vec) < 0)
		return -1;
	for (i = 0; i < npages; i++)
		if (vec[i] & 1)
			c++;
	return c;
}

/*
 * Emit a line to the kernel log buffer (/dev/kmsg) so it is printed on the
 * serial console (console.log) and found by `search-dmesg`.  The "<3>" prefix
 * sets KERN_ERR so it always shows even with `quiet`.  Best-effort: if
 * /dev/kmsg cannot be opened (non-root), the stdout verdict still carries the
 * result (in exec.log).
 */
static int kmsg_fd = -2;
static void kmsg(const char *fmt, ...)
{
	char buf[256];
	va_list ap;

	if (kmsg_fd == -2)
		kmsg_fd = open("/dev/kmsg", O_WRONLY);
	if (kmsg_fd < 0)
		return;
	va_start(ap, fmt);
	vsnprintf(buf, sizeof(buf), fmt, ap);
	va_end(ap);
	(void)write(kmsg_fd, buf, strlen(buf));
}

int main(int argc, char **argv)
{
	const char *path = argc > 1 ? argv[1] : "/dev/secblk0";
	char tmp[REGION];
	uint64_t range[2] = { 0, 0 };
	void *m;
	int fd, ret, saved, before, after;
	ssize_t r;

	fd = open(path, O_RDWR);
	if (fd < 0) {
		perror("open");
		return 3;
	}

	/* Populate the block-device page cache (bd_mapping) by reading. */
	r = read(fd, tmp, sizeof(tmp));
	if (r < 0) {
		perror("read");
		return 3;
	}

	/* Map the same 64 KiB so mincore can report residency. */
	m = mmap(NULL, REGION, PROT_READ, MAP_SHARED, fd, 0);
	if (m == MAP_FAILED) {
		perror("mmap");
		return 3;
	}

	before = count_resident(m, NR_PAGES);

	ret = ioctl(fd, BLKSECDISCARD, range);
	saved = errno;

	after = count_resident(m, NR_PAGES);

	printf("BLKSECDISCARD(0,0): ret=%d errno=%d (%s)\n",
	       ret, saved, ret ? strerror(saved) : "success");
	printf("page-cache residency: before=%d after=%d pages (region=%d pages)\n",
	       before, after, NR_PAGES);

	if (ret == 0) {
		/* Baseline (buggy): zero-length range accepted, whole page cache
		 * invalidated.  Mirror the verdict into dmesg so search-dmesg
		 * surfaces this offensive line in console.log. */
		kmsg("<3>REPRODUCED: BLKSECDISCARD(0,0) zero-length range accepted; "
		     "whole block page cache invalidated (before=%d after=%d)\n",
		     before, after);
		printf("REPRODUCED: ioctl accepted a zero-length range and "
		       "invalidated the whole page cache (before=%d after=%d)\n",
		       before, after);
		return 1;	/* non-zero == reproduced */
	}
	if (ret < 0 && saved == EINVAL) {
		kmsg("<6>FIXED: BLKSECDISCARD(0,0) zero-length range rejected with "
		     "-EINVAL; page cache intact (before=%d after=%d)\n",
		     before, after);
		printf("FIXED: ioctl rejected the zero-length range with "
		       "-EINVAL; page cache intact (before=%d after=%d)\n",
		       before, after);
		return 0;	/* zero == not reproduced */
	}

	kmsg("<3>secdiscard_repro: UNEXPECTED ioctl result ret=%d errno=%d\n",
	     ret, saved);
	printf("UNEXPECTED ioctl result -- aborting\n");
	return 2;
}
```

## Kernel config

The reproducer is an out-of-tree module plus a userspace binary; it only needs
the block layer and module support, both of which a default bootable defconfig
already enables. The fragment below documents that (it is a no-op on a normal
defconfig):

```kconf:repro.config
# Block layer + module support (defaults are on for a bootable kernel);
# the out-of-tree secblk.ko helper and the block ioctls need nothing else.
CONFIG_BLOCK=y
CONFIG_MODULES=y
```

## Start script (init)

The key correction from the previous (failing) bundle: the verdict line lived
only in `exec.log` (SSH stdout) and never reached the serial console/dmesg, so
`search-dmesg: REPRODUCED` matched nothing in `console.log` and the offensive
line was never surfaced. This script runs the reproducer **as root** (via the
cloud user's passwordless `sudo`) for two reasons: (1) `devtmpfs` creates
`/dev/secblk0` root-owned and `BLKSECDISCARD` needs `O_RDWR` (`BLK_OPEN_WRITE`),
so root bypasses the DAC check on `open()` and reaches the buggy ioctl; (2)
root can write to `/dev/kmsg`, so the `REPRODUCED`/`FIXED` marker lands in the
kernel log → serial console (`console.log`), where `search-dmesg` finds it.

```init:run-repro.sh
#!/bin/bash
# The runner has insmod'd secblk.ko, which via devtmpfs creates /dev/secblk0.
# Wait briefly for the node to appear.
i=0
while [ ! -b /dev/secblk0 ] && [ "$i" -lt 100 ]; do
	sleep 0.1 2>/dev/null || sleep 1
	i=$((i + 1))
done

if [ ! -b /dev/secblk0 ]; then
	echo "FAIL: /dev/secblk0 did not appear (secblk module not loaded?)"
	lsmod 2>/dev/null | grep secblk || true
	exit 3
fi

# Run the reproducer as root.  The runner executes this script as the
# unprivileged cloud user, so use its passwordless sudo (Ubuntu cloud images
# grant NOPASSWD sudo by default).  Root is needed for two things:
#   1. open("/dev/secblk0", O_RDWR): the node is root-owned and BLKSECDISCARD
#      requires BLK_OPEN_WRITE; a non-root caller gets EACCES at open() and
#      never reaches the buggy ioctl.  (This is why an even earlier bundle
#      failed with "open: Permission denied".)
#   2. write the verdict to /dev/kmsg: the reproducer mirrors its REPRODUCED/
#      FIXED line into the kernel log so it appears on the serial console
#      (console.log), where `search-dmesg: REPRODUCED` can surface it.  The
#      previous bundle's verdict lived only in exec.log (SSH stdout) and was
#      invisible to dmesg scanning -- the run "did not succeed" precisely
#      because the offensive line never reached console.log.
rc=4
if [ "$(id -u)" -eq 0 ]; then
	./secdiscard_repro /dev/secblk0
	rc=$?
elif sudo -n true 2>/dev/null; then
	sudo -n ./secdiscard_repro /dev/secblk0
	rc=$?
else
	# No passwordless sudo: best-effort fallback.  Relax device perms and run
	# as the cloud user.  /dev/kmsg is not writable here, so the dmesg marker
	# is skipped, but the stdout verdict in exec.log is still produced.
	chmod 666 /dev/secblk0 2>/dev/null || true
	./secdiscard_repro /dev/secblk0
	rc=$?
fi
exit $rc
```

## Fix (patch-compare)

With `patch-compare: true` the runner builds **baseline** (v6.19 with this
patch stripped — the bug reproduces) and **patched** (v6.19 + the patch below —
the ioctl rejects the zero-length range) in parallel. The fix swaps the
hand-rolled alignment/overflow checks for `blk_validate_byte_range()` (which
rejects `!len` with `-EINVAL`) and computes the inclusive end as
`start + len - 1`, so a zero-length range can never reach
`truncate_bdev_range()`.

```patch:0001-blksecdiscard-zero-range-fix.patch
From 20260704073942.3760597-1-wozizhi@huaweicloud.com Mon Sep 17 00:00:00 2001
From: Zizhi Wo <wozizhi@huaweicloud.com>
Date: Sat, 4 Jul 2026 15:39:42 +0800
Subject: [PATCH] block: fix BLKSECDISCARD zero-length range causing page cache invalidation

Commit 697ba0b6ec4a ("block: fix integer overflow in BLKSECDISCARD") fixed
the start+len overflow via check_add_overflow() but did not handle the
start=0, len=0 case. There, start + len = 0, so end = 0 passes all checks,
and truncate_bdev_range()->truncate_inode_pages_range() is then called with
lend=UINT64_MAX, whitch is the "truncate to the end of file" sentinel, so
the entire page cache is invalidated.

Fix this by replacing the validation with blk_validate_byte_range(), which
already rejects a zero-length range and is what BLKDISCARD uses. This also
switches the alignment check from a hardcoded 512 to
bdev_logical_block_size().

Signed-off-by: Zizhi Wo <wozizhi@huaweicloud.com>
---
 block/ioctl.c | 13 +++++--------
 1 file changed, 5 insertions(+), 8 deletions(-)

diff --git a/block/ioctl.c b/block/ioctl.c
index 3d4ea1537457..3b7d33a737e8 100644
--- a/block/ioctl.c
+++ b/block/ioctl.c
@@ -176,8 +176,7 @@ static int blk_ioctl_discard(struct block_device *bdev, blk_mode_t mode,
 static int blk_ioctl_secure_erase(struct block_device *bdev, blk_mode_t mode,
 		void __user *argp)
 {
-	uint64_t start, len, end;
-	uint64_t range[2];
+	uint64_t range[2], start, len;
 	int err;
 
 	if (!(mode & BLK_OPEN_WRITE))
@@ -189,15 +188,13 @@ static int blk_ioctl_secure_erase(struct block_device *bdev, blk_mode_t mode,
 
 	start = range[0];
 	len = range[1];
-	if ((start & 511) || (len & 511))
-		return -EINVAL;
-	if (check_add_overflow(start, len, &end) ||
-	    end > bdev_nr_bytes(bdev))
-		return -EINVAL;
+	err = blk_validate_byte_range(bdev, start, len);
+	if (err)
+		return err;
 
 	inode_lock(bdev->bd_mapping->host);
 	filemap_invalidate_lock(bdev->bd_mapping);
-	err = truncate_bdev_range(bdev, mode, start, end - 1);
+	err = truncate_bdev_range(bdev, mode, start, start + len - 1);
 	if (!err)
 		err = blkdev_issue_secure_erase(bdev, start >> 9, len >> 9,
 						GFP_KERNEL);
-- 
2.52.0
```

## Notes

- The base commit is **v6.19**; the fix applies cleanly to v6.19's
  `block/ioctl.c` (the previous run confirmed this — both variants built and
  booted, and the patch applied via `git apply`, and the patched guest
  correctly returned `-EINVAL` for the zero-length range).
- The bug is in `blk_ioctl_secure_erase()`, guarded by
  `if (!bdev_max_secure_erase_sectors(bdev)) return -EOPNOTSUPP;`. No in-tree
  guest-creatable device (null_blk, brd, loop, nbd, zram) and not even QEMU's
  virtio-blk set `max_secure_erase_sectors`, so the `secblk.ko` helper is
  required to get past that capability check. It only sets the limit; the
  secure-erase bio path itself is never exercised because the bug issues
  `nr_sects = 0`.
- **Why the previous run "did not succeed":** the bug *was* reproduced
  (baseline exit 1, `REPRODUCED` in `exec.log`; patched exit 0, `FIXED`),
  but the verdict lines were captured only in `exec.log` (SSH stdout) and
  never reached the serial console (`console.log`) / dmesg. `search-dmesg`
  scans `console.log`, so `search-dmesg: REPRODUCED` matched nothing and the
  offensive line was never surfaced. This bundle's reproducer mirrors its
  verdict into `/dev/kmsg` (KERN_ERR) so it is emitted on the serial console,
  and runs as root (via passwordless `sudo`) so it can open the root-owned
  device `O_RDWR` and write to root-only `/dev/kmsg`.
- The reproducible signal is the **ioctl return value**: `0` (bug, baseline)
  vs `-EINVAL` (fixed, patched). The `mincore` residency check is
  corroboration: the baseline evicts every resident page (because
  `truncate_inode_pages_range(mapping, 0, -1)` means "to end of file"); the
  patched run leaves the page cache untouched.
- `search-dmesg: REPRODUCED` now surfaces the baseline run's reproduction line
  in the Issues view (it matches the `/dev/kmsg` line in `console.log`); the
  patched run writes `FIXED:` to dmesg instead, so it is not flagged.
  Pass/fail itself is driven by the init script's exit code
  (baseline 1 / patched 0).
- Run with `./run-kernel.py repro.md`.

## Expected output

Baseline (buggy) guest — `exec.log`:
```
BLKSECDISCARD(0,0): ret=0 errno=0 (success)
page-cache residency: before=16 after=0 pages (region=16 pages)
REPRODUCED: ioctl accepted a zero-length range and invalidated the whole page cache (before=16 after=0)
```
and the same `REPRODUCED: ...` line now also appears in `console.log` / dmesg
(emitted via `/dev/kmsg`), so `search-dmesg: REPRODUCED` flags it.
(init exits 1 -> reproduced)

Patched guest — `exec.log`:
```
BLKSECDISCARD(0,0): ret=-1 errno=22 (Invalid argument)
page-cache residency: before=16 after=16 pages (region=16 pages)
FIXED: ioctl rejected the zero-length range with -EINVAL; page cache intact (before=16 after=16)
```
(`FIXED: ...` is written to `/dev/kmsg` too; `search-dmesg: REPRODUCED` does
not match it.)
(init exits 0 -> not reproduced; this is the overall result)
