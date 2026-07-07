---
url: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v6.19
arch: arm64
patch-compare: true
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
3. The harness detects the KASAN use-after-free from the serial console log
   (baseline reproduces it; the patched build is clean).

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
		cond_resched();
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

static void __exit repro_exit(void)
{
}
module_exit(repro_exit);

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

## Fix (patch-compare)

With `patch-compare: true`, the runner builds **baseline** (v6.19, patch stripped —
the UAF reproduces) and **patched** (v6.19 + the fix below — the UAF is gone) in
parallel. The essential fix is patch 2/3: it unpublishes `dev->em_pd` before teardown
and defers the free to an RCU grace period (`kfree_rcu_mightsleep`), so a reader that
already fetched the pointer keeps valid memory. Patch 3/3 documents the devfreq_cooling
caller and must apply on top of 2/3.

```patch:0002-em-defer-free-rcu.patch
From 9e3a8f41821da7f69e17a3d54bc1b423fc472135 Mon Sep 17 00:00:00 2001
From: Sivan Zohar-Kotzer <sivany32@gmail.com>
Date: Thu, 11 Jun 2026 22:56:57 +0300
Subject: [PATCH 2/3] PM: EM: Defer freeing of the perf domain to an RCU grace
 period

em_dev_unregister_perf_domain() frees dev->em_pd with kfree() and only
clears the pointer afterwards. Lockless readers such as the dtpm
devfreq callbacks fetch the perf domain with em_pd_get() and then
dereference it; racing with unregistration they can dereference freed
memory even under rcu_read_lock(), because only the perf state table is
RCU-protected (freed with kfree_rcu()), not struct em_perf_domain
itself.

Make the perf domain lifetime RCU-safe:

 - annotate dev->em_pd as __rcu so sparse checks every access, and
   convert all accesses to the RCU accessors: publication with
   rcu_assign_pointer(), mutex-side reads with
   rcu_dereference_protected(), and the lockless read in em_pd_get()
   with rcu_dereference(), so lockdep verifies that em_pd_get() callers
   hold rcu_read_lock(),

 - add em_pd_get_check() for callers that serialize against
   unregistration by other means than RCU, taking a caller-provided
   lockdep expression that documents that serialization (CPU perf
   domains are never unregistered; registration-time probes in
   dtpm_devfreq and devfreq_cooling cannot race with unregistration;
   em_dev_update_chip_binning() callers own the device), and convert
   those callers,

 - clear dev->em_pd (with RCU_INIT_POINTER()) before any teardown so
   new readers cannot pick up a perf domain that is being destroyed,

 - free struct em_perf_domain with kfree_rcu_mightsleep() so readers
   that already fetched the pointer inside an RCU read-side critical
   section can keep using it until they drop rcu_read_lock().

This is the same clear-then-RCU-free idiom used elsewhere in the tree
for per-device pointers. The scheduler publishes these very EM objects
through root_domain::pd (struct perf_domain __rcu *), installed with
rcu_assign_pointer() and freed via call_rcu() in build_perf_domains(),
and read under rcu_read_lock() with rcu_dereference(rd->pd) in
find_energy_efficient_cpu(). Networking does the same for e.g.
net_device->ip_ptr (__in_dev_get_rcu(), freed via call_rcu() in
inetdev_destroy()) and net_device->rx_handler (rcu_assign_pointer() +
synchronize_net() in netdev_rx_handler_unregister()).

Fixes: 1bc138c62295 ("PM / EM: add support for other devices than CPUs in Energy Model")
Signed-off-by: Sivan Zohar-Kotzer <sivany32@gmail.com>
Co-developed-by: Elazar Leibovich <elazarl@gmail.com>
Signed-off-by: Elazar Leibovich <elazarl@gmail.com>
---
 drivers/powercap/dtpm_devfreq.c   |  3 +-
 drivers/thermal/devfreq_cooling.c |  3 +-
 include/linux/device.h            |  2 +-
 include/linux/energy_model.h      | 17 ++++++
 kernel/power/energy_model.c       | 93 ++++++++++++++++++++-----------
 5 files changed, 82 insertions(+), 36 deletions(-)

diff --git a/drivers/powercap/dtpm_devfreq.c b/drivers/powercap/dtpm_devfreq.c
index 13f70519b53b..81a6399c5e79 100644
--- a/drivers/powercap/dtpm_devfreq.c
+++ b/drivers/powercap/dtpm_devfreq.c
@@ -169,7 +169,8 @@ static int __dtpm_devfreq_setup(struct devfreq *devfreq, struct dtpm *parent)
 	struct em_perf_domain *pd;
 	int ret = -ENOMEM;
 
-	pd = em_pd_get(dev);
+	/* Setup-time probe; the EM cannot be unregistered concurrently. */
+	pd = em_pd_get_check(dev, true);
 	if (!pd) {
 		ret = dev_pm_opp_of_register_em(dev, NULL);
 		if (ret) {
diff --git a/drivers/thermal/devfreq_cooling.c b/drivers/thermal/devfreq_cooling.c
index 8fd7cf1932cd..41e04cbcace2 100644
--- a/drivers/thermal/devfreq_cooling.c
+++ b/drivers/thermal/devfreq_cooling.c
@@ -413,7 +413,8 @@ of_devfreq_cooling_register_power(struct device_node *np, struct devfreq *df,
 	ops->get_cur_state = devfreq_cooling_get_cur_state;
 	ops->set_cur_state = devfreq_cooling_set_cur_state;
 
-	em = em_pd_get(dev);
+	/* Registration-time probe; the cooling device is not live yet. */
+	em = em_pd_get_check(dev, true);
 	if (em && !em_is_artificial(em)) {
 		dfc->em_pd = em;
 		ops->get_requested_power =
diff --git a/include/linux/device.h b/include/linux/device.h
index 0be95294b6e6..8e750524a3c4 100644
--- a/include/linux/device.h
+++ b/include/linux/device.h
@@ -585,7 +585,7 @@ struct device {
 	struct dev_pm_domain	*pm_domain;
 
 #ifdef CONFIG_ENERGY_MODEL
-	struct em_perf_domain	*em_pd;
+	struct em_perf_domain __rcu *em_pd;
 #endif
 
 #ifdef CONFIG_PINCTRL
diff --git a/include/linux/energy_model.h b/include/linux/energy_model.h
index e7497f804644..6c6152387c93 100644
--- a/include/linux/energy_model.h
+++ b/include/linux/energy_model.h
@@ -170,6 +170,22 @@ struct em_data_callback {
 
 struct em_perf_domain *em_cpu_get(int cpu);
 struct em_perf_domain *em_pd_get(struct device *dev);
+
+/**
+ * em_pd_get_check() - Return the performance domain for a device
+ * @dev : Device to find the performance domain for
+ * @c : lockdep expression describing how the caller serializes
+ *	against em_dev_unregister_perf_domain()
+ *
+ * Like em_pd_get(), for callers that cannot rely on RCU and instead
+ * serialize against unregistration of the perf domain by other means.
+ */
+#define em_pd_get_check(dev, c)					\
+({								\
+	struct device *__dev = (dev);				\
+	IS_ERR_OR_NULL(__dev) ? NULL :				\
+		rcu_dereference_check(__dev->em_pd, c);		\
+})
 int em_dev_update_perf_domain(struct device *dev,
 			      struct em_perf_table *new_table);
 int em_dev_register_perf_domain(struct device *dev, unsigned int nr_states,
@@ -375,6 +391,7 @@ static inline struct em_perf_domain *em_pd_get(struct device *dev)
 {
 	return NULL;
 }
+#define em_pd_get_check(dev, c)	((struct em_perf_domain *)NULL)
 static inline unsigned long em_cpu_energy(struct em_perf_domain *pd,
 			unsigned long max_util, unsigned long sum_util,
 			unsigned long allowed_cpu_cap)
diff --git a/kernel/power/energy_model.c b/kernel/power/energy_model.c
index 5b055cbe5341..68492266f580 100644
--- a/kernel/power/energy_model.c
+++ b/kernel/power/energy_model.c
@@ -140,30 +140,33 @@ DEFINE_SHOW_ATTRIBUTE(em_debug_id);
 
 static void em_debug_create_pd(struct device *dev)
 {
+	struct em_perf_domain *pd;
 	struct em_dbg_info *em_dbg;
 	struct dentry *d;
 	int i;
 
+	pd = rcu_dereference_protected(dev->em_pd,
+				       lockdep_is_held(&em_pd_mutex));
+
 	/* Create the directory of the performance domain */
 	d = debugfs_create_dir(dev_name(dev), rootdir);
 
 	if (_is_cpu_device(dev))
-		debugfs_create_file("cpus", 0444, d, dev->em_pd->cpus,
+		debugfs_create_file("cpus", 0444, d, pd->cpus,
 				    &em_debug_cpus_fops);
 
-	debugfs_create_file("flags", 0444, d, dev->em_pd,
-			    &em_debug_flags_fops);
+	debugfs_create_file("flags", 0444, d, pd, &em_debug_flags_fops);
 
-	debugfs_create_file("id", 0444, d, dev->em_pd, &em_debug_id_fops);
+	debugfs_create_file("id", 0444, d, pd, &em_debug_id_fops);
 
-	em_dbg = devm_kcalloc(dev, dev->em_pd->nr_perf_states,
+	em_dbg = devm_kcalloc(dev, pd->nr_perf_states,
 			      sizeof(*em_dbg), GFP_KERNEL);
 	if (!em_dbg)
 		return;
 
 	/* Create a sub-directory for each performance state */
-	for (i = 0; i < dev->em_pd->nr_perf_states; i++)
-		em_debug_create_ps(dev->em_pd, em_dbg, i, d);
+	for (i = 0; i < pd->nr_perf_states; i++)
+		em_debug_create_ps(pd, em_dbg, i, d);
 
 }
 
@@ -335,11 +338,12 @@ int em_dev_update_perf_domain(struct device *dev,
 	/* Serialize update/unregister or concurrent updates */
 	mutex_lock(&em_pd_mutex);
 
-	if (!dev->em_pd) {
+	pd = rcu_dereference_protected(dev->em_pd,
+				       lockdep_is_held(&em_pd_mutex));
+	if (!pd) {
 		mutex_unlock(&em_pd_mutex);
 		return -EINVAL;
 	}
-	pd = dev->em_pd;
 
 	kref_get(&new_table->kref);
 
@@ -468,10 +472,10 @@ static int em_create_pd(struct device *dev, int nr_states,
 	if (_is_cpu_device(dev))
 		for_each_cpu(cpu, cpus) {
 			cpu_dev = get_cpu_device(cpu);
-			cpu_dev->em_pd = pd;
+			rcu_assign_pointer(cpu_dev->em_pd, pd);
 		}
 
-	dev->em_pd = pd;
+	rcu_assign_pointer(dev->em_pd, pd);
 
 	return 0;
 
@@ -486,7 +490,8 @@ static int em_create_pd(struct device *dev, int nr_states,
 static void
 em_cpufreq_update_efficiencies(struct device *dev, struct em_perf_state *table)
 {
-	struct em_perf_domain *pd = dev->em_pd;
+	struct em_perf_domain *pd = rcu_dereference_protected(dev->em_pd,
+						lockdep_is_held(&em_pd_mutex));
 	struct cpufreq_policy *policy;
 	int found = 0;
 	int i, cpu;
@@ -533,13 +538,19 @@ em_cpufreq_update_efficiencies(struct device *dev, struct em_perf_state *table)
  *
  * Returns the performance domain to which @dev belongs, or NULL if it doesn't
  * exist.
+ *
+ * Must be called, and the returned perf domain used, within the same RCU
+ * read-side critical section. Callers that instead serialize against
+ * em_dev_unregister_perf_domain() by other means must use
+ * em_pd_get_check() with a lockdep expression describing that
+ * serialization.
  */
 struct em_perf_domain *em_pd_get(struct device *dev)
 {
 	if (IS_ERR_OR_NULL(dev))
 		return NULL;
 
-	return dev->em_pd;
+	return rcu_dereference(dev->em_pd);
 }
 EXPORT_SYMBOL_GPL(em_pd_get);
 
@@ -548,7 +559,9 @@ EXPORT_SYMBOL_GPL(em_pd_get);
  * @cpu : CPU to find the performance domain for
  *
  * Returns the performance domain to which @cpu belongs, or NULL if it doesn't
- * exist.
+ * exist. No serialization against unregistration is required: CPU perf
+ * domains are never unregistered (em_dev_unregister_perf_domain() returns
+ * early for CPU devices).
  */
 struct em_perf_domain *em_cpu_get(int cpu)
 {
@@ -558,7 +571,8 @@ struct em_perf_domain *em_cpu_get(int cpu)
 	if (!cpu_dev)
 		return NULL;
 
-	return em_pd_get(cpu_dev);
+	/* CPU perf domains are never unregistered. */
+	return em_pd_get_check(cpu_dev, true);
 }
 EXPORT_SYMBOL_GPL(em_cpu_get);
 
@@ -614,6 +628,7 @@ int em_dev_register_pd_no_update(struct device *dev, unsigned int nr_states,
 				 const cpumask_t *cpus, bool microwatts)
 {
 	struct em_perf_table *em_table;
+	struct em_perf_domain *pd;
 	unsigned long cap, prev_cap = 0;
 	unsigned long flags = 0;
 	int cpu, ret;
@@ -627,7 +642,7 @@ int em_dev_register_pd_no_update(struct device *dev, unsigned int nr_states,
 	 */
 	mutex_lock(&em_pd_mutex);
 
-	if (dev->em_pd) {
+	if (rcu_access_pointer(dev->em_pd)) {
 		ret = -EEXIST;
 		goto unlock;
 	}
@@ -682,11 +697,13 @@ int em_dev_register_pd_no_update(struct device *dev, unsigned int nr_states,
 	if (ret)
 		goto unlock;
 
-	dev->em_pd->flags |= flags;
-	dev->em_pd->min_perf_state = 0;
-	dev->em_pd->max_perf_state = nr_states - 1;
+	pd = rcu_dereference_protected(dev->em_pd,
+				       lockdep_is_held(&em_pd_mutex));
+	pd->flags |= flags;
+	pd->min_perf_state = 0;
+	pd->max_perf_state = nr_states - 1;
 
-	em_table = rcu_dereference_protected(dev->em_pd->em_table,
+	em_table = rcu_dereference_protected(pd->em_table,
 					     lockdep_is_held(&em_pd_mutex));
 	em_cpufreq_update_efficiencies(dev, em_table->state);
 
@@ -699,10 +716,10 @@ int em_dev_register_pd_no_update(struct device *dev, unsigned int nr_states,
 		return ret;
 
 	mutex_lock(&em_pd_list_mutex);
-	list_add_tail(&dev->em_pd->node, &em_pd_list);
+	list_add_tail(&pd->node, &em_pd_list);
 	mutex_unlock(&em_pd_list_mutex);
 
-	em_notify_pd_created(dev->em_pd);
+	em_notify_pd_created(pd);
 
 	return 0;
 }
@@ -716,17 +733,21 @@ EXPORT_SYMBOL_GPL(em_dev_register_pd_no_update);
  */
 void em_dev_unregister_perf_domain(struct device *dev)
 {
-	if (IS_ERR_OR_NULL(dev) || !dev->em_pd)
+	struct em_perf_domain *pd;
+
+	/* Unregistration is the write side; callers must serialize it. */
+	pd = em_pd_get_check(dev, true);
+	if (!pd)
 		return;
 
 	if (_is_cpu_device(dev))
 		return;
 
 	mutex_lock(&em_pd_list_mutex);
-	list_del_init(&dev->em_pd->node);
+	list_del_init(&pd->node);
 	mutex_unlock(&em_pd_list_mutex);
 
-	em_notify_pd_deleted(dev->em_pd);
+	em_notify_pd_deleted(pd);
 
 	/*
 	 * The mutex separates all register/unregister requests and protects
@@ -736,14 +757,17 @@ void em_dev_unregister_perf_domain(struct device *dev)
 	mutex_lock(&em_pd_mutex);
 	em_debug_remove_pd(dev);
 
-	em_table_free(rcu_dereference_protected(dev->em_pd->em_table,
-						lockdep_is_held(&em_pd_mutex)));
+	/* Hide the perf domain from new lockless readers. */
+	RCU_INIT_POINTER(dev->em_pd, NULL);
 
-	ida_free(&em_pd_ida, dev->em_pd->id);
+	em_table_free(rcu_dereference_protected(pd->em_table,
+						lockdep_is_held(&em_pd_mutex)));
 
-	kfree(dev->em_pd);
-	dev->em_pd = NULL;
+	ida_free(&em_pd_ida, pd->id);
 	mutex_unlock(&em_pd_mutex);
+
+	/* Readers in an RCU read-side critical section may still use pd. */
+	kfree_rcu_mightsleep(pd);
 }
 EXPORT_SYMBOL_GPL(em_dev_unregister_perf_domain);
 
@@ -843,7 +867,8 @@ void em_adjust_cpu_capacity(unsigned int cpu)
 	struct device *dev = get_cpu_device(cpu);
 	struct em_perf_domain *pd;
 
-	pd = em_pd_get(dev);
+	/* CPU perf domains are never unregistered. */
+	pd = em_pd_get_check(dev, true);
 	if (pd)
 		em_adjust_new_capacity(cpu, dev, pd);
 }
@@ -875,7 +900,8 @@ static void em_check_capacity_update(void)
 		cpufreq_cpu_put(policy);
 
 		dev = get_cpu_device(cpu);
-		pd = em_pd_get(dev);
+		/* CPU perf domains are never unregistered. */
+		pd = em_pd_get_check(dev, true);
 		if (!pd || em_is_artificial(pd))
 			continue;
 
@@ -914,7 +940,8 @@ int em_dev_update_chip_binning(struct device *dev)
 	if (IS_ERR_OR_NULL(dev))
 		return -EINVAL;
 
-	pd = em_pd_get(dev);
+	/* The caller owns @dev and serializes against unregistration. */
+	pd = em_pd_get_check(dev, true);
 	if (!pd) {
 		dev_warn(dev, "Couldn't find Energy Model\n");
 		return -EINVAL;
-- 
2.50.1 (Apple Git-155)
```

