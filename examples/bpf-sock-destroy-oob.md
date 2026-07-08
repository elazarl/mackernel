---
commit: v6.19
thread-compare: https://lore.kernel.org/lkml/20260702224519.800135-1-xmei5@asu.edu/
search-dmesg: BUG: KASAN: slab-out-of-bounds
search-dmesg: bpf_sock_destroy
summary: slab-OOB in bpf_sock_destroy() reading sk_protocol off a TIME_WAIT mini-socket
tag: bpf
tag: net
tag: oob
tag: kasan
---

# slab-out-of-bounds in `bpf_sock_destroy()` via TIME_WAIT mini-socket

Reproducer for the bug fixed by the lkml patch *"bpf: reject mini-sockets in
`bpf_sock_destroy()`"*.

## The bug

`bpf_sock_destroy()` is a BPF kfunc that takes a `struct sock_common *`, casts
it to `struct sock *`, and reads `sk->sk_protocol` to check if it's TCP/UDP:

```c
__bpf_kfunc int bpf_sock_destroy(struct sock_common *sock)
{
    struct sock *sk = (struct sock *)sock;
    if (!sk->sk_prot->diag_destroy || (sk->sk_protocol != IPPROTO_TCP &&
                                       sk->sk_protocol != IPPROTO_UDP))
        return -EOPNOTSUPP;
    return sk->sk_prot->diag_destroy(sk, ECONNABORTED);
}
```

The BPF TCP iterator passes `sock_common *` for every socket it visits,
including **TIME_WAIT** and **NEW_SYN_RECV** mini-sockets. These only embed a
`sock_common` prefix — `sk_protocol` lives beyond it, so the read goes out of
bounds of the small `tw_sock_TCP` object (type confusion).

```
BUG: KASAN: slab-out-of-bounds in bpf_sock_destroy (net/core/filter.c:12673)
Read of size 2 at addr ffff888013ffc71c by task exploit/143
Call Trace:
 bpf_sock_destroy+.. / bpf_iter_run_prog+.. / bpf_iter_tcp_seq_show+..
 bpf_seq_read+.. / vfs_read+..
```

The fix adds `if (!sk_fullsock(sk)) return -EOPNOTSUPP;` before touching any
full-sock field.

## Why `thread-compare`

The fix is an unmerged lkml patch (not yet in any tag). `thread-compare` runs
**baseline** = plain `v6.19` (buggy, kfunc added in `4ddbcb886268`) vs
**patched** = `v6.19` + the thread's series applied. The two trees differ by
exactly that one fix.

## Trigger

A C program creates a TCP connection and closes the client side → **TIME_WAIT**.
It then loads a `BPF_TRACE_ITER` program attached to the "tcp" target that
calls `bpf_sock_destroy(ctx->sk_common)`, and `read()`s the iter fd. The
iterator visits every TCP socket; when it hits the TIME_WAIT mini-socket, the
kfunc reads `sk_protocol` out of bounds → KASAN slab-out-of-bounds.

`CONFIG_KASAN=y` is required: the OOB read is within the slab slot (offset 220
in a 256-byte `tw_sock_TCP`) and does not crash without a sanitizer.

`CONFIG_DEBUG_INFO_BTF=y` is required: the verifier needs the kernel's own BTF
(`/sys/kernel/btf/vmlinux`) to resolve the `bpf_sock_destroy` kfunc call.
`CONFIG_DEBUG_KERNEL=y` is needed too: the DWARF5 debug-info choice (which BTF
generates from) depends on it, and `tinyconfig` leaves it off.

