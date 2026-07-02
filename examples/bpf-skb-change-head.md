---
commit: v6.17
arch: x86_64
thread-compare: https://lore.kernel.org/all/20251023125532.182262-1-daniel@iogearbox.net
search-dmesg: kernel BUG at
regex-dmesg: invalid opcode|general protection fault
---

# Reproducer — negative `head_room` in `bpf_skb_change_head()` -> `BUG_ON` in `pskb_expand_head()`

This is a `thread-compare` run: **baseline** is plain `v6.17`; **patched** is
`v6.17` with the lore series *“bpf: Reject negative head_room in
\__bpf_skb_change_head”* applied. The two trees differ by exactly that fix.

## Why the previous bundle did not reproduce

The previous `init.sh` only printed `uname -r` and the last 20 `dmesg` lines and
exited `0` on **both** variants — so the bug was never exercised and the run
was not a reproduction. The bug is not in boot or build; it is triggered only
by actually running a BPF program that calls `bpf_skb_change_head()` with a
bad `head_room`, which the old script never did.

## The bug

`bpf_skb_change_head()` is exposed with `arg2_type = ARG_ANYTHING`, so the
verifier lets a program pass any `u32` `head_room`, including values that are
negative when read as a signed `int` (e.g. `0x90000000`).

`__bpf_skb_change_head()` only rejects when:

```c
if (flags || (!skb_is_gso(skb) && new_len > max_len) || new_len < skb->len)
        return -EINVAL;
```

A huge but **non-wrapping** `head_room` on a **GSO** skb skips the
`new_len > max_len` clause (`skb_is_gso()` is true) and keeps
`new_len >= skb->len`, so the guard does not fire. The value then reaches
`skb_cow()` -> `__skb_cow()`:

```c
int delta = 0;
if (headroom > skb_headroom(skb))
        delta = headroom - skb_headroom(skb);     /* stored as *signed* int */
if (delta || cloned)
        return pskb_expand_head(skb, ALIGN(delta, NET_SKB_PAD), 0, ...);
```

For `head_room >= 0x80000000 + skb_headroom`, `delta` is a **negative** `int`;
`ALIGN(delta, NET_SKB_PAD)` stays negative and is handed to
`pskb_expand_head()`, whose very first line is:

```c
BUG_ON(nhead < 0);        /* net/core/skbuff.c */
```

→ kernel `BUG()` → oops that kills the calling task (the loader, via
`SIGSEGV`). The fix rejects `(s32)head_room < 0` up front with `-EINVAL`, so no
oops.

## How the GSO skb is built

`bpf_prog_test_run_skb()` (the `BPF_PROG_RUN` path for `SCHED_CLS`) lets the
caller set `__sk_buff.gso_size`; a non-zero `gso_size` makes `skb_is_gso()`
true, which is exactly what bypasses the `new_len > max_len` check above. The
loader supplies a `__sk_buff` context with only `gso_size = 8` / `gso_segs = 1`
set (every other field zero, satisfying the test-run’s `range_is_zero` checks).

## Expected result

| variant  | behaviour                                                        | init.sh exit |
|----------|------------------------------------------------------------------|--------------|
| baseline | loader killed by the kernel oops (`SIGSEGV`); `BUG:` in dmesg   | **1** (reproduced) |
| patched  | helper returns `-EINVAL`; loader prints `PATCHED_OK` and exits 0 | 0 (fixed)    |

```kconf:bpf.config
# Make sure the BPF syscall and the SCHED_CLS program type (registered under
# CONFIG_NET, test_run = bpf_prog_test_run_skb) are available.
CONFIG_NET=y
CONFIG_BPF=y
CONFIG_BPF_SYSCALL=y
CONFIG_BPF_JIT=y
```