```patch:0003-devfreq-cooling-doc.patch
From cf3d3c924b53b2e2369effada1d83293b1bd2102 Mon Sep 17 00:00:00 2001
From: Sivan Zohar-Kotzer <sivany32@gmail.com>
Date: Thu, 11 Jun 2026 23:07:51 +0300
Subject: [PATCH 3/3] thermal: devfreq_cooling: Document why caching the perf
 domain is safe

em_pd_get() returns a raw pointer whose lifetime ends at
em_dev_unregister_perf_domain(), so caching it looks suspicious next
to the lockless readers that were recently converted to fetch the perf
domain under rcu_read_lock().

Spell out why this caching is nevertheless safe: the only path that
unregisters a device EM is devfreq_cooling_unregister(), and it
unregisters the cooling device - synchronizing against any in-flight
cooling callbacks - before unregistering the EM, so no callback can
observe a freed perf domain.

Signed-off-by: Sivan Zohar-Kotzer <sivany32@gmail.com>
Co-developed-by: Elazar Leibovich <elazarl@gmail.com>
Signed-off-by: Elazar Leibovich <elazarl@gmail.com>
---
 drivers/thermal/devfreq_cooling.c | 6 ++++++
 1 file changed, 6 insertions(+)

diff --git a/drivers/thermal/devfreq_cooling.c b/drivers/thermal/devfreq_cooling.c
index 41e04cbcace2..6766a6a22e03 100644
--- a/drivers/thermal/devfreq_cooling.c
+++ b/drivers/thermal/devfreq_cooling.c
@@ -416,6 +416,12 @@ of_devfreq_cooling_register_power(struct device_node *np, struct devfreq *df,
 	/* Registration-time probe; the cooling device is not live yet. */
 	em = em_pd_get_check(dev, true);
 	if (em && !em_is_artificial(em)) {
+		/*
+		 * Caching the perf domain is safe: the EM can only go away
+		 * via devfreq_cooling_unregister(), which unregisters the
+		 * cooling device (synchronizing against any in-flight
+		 * cooling callbacks) before unregistering the EM.
+		 */
 		dfc->em_pd = em;
 		ops->get_requested_power =
 			devfreq_cooling_get_requested_power;
-- 
2.50.1 (Apple Git-155)
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