```kconf:bpf.config
CONFIG_DEBUG_KERNEL=y
CONFIG_NET=y
CONFIG_INET=y
CONFIG_BPF=y
CONFIG_BPF_SYSCALL=y
CONFIG_BPF_JIT=y
# BPF_PROG_TYPE_TRACING (iter programs) is gated by CONFIG_BPF_EVENTS, which
# needs PERF_EVENTS + (KPROBE_EVENTS || UPROBE_EVENTS). FTRACE enables the
# tracing menu; tinyconfig leaves all of these off.
CONFIG_FTRACE=y
CONFIG_PERF_EVENTS=y
CONFIG_KPROBES=y
CONFIG_KPROBE_EVENTS=y
CONFIG_DEBUG_INFO_BTF=y
CONFIG_KASAN=y
CONFIG_KASAN_GENERIC=y
CONFIG_STACKTRACE=y
# CONFIG_SLUB_TINY is not set
```

```user:sock_destroy_oob.c
// SPDX-License-Identifier: GPL-2.0
/* Reproducer for slab-out-of-bounds in bpf_sock_destroy() on TIME_WAIT sockets.
 *
 * Reads /sys/kernel/btf/vmlinux to find the BTF type IDs of bpf_iter_tcp (the
 * iter attach target) and bpf_sock_destroy (the kfunc). Builds a BPF_TRACE_ITER
 * program that calls bpf_sock_destroy(ctx->sk_common), loads it, creates a TCP
 * TIME_WAIT socket, and reads the iter fd. On a buggy kernel the kfunc reads
 * sk_protocol past the sock_common prefix of a tw_sock_TCP -> KASAN OOB.
 *
 * No libbpf: raw bpf(2) syscalls + a minimal BTF parser. */
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <netinet/in.h>

#ifndef __NR_bpf
#define __NR_bpf 321
#endif
static int bpf(int cmd, void *attr, unsigned int sz) { return syscall(__NR_bpf, cmd, attr, sz); }

#define BPF_PROG_LOAD     5
#define BPF_LINK_CREATE  28
#define BPF_ITER_CREATE  33
#define BPF_PROG_TYPE_TRACING 26
#define BPF_TRACE_ITER        28

/* BPF instruction encoding */
struct bpf_insn { uint8_t code; uint8_t regs; int16_t off; int32_t imm; };
#define BPF_PSEUDO_KFUNC_CALL 2

/* BTF kinds */
#define BTF_KIND_FUNC 12

struct btf_hdr { uint16_t magic; uint8_t ver; uint8_t flags; uint32_t hdr_len;
                 uint32_t type_off; uint32_t type_len; uint32_t str_off; uint32_t str_len; };

/* Extra bytes after struct btf_type (12) for each kind. */
static int btf_extra(int kind, int vlen) {
    switch (kind) {
    case 1:  return 4;             /* INT */
    case 3:  return 12;            /* ARRAY */
    case 4:  return vlen*12;       /* STRUCT */
    case 5:  return vlen*12;       /* UNION */
    case 6:  return vlen*8;        /* ENUM */
    case 13: return vlen*8;        /* FUNC_PROTO */
    case 14: return 4;            /* VAR */
    case 15: return vlen*12;       /* DATASEC */
    case 17: return 4;            /* DECL_TAG */
    case 19: return vlen*12;      /* ENUM64 */
    default: return 0;
    }
}

/* Find the BTF type ID of a BTF_KIND_FUNC by name in /sys/kernel/btf/vmlinux. */
static unsigned int btf_func_id(const char *name) {
    int fd = open("/sys/kernel/btf/vmlinux", O_RDONLY);
    if (fd < 0) { perror("open /sys/kernel/btf/vmlinux"); return 0; }
    /* sysfs BTF doesn't support lseek(SEEK_END); read in a loop. */
    size_t cap = 1 << 20, len = 0;
    uint8_t *buf = malloc(cap);
    if (!buf) { close(fd); return 0; }
    for (;;) {
        if (len >= cap) { cap *= 2; buf = realloc(buf, cap); if (!buf) { close(fd); return 0; } }
        ssize_t n = read(fd, buf + len, cap - len);
        if (n < 0) { perror("read btf"); free(buf); close(fd); return 0; }
        if (n == 0) break;
        len += n;
    }
    close(fd);

    struct btf_hdr *h = (void *)buf;
    if (h->magic != 0xeB9F) { fprintf(stderr, "bad BTF magic\n"); free(buf); return 0; }
    uint32_t hl = h->hdr_len;
    const char *strs = (const char *)(buf + hl + h->str_off);
    uint8_t *p = buf + hl + h->type_off;
    uint8_t *end = p + h->type_len;
    unsigned int id = 1; /* type ID 0 = void */
    while (p + 12 <= end) {
        uint32_t name_off = *(uint32_t *)p;
        uint32_t info = *(uint32_t *)(p + 4);
        int kind = (info >> 24) & 0x1f;
        int vlen = info & 0xffff;
        const char *nm = strs + name_off;
        if (kind == BTF_KIND_FUNC && strcmp(nm, name) == 0) {
            free(buf);
            return id;
        }
        int sz_t = 12 + btf_extra(kind, vlen);
        p += sz_t;
        id++;
    }
    free(buf);
    fprintf(stderr, "BTF: function %s not found\n", name);
    return 0;
}

/* bpf_attr for PROG_LOAD — enough fields up to attach_btf_id (offset 108) + attach_btf_obj_fd (112). */
struct bpf_load_attr {
    uint32_t prog_type;            /*   0 */
    uint32_t insn_cnt;             /*   4 */
    uint64_t insns;                 /*   8 */
    uint64_t license;               /*  16 */
    uint32_t log_level;             /*  24 */
    uint32_t log_size;              /*  28 */
    uint64_t log_buf;               /*  32 */
    uint32_t kern_version;          /*  40 */
    uint32_t prog_flags;            /*  44 */
    char     prog_name[16];         /*  48 */
    uint32_t prog_ifindex;          /*  64 */
    uint32_t expected_attach_type;  /*  68 */
    uint32_t prog_btf_fd;           /*  72 */
    uint32_t func_info_rec_size;    /*  76 */
    uint64_t func_info;             /*  80 */
    uint32_t func_info_cnt;         /*  88 */
    uint32_t line_info_rec_size;    /*  92 */
    uint64_t line_info;             /*  96 */
    uint32_t line_info_cnt;        /* 104 */
    uint32_t attach_btf_id;        /* 108 */
    uint32_t attach_btf_obj_fd;    /* 112 */
};                                 /* size 116 */

struct bpf_link_attr {
    uint32_t prog_fd;       /*  0 */
    uint32_t target_fd;     /*  4 */
    uint32_t attach_type;   /*  8 */
    uint32_t flags;          /* 12 */
};

struct bpf_iter_attr {
    uint32_t link_fd;       /*  0 */
    uint32_t flags;          /*  4 */
};

int main(int argc, char **argv) {
    /* 1. Resolve BTF IDs from the kernel's BTF.
     * If two integer args are given, use them directly (from bpftool);
     * otherwise parse /sys/kernel/btf/vmlinux at runtime. */
    unsigned int iter_btf, kfunc_btf;
    if (argc >= 3) {
        iter_btf = (unsigned)atoi(argv[1]);
        kfunc_btf = (unsigned)atoi(argv[2]);
    } else {
        iter_btf = btf_func_id("bpf_iter_tcp");
        kfunc_btf = btf_func_id("bpf_sock_destroy");
    }
    if (!iter_btf || !kfunc_btf) {
        fprintf(stderr, "FAIL: could not resolve BTF IDs (need CONFIG_DEBUG_INFO_BTF)\n");
        return 2;
    }
    printf("bpf_iter_tcp BTF id: %u\n", iter_btf);
    printf("bpf_sock_destroy BTF id: %u\n", kfunc_btf);

    /* 2. Build the BPF program:
     *   r0 = 0                  // default return (continue)
     *   r1 = *(u64*)(r1 + 8)   // ctx->sk_common (offset 8 in bpf_iter__tcp)
     *   if r1 == 0, goto exit   // skip NULL (KF_TRUSTED_ARGS rejects NULL)
     *   r2 = *(u8*)(r1 + 18)   // skc_state (offset 18 in sock_common)
     *   if r2 != 6, goto exit   // only TIME_WAIT sockets (TCP_TIME_WAIT = 6)
     *   call bpf_sock_destroy   // kfunc(r1), src_reg=2, imm=btf_id
     *   r0 = 0                  // return 0 (continue iteration)
     *   exit */
    struct bpf_insn prog[] = {
        {0xb7, 0,          0, 0},           /* MOV64 IMM: r0 = 0 */
        {0x79, (1<<4)|1,  8, 0},           /* LDX MEM DW: r1 = *(u64*)(r1+8) */
        {0x15, 1,          4, 0},           /* JEQ IMM: if r1 == 0, goto +4 (exit) */
        {0x71, (1<<4)|2, 18, 0},           /* LDX MEM B: r2 = *(u8*)(r1+18) = skc_state */
        {0x55, 2,          2, 6},           /* JNE IMM: if r2 != 6 (TIME_WAIT), goto +2 (exit) */
        {0x85, (2<<4)|0,  0, (int32_t)kfunc_btf}, /* CALL kfunc: bpf_sock_destroy */
        {0xb7, 0,          0, 0},           /* MOV64 IMM: r0 = 0 */
        {0x95, 0,          0, 0},           /* EXIT */
    };

    char log[65536];
    struct bpf_load_attr la;
    memset(&la, 0, sizeof(la));
    la.prog_type = BPF_PROG_TYPE_TRACING;
    la.insn_cnt = sizeof(prog) / sizeof(prog[0]);
    la.insns = (uint64_t)(unsigned long)prog;
    la.license = (uint64_t)(unsigned long)"GPL";
    la.log_level = 1;
    la.log_size = sizeof(log);
    la.log_buf = (uint64_t)(unsigned long)log;
    la.expected_attach_type = BPF_TRACE_ITER;
    la.attach_btf_id = iter_btf;
    strncpy(la.prog_name, "sock_dst_tw", sizeof(la.prog_name) - 1);

    _Static_assert(offsetof(struct bpf_load_attr, attach_btf_id) == 108, "attach_btf_id off");
    _Static_assert(offsetof(struct bpf_load_attr, attach_btf_obj_fd) == 112, "attach_btf_obj_fd off");
    _Static_assert(offsetof(struct bpf_load_attr, expected_attach_type) == 68, "eat off");

    int prog_fd = bpf(BPF_PROG_LOAD, &la, sizeof(la));
    if (prog_fd < 0) {
        fprintf(stderr, "LOAD_FAIL errno=%d (%s)\n", errno, strerror(errno));
        log[sizeof(log)-1] = 0;
        fprintf(stderr, "verifier: %s\n", log);
        return 2;
    }
    printf("program loaded (fd=%d)\n", prog_fd);

    /* 3. Create a BPF link (attach the iter program). */
    struct bpf_link_attr lka;
    memset(&lka, 0, sizeof(lka));
    lka.prog_fd = prog_fd;
    lka.attach_type = BPF_TRACE_ITER;
    int link_fd = bpf(BPF_LINK_CREATE, &lka, sizeof(lka));
    if (link_fd < 0) {
        fprintf(stderr, "LINK_FAIL errno=%d (%s)\n", errno, strerror(errno));
        return 2;
    }

    /* 4. Create a TCP TIME_WAIT socket: connect, accept, close client first
     * then server. The side that initiates the active close (client) enters
     * TIME_WAIT after receiving the server's FIN. Without accept()+close
     * on the server, the client stays in FIN_WAIT2 (no server FIN). */
    int srv = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in addr = {.sin_family=AF_INET, .sin_addr.s_addr=htonl(INADDR_LOOPBACK)};
    bind(srv, (struct sockaddr *)&addr, sizeof(addr));
    listen(srv, 1);
    socklen_t alen = sizeof(addr);
    getsockname(srv, (struct sockaddr *)&addr, &alen);
    int cli = socket(AF_INET, SOCK_STREAM, 0);
    connect(cli, (struct sockaddr *)&addr, sizeof(addr));
    int acc = accept(srv, NULL, NULL);
    close(cli);  /* client sends FIN -> FIN_WAIT1 -> FIN_WAIT2 */
    close(acc);  /* server sends FIN -> client receives FIN -> TIME_WAIT */
    usleep(100000);  /* let loopback TCP state machine settle */
    printf("TIME_WAIT socket created on port %d\n", ntohs(addr.sin_port));

    /* 5. Create the iter fd and read it — this iterates all TCP sockets.
     * When the show function hits the TIME_WAIT mini-socket, bpf_sock_destroy
     * reads sk_protocol OOB -> KASAN slab-out-of-bounds (buggy kernel). */
    struct bpf_iter_attr ia;
    memset(&ia, 0, sizeof(ia));
    ia.link_fd = link_fd;
    int iter_fd = bpf(BPF_ITER_CREATE, &ia, sizeof(ia));
    if (iter_fd < 0) {
        fprintf(stderr, "ITER_FAIL errno=%d (%s)\n", errno, strerror(errno));
        return 2;
    }

    char buf[4096];
    ssize_t n;
    while ((n = read(iter_fd, buf, sizeof(buf))) > 0)
        fwrite(buf, 1, n, stdout);

    close(iter_fd);
    close(link_fd);
    close(prog_fd);
    close(srv);
    printf("iter read complete\n");
    return 0;
}
```