```user:loader.c
// SPDX-License-Identifier: GPL-2.0
/*
 * Loader for the bpf_skb_change_head negative-head_room reproducer.
 *
 * Loads a tiny BPF_PROG_TYPE_SCHED_CLS program that calls
 * bpf_skb_change_head(ctx, 0x90000000, 0) and runs it via BPF_PROG_RUN on a
 * GSO skb (gso_size != 0).  On an unfixed kernel the huge head_room makes
 * __skb_cow() compute a negative `delta` (signed int overflow) which is then
 * passed as `nhead` to pskb_expand_head(), hitting BUG_ON(nhead < 0) -> oops.
 * On a fixed kernel the helper rejects the negative head_room with -EINVAL
 * and the program returns cleanly.
 *
 * No libbpf / no kernel headers: raw bpf(2) syscall + hand-rolled structs.
 */
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/syscall.h>

#ifndef __NR_bpf
#define __NR_bpf 321
#endif

static int bpf(int cmd, void *attr, unsigned int size)
{
	return syscall(__NR_bpf, cmd, attr, size);
}

/* ---- BPF commands / types ---- */
#define BPF_PROG_LOAD   5
#define BPF_PROG_RUN    10
#define BPF_PROG_TYPE_SCHED_CLS 3
#define BPF_FUNC_skb_change_head 43

/* ---- BPF instruction encoding ---- */
struct bpf_insn {
	uint8_t  code;
	uint8_t  regs;   /* dst_reg:4 | src_reg:4 */
	int16_t  off;
	int32_t  imm;
};
#define BPF_ALU64 0x07
#define BPF_MOV   0xb0
#define BPF_K     0x00
#define BPF_JMP   0x05
#define BPF_CALL  0x80
#define BPF_EXIT  0x90
#define MOV64_IMM(dst, im) (struct bpf_insn){BPF_ALU64|BPF_MOV|BPF_K, (dst)&0xf, 0, (im)}
#define CALL_FN(fn)        (struct bpf_insn){BPF_JMP|BPF_CALL, 0, 0, (fn)}
#define EXIT_INSN()        (struct bpf_insn){BPF_JMP|BPF_EXIT, 0, 0, 0}

/* ---- bpf(2) attribute structs (must match the kernel union layout) ---- */
struct bpf_load_attr {
	uint32_t prog_type;            /*  0 */
	uint32_t insn_cnt;             /*  4 */
	uint64_t insns;                 /*  8 */
	uint64_t license;               /* 16 */
	uint32_t log_level;             /* 24 */
	uint32_t log_size;              /* 28 */
	uint64_t log_buf;               /* 32 */
	uint32_t kern_version;          /* 40 */
	uint32_t prog_flags;            /* 44 */
	char     prog_name[16];         /* 48 */
	uint32_t prog_ifindex;          /* 64 */
	uint32_t expected_attach_type;  /* 68 */
};                                 /* size 72 */

struct bpf_run_attr {
	uint32_t prog_fd;       /*  0 */
	uint32_t retval;         /*  4 */
	uint32_t data_size_in;  /*  8 */
	uint32_t data_size_out; /* 12 */
	uint64_t data_in;        /* 16 */
	uint64_t data_out;       /* 24 */
	uint32_t repeat;         /* 32 */
	uint32_t duration;       /* 36 */
	uint32_t ctx_size_in;    /* 40 */
	uint32_t ctx_size_out;  /* 44 */
	uint64_t ctx_in;          /* 48 */
	uint64_t ctx_out;        /* 56 */
	uint32_t flags;           /* 64 */
	uint32_t cpu;             /* 68 */
	uint32_t batch_size;     /* 72 */
};                          /* size 76 */

/* __sk_buff UAPI offsets we care about (verified: sizeof(struct __sk_buff)=192). */
#define SKB_SIZE        192
#define SKB_OFF_GSOSEGS 164
#define SKB_OFF_GSOSIZE 176

/* head_room passed to bpf_skb_change_head. 0x90000000 is negative as s32 and
 * large enough that delta = head_room - skb_headroom() wraps to a negative int
 * for any realistic skb headroom, while new_len = skb->len + head_room does
 * not wrap below skb->len (so the helper's own guard does not catch it) as
 * long as the skb is GSO (which we force via gso_size). */
#define HEAD_ROOM 0x90000000u

_Static_assert(offsetof(struct bpf_load_attr, insns) == 8, "insns off");
_Static_assert(offsetof(struct bpf_load_attr, license) == 16, "license off");
_Static_assert(offsetof(struct bpf_load_attr, log_buf) == 32, "log_buf off");
_Static_assert(offsetof(struct bpf_load_attr, expected_attach_type) == 68, "eat off");
_Static_assert(offsetof(struct bpf_run_attr, data_in) == 16, "data_in off");
_Static_assert(offsetof(struct bpf_run_attr, ctx_in) == 48, "ctx_in off");
_Static_assert(offsetof(struct bpf_run_attr, batch_size) == 72, "batch off");

int main(void)
{
	char log[65536];
	const char *lic = "GPL";

	/* r1 already holds ctx (skb); pass head_room in r2, flags=0 in r3.
	 * Return the helper's result so a fixed kernel reports -EINVAL. */
	struct bpf_insn prog[] = {
		MOV64_IMM(2, (int32_t)HEAD_ROOM),  /* r2 = head_room */
		MOV64_IMM(3, 0),                    /* r3 = flags = 0 */
		MOV64_IMM(4, 0),                    /* r4 = 0 (unused) */
		MOV64_IMM(5, 0),                    /* r5 = 0 (unused) */
		CALL_FN(BPF_FUNC_skb_change_head),  /* bpf_skb_change_head(r1, r2, r3) */
		EXIT_INSN(),                        /* r0 = helper return value */
	};

	struct bpf_load_attr la;
	memset(&la, 0, sizeof(la));
	la.prog_type = BPF_PROG_TYPE_SCHED_CLS;
	la.insn_cnt  = sizeof(prog) / sizeof(prog[0]);
	la.insns     = (uint64_t)(unsigned long)prog;
	la.license   = (uint64_t)(unsigned long)lic;
	la.log_level = 1;
	la.log_size  = sizeof(log);
	la.log_buf   = (uint64_t)(unsigned long)log;
	strncpy(la.prog_name, "chhead_repro", sizeof(la.prog_name) - 1);

	int fd = bpf(BPF_PROG_LOAD, &la, sizeof(la));
	if (fd < 0) {
		fprintf(stderr, "LOAD_FAIL errno=%d (%s)\n", errno, strerror(errno));
		log[sizeof(log) - 1] = 0;
		fprintf(stderr, "verifier: %s\n", log);
		return 2;
	}

	/* Build a GSO __sk_buff context: only gso_segs/gso_size are non-zero. */
	uint8_t ctx[SKB_SIZE];
	memset(ctx, 0, sizeof(ctx));
	*(uint32_t *)(ctx + SKB_OFF_GSOSEGS) = 1;   /* gso_segs <= GSO_MAX_SEGS */
	*(uint32_t *)(ctx + SKB_OFF_GSOSIZE) = 8;   /* gso_size != 0 -> skb_is_gso() */

	uint8_t data[64];
	memset(data, 0, sizeof(data));

	uint8_t data_out[64];
	memset(data_out, 0, sizeof(data_out));
	uint8_t ctx_out[SKB_SIZE];
	memset(ctx_out, 0, sizeof(ctx_out));

	struct bpf_run_attr ra;
	memset(&ra, 0, sizeof(ra));
	ra.prog_fd      = fd;
	ra.data_size_in = sizeof(data);
	ra.data_size_out = sizeof(data_out);
	ra.data_in      = (uint64_t)(unsigned long)data;
	ra.data_out     = (uint64_t)(unsigned long)data_out;
	ra.repeat       = 1;
	ra.ctx_size_in  = sizeof(ctx);
	ra.ctx_size_out = sizeof(ctx_out);
	ra.ctx_in       = (uint64_t)(unsigned long)ctx;
	ra.ctx_out      = (uint64_t)(unsigned long)ctx_out;

	int r = bpf(BPF_PROG_RUN, &ra, sizeof(ra));
	if (r < 0) {
		fprintf(stderr, "RUN_FAIL errno=%d (%s)\n", errno, strerror(errno));
		close(fd);
		return 3;
	}

	printf("PATCHED_OK retval=%u duration=%u\n", ra.retval, ra.duration);
	close(fd);
	return 0;
}
```

