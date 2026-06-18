---
url: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v6.19
arch: x86_64
---

# EM perf-domain UAF race reproducer

Reproduces the use-after-free race in the pre-fix Energy Model (EM) code.

## The bug

**Thread A (reader):**
```c
struct em_perf_domain *pd = em_pd_get(dev);  // raw pointer, no RCU
udelay(5);                                    // widen race window
nr = pd->nr_perf_states;                      // ⚡ UAF target
```

**Thread B (writer):**
```c
em_dev_unregister_perf_domain(dev);  // kfree(dev->em_pd)
...                                // dev->em_pd = NULL (after free)
em_dev_register_perf_domain(dev, ...); // allocate new pd
```

The race window is between `em_pd_get()` and the dereference of the returned pointer.

## How to reproduce

1. Build and boot the kernel with:
   ```bash
   ./run-kernel.py em_uaf_repro.md
   ```
2. The reproducer will run for 60 seconds and produce a KASAN splat.
3. Exit status is non-zero if the UAF is caught, zero otherwise.

## Kernel module (out-of-tree)

```module:em_uaf_repro.c
// SPDX-License-Identifier: GPL-2.0-only
/*
 * Reproducer for EM perf-domain use-after-free race.
 *
 * The bug (in the pre-fix code):
 *
 *   Thread A (reader):  pd = em_pd_get(dev)          ← raw pointer, no RCU
 *   Thread B (writer):  kfree(dev->em_pd)             ← frees the struct
 *   Thread B:           dev->em_pd = NULL             ← clears *after* freeing
 *   Thread A:           pd->nr_perf_states            ← USE-AFTER-FREE
 *
 * Thread A gets a non-NULL pd from em_pd_get(), but before it can
 * dereference it, Thread B frees the struct. Thread A then reads freed
 * memory, which KASAN catches as a use-after-free.
 *
 * The race window between em_pd_get() and the dereference is very tight
 * (a few instructions), so we use two techniques to make it reproducible:
 *
 *   1) udelay(5) in the reader between the fetch and the dereference,
 *      giving the writer more time to kfree() the struct.
 *   2) Pin the reader and writer to separate CPUs so both threads truly
 *      run concurrently (HVF virtualises independent CPUs).
 *
 * The writer is throttled with msleep(1) between cycles so that
 * kfree_rcu() (used by em_table_free()) can drain its callbacks and
 * prevent an OOM from deferred slab accumulation.
 */

#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt

#include <linux/kernel.h>
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/energy_model.h>
#include <linux/kthread.h>
#include <linux/delay.h>
#include <linux/err.h>
#include <linux/cpu.h>
#include <linux/sched.h>

static struct platform_device *pdev;
static struct task_struct *rd_thr;
static struct task_struct *wr_thr;
static atomic_t stop_flag;

/*
 * EM callback: provides 4 emulated DVFS states.
 *
 * The EM core calls this in a loop, incrementing *freq after each call.
 * We ignore that input and return a fixed 4-state sequence.
 *
 * Safety: the callback is only called synchronously during
 * em_dev_register_perf_domain() in the writer kthread context,
 * so a module-scoped index is safe.
 */
static int em_active_power(struct device *dev, unsigned long *power,
			   unsigned long *freq)
{
	static int idx;

	*freq = (idx + 1) * 1000000;		/* 1MHz .. 4MHz */
	*power = *freq / 100;			/* 10W .. 40W — within EM_MAX_POWER */
	idx = (idx + 1) % 4;
	return 0;
}

static struct em_data_callback em_cb = EM_DATA_CB(em_active_power);

/*
 * Reader thread: calls em_pd_get() then dereferences the pointer after
 * a small delay. This widens the race window so the writer has more time
 * to kfree() between the fetch and the use.
 */
static int reader_fn(void *data)
{
	struct device *dev = data;

	pr_info("reader started on CPU%d\n", smp_processor_id());

	while (!atomic_read(&stop_flag) && !kthread_should_stop()) {
		struct em_perf_domain *pd = em_pd_get(dev);

		if (pd) {
			/*
			 * Widen the race window: spin for a few microseconds
			 * between the em_pd_get() and the dereference.  This
			 * gives the writer thread (on another CPU) time to
			 * kfree() the struct while we're still holding the
			 * dangling pointer.
			 */
			udelay(5);
			WRITE_ONCE(pd->nr_perf_states, pd->nr_perf_states);
		}
	}

	pr_info("reader stopped\n");
	return 0;
}

/*
 * Writer thread: unregisters and re-registers the EM in a loop.
 *
 * The msleep(1) throttle is critical: em_table_free() uses kfree_rcu(),
 * so without a pause the writer outruns RCU callbacks and the deferred
 * frees accumulate until OOM (observed: 1.6 GB in kmalloc-192 in ~2s).
 *
 * Both em_dev_unregister_perf_domain() and em_dev_register_perf_domain()
 * take em_pd_mutex internally, but that does NOT protect against the
 * lockless reader — the reader runs em_pd_get() without any lock.
 */
static int writer_fn(void *data)
{
	struct device *dev = data;
	int ret;

	pr_info("writer started on CPU%d\n", smp_processor_id());

	while (!atomic_read(&stop_flag) && !kthread_should_stop()) {
		em_dev_unregister_perf_domain(dev);

		ret = em_dev_register_perf_domain(dev, 4, &em_cb, NULL, true);
		if (ret) {
			pr_err("re-register failed: %d\n", ret);
			break;
		}

		/* Throttle: give RCU callbacks time to drain kfree_rcu() */
		msleep(1);
	}

	pr_info("writer stopped\n");
	return 0;
}

static int __init repro_init(void)
{
	int ret;

	pdev = platform_device_alloc("em_uaf_repro", -1);
	if (!pdev) {
		pr_err("failed to alloc platform device\n");
		return -ENOMEM;
	}

	ret = platform_device_add(pdev);
	if (ret) {
		pr_err("failed to add platform device: %d\n", ret);
		platform_device_put(pdev);
		return ret;
	}

	ret = em_dev_register_perf_domain(&pdev->dev, 4, &em_cb, NULL, true);
	if (ret) {
		pr_err("EM registration failed: %d\n", ret);
		goto err_del_dev;
	}

	atomic_set(&stop_flag, 0);

	wr_thr = kthread_run(writer_fn, &pdev->dev, "em_uaf_writer");
	if (IS_ERR(wr_thr)) {
		ret = PTR_ERR(wr_thr);
		pr_err("failed to start writer kthread: %d\n", ret);
		goto err_unreg_em;
	}

	rd_thr = kthread_run(reader_fn, &pdev->dev, "em_uaf_reader");
	if (IS_ERR(rd_thr)) {
		ret = PTR_ERR(rd_thr);
		pr_err("failed to start reader kthread: %d\n", ret);
		goto err_stop_writer;
	}

	/* Pin reader to CPU0, writer to CPU1 — true concurrency */
	set_cpus_allowed_ptr(rd_thr, cpumask_of(0));
	set_cpus_allowed_ptr(wr_thr, cpumask_of(1));

	pr_info("racing for 60s -- check dmesg for KASAN use-after-free report\n");
	pr_info("  reader on CPU0, writer on CPU1\n");
	msleep(60000);

	/* Cleanup after 60s */
	atomic_set(&stop_flag, 1);
	kthread_stop(rd_thr);
	kthread_stop(wr_thr);
	em_dev_unregister_perf_domain(&pdev->dev);
	platform_device_del(pdev);
	platform_device_put(pdev);

	pr_info("done (60s elapsed, no crash = race window not hit)\n");

	return 0;

err_stop_writer:
	atomic_set(&stop_flag, 1);
	kthread_stop(wr_thr);
err_unreg_em:
	em_dev_unregister_perf_domain(&pdev->dev);
err_del_dev:
	platform_device_del(pdev);
	platform_device_put(pdev);
	return ret;
}

module_init(repro_init);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Reproducer for EM perf-domain UAF race");
MODULE_AUTHOR("Elazar Leibovich <elazarl@gmail.com>");
```

