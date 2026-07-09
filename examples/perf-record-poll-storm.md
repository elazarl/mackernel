---
url: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v7.2-rc1
patch-compare: true
arch: x86_64
tools: perf
search-user: BUG: poll storm
regex-user: Woken up [0-9]{4,} times
summary: perf record spins at 100% CPU (poll storm) when a monitored thread exits and its dead ring-buffer fd keeps returning POLLHUP
tag: perf
tag: tools
tag: poll-storm
---

# perf record: poll storm when monitored threads exit

Reproducer for the perf fix *"[PATCH] perf record: fix poll storm when
monitored threads exit"*. The defect is in the perf **userspace** tool, so
this bundle drives the **real** `perf` binary built from the job's own tree:
the `tools: perf` key makes the runner compile a version-matched perf
(v7.2-rc1) and ship it into the guest on `PATH` — no guest compiler or kernel
source needed. `perf record` then runs against a tiny multi-threaded workload
whose worker threads exit partway through while the main thread stays alive —
exactly the scenario the patch's lore describes ("15 pthreads in a compute
loop where one thread exits halfway through", "Woken up 1,300,000 → 3, CPU
100% → 0%").

## The bug

`fdarray__filter()` (`tools/lib/api/fd/array.c`) zeroes `events`/`revents` of
a dead fd but, since commit 59b4412f27f1 ("libperf: Avoid internal moving of
fdarray fds"), no longer sets `fd = -1`. POSIX requires `poll()` to report
`POLLHUP`/`POLLERR` regardless of the `events` mask, so the dead entry makes
`poll()` return immediately forever — a ~100% CPU spin in perf record's main
loop (and in the BPF sideband thread, which additionally never called the
filter at all). A single monitored-thread exit is enough to trigger it.

Step by step, for `perf record -p <pid>`:
- a process target uses a *dummy* cpu map (`target__uses_dummy_map` is true
  for `-p`), so `evlist__per_thread` is true and perf opens one per-thread
  mmap + pollfd entry per thread (`tools/lib/perf/evlist.c:510`).
- when a monitored thread exits, the kernel closes its ring-buffer fd, which
  then returns `POLLHUP`; the record main loop polls `thread->pollfd` then
  calls `fdarray__filter()` (`tools/perf/builtin-record.c:2802/2811`).
- the unpatched `fdarray__filter()` leaves `fd` set, so `fdarray__poll()`
  returns immediately every iteration → `thread->waking++` explodes → the
  final `[ perf record: Woken up N times to write data ]` line shows N in the
  millions, and the perf process sits at ~100% CPU.
- the BPF sideband thread (`tools/perf/util/sideband_evlist.c`) has the same
  defect and never called the filter at all, so it also spins until the
  session ends.

## The fix

The patch (applied in the "patched" variant of this `patch-compare` run) sets
`fda->entries[fd].fd = -1` in `fdarray__filter()` *and* calls
`evlist__filter_pollfd()` in the sideband thread. With `fd = -1`, `poll()`
ignores the dead entry, blocks on the remaining live fds, and the wakeup count
stays in the single digits (CPU idle). The init script treats a huge `Woken up
N` count or high perf CPU usage as the storm (exit 1 = reproduced) and a tiny
count / idle CPU as fixed (exit 0).

## Why an inline patch (not a lore URL)

The lore patch's `sideband_evlist.c` hunk carries context lines
`evlist__core(evlist)->nr_mmaps` / `evlist__mmap(evlist)[i]` — accessors that
do **not** exist in any mainline tree (v7.2-rc1 uses `evlist->core.nr_mmaps` /
`evlist->mmap[i]` directly), so `git am` of the lore series fails. This bundle
keeps `patch-compare: true` with an inline `patch` fence whose context is
rewritten to match v7.2-rc1's actual source; the fix itself is byte-identical
to the lore patch.

## Why `search-user`/`regex-user` (not `search-dmesg`)

The defect and its fix live entirely in perf userspace; the kernel merely
closes the dead ring-buffer fd (normal, no oops/warning). The serial console
is therefore byte-identical between baseline and patched, so only matchers
against the reproducer's own output (`exec.log`) can distinguish them:
- `search-user: BUG: poll storm` — printed by the init script only when the
  storm is detected (baseline), never on patched.
- `regex-user: Woken up [0-9]{4,} times` — matches perf's million-scale wakeup
  summary on baseline, not the single-digit count on patched.

```patch:fix.patch
diff --git a/tools/lib/api/fd/array.c b/tools/lib/api/fd/array.c
--- a/tools/lib/api/fd/array.c
+++ b/tools/lib/api/fd/array.c
@@ -122,6 +122,12 @@ int fdarray__filter(struct fdarray *fda, short revents,
 			if (entry_destructor)
 				entry_destructor(fda, fd, arg);
 
+			/*
+			 * Set fd to -1 so poll() ignores this entry; otherwise
+			 * POLLHUP/POLLERR are still reported for events=0 fds
+			 * (POSIX: always checked), causing a poll storm.
+			 */
+			fda->entries[fd].fd = -1;
 			fda->entries[fd].revents = fda->entries[fd].events = 0;
 			continue;
 		}
diff --git a/tools/perf/util/sideband_evlist.c b/tools/perf/util/sideband_evlist.c
--- a/tools/perf/util/sideband_evlist.c
+++ b/tools/perf/util/sideband_evlist.c
@@ -8,6 +8,7 @@
 #include <perf/mmap.h>
 #include <linux/perf_event.h>
 #include <limits.h>
+#include <poll.h>
 #include <pthread.h>
 #include <sched.h>
 #include <stdbool.h>
@@ -55,6 +56,19 @@ static void *perf_evlist__poll_thread(void *arg)
 		if (!draining)
 			evlist__poll(evlist, 1000);
 
+		/*
+		 * When a thread of the monitored target exits, its per-cpu
+		 * ring-buffer fd is closed and starts returning POLLHUP. Such
+		 * dead fds are never requested for POLLIN, but poll() reports
+		 * POLLHUP/POLLERR unconditionally, so leaving them in the
+		 * pollfd array makes the following evlist__poll() return
+		 * immediately forever, spinning this thread at 100% CPU.
+		 *
+		 * Filter them out here, mirroring what the 'perf record' main
+		 * loop does after fdarray__poll().
+		 */
+		evlist__filter_pollfd(evlist, POLLERR | POLLHUP);
+
 		for (i = 0; i < evlist->core.nr_mmaps; i++) {
 			struct mmap *map = &evlist->mmap[i];
 			union perf_event *event;
```

```kconf:extra.config
# perf record needs the perf_event subsystem; it's default-y everywhere
# but make it explicit so a minimal base config still supports it.
CONFIG_PERF_EVENTS=y
```

```user:workload.c
#define _GNU_SOURCE
#include <stdio.h>
#include <sched.h>
#include <signal.h>
#include <unistd.h>
#include <stdlib.h>

/*
 * Multi-threaded workload for the perf-record poll-storm reproducer.
 *
 * Uses clone(2) (not pthreads) so the binary links with plain `gcc` and
 * needs no extra library: each child is a true kernel thread
 * (CLONE_THREAD) sharing the process, so a process-targeted
 * `perf record -p` opens one per-thread ring-buffer fd for it.
 *
 * The NWORK workers do a tiny compute burst (so perf records a couple of
 * task-clock samples) then exit at staggered times (~1.5..2.2s) while the
 * main thread stays alive for 7s.  A worker exit closes its ring-buffer
 * fd -> POLLHUP -> the poll storm.  The long-lived main thread keeps
 * perf's session (and the storm) running until it finally exits.
 */

#define NWORK 4
static char stacks[NWORK][65536];

static int worker(void *arg)
{
	long n = (long)arg;

	usleep(100000);                            /* let perf attach first */
	volatile unsigned long x = 0;
	for (x = 0; x < 2000000UL; x++) { }        /* brief compute burst */
	usleep(1400000 + n * 200000);              /* exit ~1.5/1.7/1.9/2.1s */
	return 0;
}

int main(void)
{
	long i;

	for (i = 0; i < NWORK; i++) {
		pid_t tid = clone(worker, stacks[i] + sizeof(stacks[i]),
				  CLONE_THREAD | CLONE_SIGHAND | CLONE_VM |
				  CLONE_FILES | CLONE_FS | SIGCHLD, (void *)i);
		if (tid < 0) {
			perror("clone");
			return 2;
		}
	}

	sleep(7);                                   /* keep the process alive */
	return 0;
}
```

```init:init.sh
#!/bin/bash
# Reproducer for the perf record poll-storm bug.
#
# The defect is in fdarray__filter() (tools/lib/api/fd/array.c): it zeroes
# events/revents of a dead fd but (since commit 59b4412f27f1) doesn't set
# fd=-1, so poll() keeps reporting POLLHUP (POSIX: always checked) and
# spins at ~100% CPU.  The patch (applied in the "patched" variant of this
# patch-compare run) sets fd=-1 so poll() ignores the entry, and adds the
# same filter call to the BPF sideband thread.
#
# This drives the REAL perf binary (built from the job's tree via
# `tools: perf`, shipped on PATH) on a multi-threaded workload whose
# threads exit partway while the main thread stays alive.  The storm
# inflates perf's "Woken up N times" summary and its CPU usage; the fix
# makes poll() block again (N tiny, CPU idle).

set -u
echo "=== perf record poll-storm reproducer ==="

command -v perf >/dev/null 2>&1 || { echo "FAIL: perf not on PATH"; exit 2; }
perf --version 2>&1 | head -1
echo "uid=$(id -u) perf_event_paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null)"

# The guest runs the script as a non-root user (uid 1000) and Ubuntu 24.04
# ships with perf_event_paranoid=4, which blocks unprivileged perf_event_open.
# Lower it so perf can attach.  Try a direct write first (we may be root),
# then passwordless sudo (the Ubuntu cloud image grants the login user
# NOPASSWD sudo); -n keeps sudo from ever hanging on a password prompt.
if ! sh -c 'echo -1 > /proc/sys/kernel/perf_event_paranoid' 2>/dev/null; then
	sudo -n sh -c 'echo -1 > /proc/sys/kernel/perf_event_paranoid' 2>/dev/null \
		|| echo "warning: could not lower perf_event_paranoid"
fi

# Sanity-check that perf can open events here before the long run.  A quick
# self-record; we only bail early on a *permission* error (paranoia/caps) --
# a benign non-zero exit from a too-short workload is fine and ignored.
perf record -e task-clock -o /tmp/perfcheck.data -- sleep 0.5 \
		>/tmp/perfcheck.err 2>&1 || true
# Success signal is perf's own "Captured and wrote" line -- printed only after
# perf_event_open succeeded and it recorded.  Do NOT grep stderr for
# "permission"/"perf_event_paranoid": perf emits a benign "Kernel address maps
# are restricted, check ... perf_event_paranoid" warning (kptr_restrict) even on
# a fully successful record, which false-failed this check.
if ! grep -q 'Captured and wrote' /tmp/perfcheck.err; then
	echo "FAIL: perf cannot open events in this guest:"
	cat /tmp/perfcheck.err
	echo "(perf_event_paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null), uid=$(id -u))"
	exit 2
fi
rm -f /tmp/perfcheck.data
echo "perf sanity check OK"

# Locate the workload binary shipped alongside this script.
HERE=$(cd "$(dirname "$0")" && pwd)
WL=""
for d in "$HERE" "$PWD" /tmp/mkbundle; do
	if [ -x "$d/workload" ]; then WL="$d/workload"; break; fi
done
[ -n "$WL" ] || { echo "FAIL: workload binary not found"; exit 2; }
echo "workload=$WL"

# Start the workload: 4 short-lived worker threads + a 7s-lived main thread.
"$WL" & WP=$!
sleep 0.1                                   # let the threads spawn

# Attach perf to the running process (per-thread mmaps -> a thread exit
# POLLHUPs its fd).  Run perf as the current user (paranoia is now -1) so
# $PERFPID is perf itself and we can sample its CPU directly.
perf record -p "$WP" -e task-clock -o /tmp/perf.data 2>/tmp/perf.err &
PERFPID=$!

# Workers exit ~1.5..2.2s; the storm begins then.  Wait past that point.
sleep 3

# Sample perf's total CPU (all threads) over a 2s window.  Fields 14+15 of
# /proc/<pid>/stat are utime+stime in clock ticks (USER_HZ, usually 100):
# a 100%-CPU spin for 2s ~= 200 ticks per spinning thread.
cpu_ticks() {
	awk '{ s += $14 + $15 } END { print s+0 }' /proc/$1/task/*/stat 2>/dev/null
}
c1=$(cpu_ticks "$PERFPID")
sleep 2
c2=$(cpu_ticks "$PERFPID")
CPU_DELTA=$((c2 - c1))
echo "PERF_CPU_TICKS_DELTA=$CPU_DELTA (2s window; storm ~= 200+ per spinning thread)"

# Let perf finish when the workload's main thread exits (~7s); its final
# summary line "[ perf record: Woken up N times to write data ]" is what
# we read.
wait "$PERFPID"; PERF_EXIT=$?
wait "$WP" 2>/dev/null

echo "--- perf stderr ---"
cat /tmp/perf.err

WOK=$(sed -n 's/.*Woken up \([0-9][0-9]*\) times.*/\1/p' /tmp/perf.err | head -1)
echo "Woken_up=${WOK:-none}"

# If perf never printed its "Woken up" summary it did not run the main
# loop to completion (event-open failure, missing libs, etc.) -- that is
# an infrastructure failure, not a pass.
if [ -z "$WOK" ]; then
	echo "FAIL: perf did not run to completion (no 'Woken up' summary)"
	exit 2
fi

# Storm iff the wakeup count exploded (millions on baseline vs ~5-10 fixed)
# or perf burned a whole core during the sample window.
storm=0
if [ "$WOK" -ge 1000 ]; then
	storm=1
fi
if [ "$CPU_DELTA" -ge 100 ]; then
	storm=1
fi

if [ "$storm" = 1 ]; then
	echo "BUG: poll storm - perf record spun after a monitored thread exited (Woken up $WOK, CPU ticks $CPU_DELTA/2s)"
	exit 1
fi
echo "ok: no poll storm - fd=-1 keeps poll() blocking as expected (Woken up $WOK, CPU ticks $CPU_DELTA/2s)"
exit 0
```