```init:init.sh
#!/bin/bash
# Reproducer for the bug fixed by:
#   "bpf: Reject negative head_room in __bpf_skb_change_head"
#
# A BPF_PROG_TYPE_SCHED_CLS program calls bpf_skb_change_head(ctx, 0x90000000, 0)
# on a skb forced to be GSO (ctx.gso_size != 0). The helper's own guard only
# rejects when (!gso && new_len > max_len) || new_len < len; a huge but
# non-wrapping head_room avoids that because the skb is GSO. The value then
# reaches __skb_cow(), which computes delta = head_room - skb_headroom() as a
# *signed* int; for head_room >= 0x80000000 that delta is negative. ALIGN(delta,
# NET_SKB_PAD) stays negative and is passed as `nhead` to pskb_expand_head(),
# which does BUG_ON(nhead < 0) -> kernel oops that kills the loader (SIGSEGV).
#
# With the fix the helper rejects the negative head_room with -EINVAL, the
# program returns -EINVAL and the loader exits cleanly (exit 0).
#
#   baseline (v6.17)        -> oops      -> init.sh exits 1 (REPRODUCED)
#   patched  (v6.17 + fix) -> -EINVAL   -> init.sh exits 0 (NOT reproduced)

cd "$(dirname "$0")"

# An oops should kill only the loader, not the whole guest.
sudo sysctl -qw kernel.panic_on_oops=0 2>/dev/null || true
# Start from an empty ring buffer so only this run's messages are inspected.
sudo dmesg -C 2>/dev/null || true

sudo ./loader >/tmp/loader.out 2>&1
RC=$?

echo "----- loader output -----"
cat /tmp/loader.out 2>/dev/null || true
echo "----- loader exit: $RC -----"

DMESG="$(sudo dmesg 2>/dev/null || true)"
if printf '%s\n' "$DMESG" | grep -qE 'kernel BUG|BUG:|invalid opcode|general protection|Oops|UBSAN|\[ cut here \]'; then
	echo "REPRODUCED: kernel BUG/oops detected in dmesg"
	printf '%s\n' "$DMESG" | tail -n 40
	exit 1
fi

if [ "$RC" -gt 128 ]; then
	echo "REPRODUCED: loader killed by signal $((RC-128)) (kernel oops)"
	exit 1
fi

echo "NOT-REPRODUCED: loader exited cleanly (head_room rejected by the fix)."
exit 0
```