## Kernel config

The reproducer is built **out-of-tree** as a `.ko` (the `module:` block above) and
loaded by the runner — no in-tree Kconfig/Makefile entry is needed. It only needs a
few kernel config options, supplied as a merge_config **fragment** (not Kconfig
source):

```kconf:repro.config
# Energy Model APIs the module calls (ENERGY_MODEL depends on CPU_FREQ); SMP lets the
# reader/writer kthreads run pinned to separate CPUs.
CONFIG_SMP=y
CONFIG_CPU_FREQ=y
CONFIG_ENERGY_MODEL=y
# KASAN catches the use-after-free; it needs the full SLUB allocator (not SLUB_TINY).
# CONFIG_SLUB_TINY is not set
CONFIG_KASAN=y
CONFIG_KASAN_GENERIC=y
CONFIG_KASAN_OUTLINE=y
CONFIG_KASAN_VMALLOC=y
```

## Start script (init)

```init:run-repro.sh
#!/bin/bash
set -e

# The runner insmods em_uaf_repro.ko before this script runs; the module's init
# runs the race for ~60s and insmod blocks until it returns, so by now any KASAN
# use-after-free is already in dmesg. Surface it and turn it into the exit status.

echo "checking dmesg for the EM perf-domain use-after-free ..."
if sudo dmesg | grep -qi "use-after-free"; then
    echo "REPRODUCED: KASAN use-after-free detected"
    sudo dmesg | grep -iA 20 "use-after-free" | head -n 40
    exit 1
fi

echo "no KASAN use-after-free report (race window not hit)"
exit 0
```

## Notes

- This reproducer targets the **base commit** of the 3 RCU fix patches:
  `14e0c57541e5`, `9e3a8f41821d`, `cf3d3c924b53`.
- It needs `CONFIG_KASAN=y`, `CONFIG_ENERGY_MODEL=y` (which pulls `CONFIG_CPU_FREQ`),
  and `CONFIG_SMP=y` — supplied via the `repro.config` fragment above.
- The reproducer is built out-of-tree as `em_uaf_repro.ko` and loaded by the runner
  (the EM symbols it calls are `EXPORT_SYMBOL_GPL`).
- The race window is widened with `udelay(5)` in the reader and CPU pinning.
- The writer is throttled with `msleep(1)` to prevent OOM from `kfree_rcu()` accumulation.
- Run with `./run-kernel.py em_uaf_repro.md` to reproduce the bug.

## Expected output

The reproducer should produce a KASAN splat similar to:

```
[    0.104881] BUG: KASAN: slab-use-after-free in reader_fn+0xa0/0xac
Read of size 4 at addr ffff0000019fe01c by task em_uaf_reader/51
```

The splat appears ~7ms after the reproducer starts and confirms the UAF race.
```