```init:init.sh
#!/bin/bash
# Reproducer for: bpf: slab-out-of-bounds in bpf_sock_destroy() on TIME_WAIT
#
# A BPF_TRACE_ITER program calls bpf_sock_destroy(ctx->sk_common) while the
# TCP iterator visits a TIME_WAIT mini-socket. The kfunc casts sock_common* to
# sock* and reads sk_protocol, which is beyond the sock_common prefix of the
# small tw_sock_TCP object -> KASAN slab-out-of-bounds.
#
#   baseline (v6.19)             -> KASAN OOB -> init.sh exits 1 (REPRODUCED)
#   patched  (v6.19 + fix)        -> -EOPNOTSUPP (sk_fullsock check) -> exits 0

cd "$(dirname "$0")"
sudo dmesg -C 2>/dev/null || true

# Try bpftool for reliable BTF type-ID lookup; fall back to the C parser.
ARGS=""
if command -v bpftool >/dev/null 2>&1; then
    DUMP="$(sudo bpftool btf dump file /sys/kernel/btf/vmlinux format raw 2>/dev/null || true)"
    ITER_BTF="$(printf '%s\n' "$DUMP" | grep "FUNC 'bpf_iter_tcp'" | head -1 | grep -oE '^\[[0-9]+\]' | tr -d '[]')"
    KFUNC_BTF="$(printf '%s\n' "$DUMP" | grep "FUNC 'bpf_sock_destroy'" | head -1 | grep -oE '^\[[0-9]+\]' | tr -d '[]')"
    if [ -n "$ITER_BTF" ] && [ -n "$KFUNC_BTF" ]; then
        echo "bpftool: bpf_iter_tcp=$ITER_BTF bpf_sock_destroy=$KFUNC_BTF"
        ARGS="$ITER_BTF $KFUNC_BTF"
    fi
fi

sudo ./sock_destroy_oob $ARGS
RC=$?

DMESG="$(sudo dmesg 2>/dev/null || true)"
if printf '%s\n' "$DMESG" | grep -q "BUG: KASAN: slab-out-of-bounds"; then
    echo "REPRODUCED: KASAN slab-out-of-bounds in bpf_sock_destroy"
    printf '%s\n' "$DMESG" | grep -A8 "BUG: KASAN" | head -12
    exit 1
fi

echo "NOT-REPRODUCED: no KASAN OOB (kfunc rejected mini-socket)"
exit 0
```
