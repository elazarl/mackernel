---
regex-dmesg: (BUG: KASAN:|invalid opcode|general protection|Oops)
---

# helene double-free reproducer (KASAN)

Reproduces the bug fixed by
[media: dvb-frontends: helene: Fix double free on release](https://lore.kernel.org/lkml/20260701083223.90137-1-pengcan@kylinos.cn/).

`helene_probe()` (the i2c path) allocates `priv` with `devm_kzalloc()` — so **devres
owns it** and frees it at device teardown — but it also installs `helene_tuner_ops`,
whose `.release` callback `kfree()`s the same `fe->tuner_priv`. Two owners → one gets
freed twice.

This module models that "two owners, one object" bug on a throwaway platform device:
`priv` is a real slab object owned by **devres** (registered via `devm_add_action_or_reset`,
which `kfree()`s it at device teardown), and the still-installed release path *also*
`kfree()`s it. The second free is a genuine double-free — KASAN reports it and continues
(no slab corruption / guest hang), so the module returns and the check below runs.

```module:helene_repro.c
// SPDX-License-Identifier: GPL-2.0
#include <linux/module.h>
#include <linux/platform_device.h>
#include <linux/slab.h>

static void *priv;                 // fe->tuner_priv analog: a real kmalloc object

static void priv_free(void *p)     // devres owner's free (helene: the devm allocation)
{
	kfree(p);
}

static int mk_probe(struct platform_device *pdev)
{
	priv = kmalloc(128, GFP_KERNEL);
	if (!priv)
		return -ENOMEM;
	// devres now owns priv -- it will kfree(priv) at device teardown.
	return devm_add_action_or_reset(&pdev->dev, priv_free, priv);
}

static struct platform_driver mk_drv = {
	.probe = mk_probe,
	.driver = { .name = "helene_repro" },
};

static struct platform_device *mk_dev;

static int __init mk_init(void)
{
	int ret = platform_driver_register(&mk_drv);
	if (ret)
		return ret;

	mk_dev = platform_device_register_simple("helene_repro", -1, NULL, 0);
	if (IS_ERR(mk_dev)) {
		platform_driver_unregister(&mk_drv);
		return PTR_ERR(mk_dev);
	}

	// helene_release(): the still-installed tuner_ops.release kfree()s priv -- free #1.
	pr_info("helene_repro: release: kfree(priv) -- free #1\n");
	kfree(priv);

	// device teardown: devres runs priv_free(priv) -> kfree(priv) again -- free #2.
	pr_info("helene_repro: teardown: devres frees priv again -- free #2 (double free)\n");
	platform_device_unregister(mk_dev);

	platform_driver_unregister(&mk_drv);
	pr_info("helene_repro: done (KASAN should have reported a double-free above)\n");
	return 0;
}

static void __exit mk_exit(void) { }

module_init(mk_init);
module_exit(mk_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("helene double-free-on-release reproducer");
```

```kconf:kasan.config
# KASAN needs the full SLUB allocator; tinyconfig's SLUB_TINY is incompatible and
# would make olddefconfig silently drop CONFIG_KASAN. (configure-kernel.py --kasan
# disables it after the merge; a raw kconf fragment must do it itself.)
# CONFIG_SLUB_TINY is not set
CONFIG_KASAN=y
CONFIG_KASAN_GENERIC=y
CONFIG_STACKTRACE=y
```

```init:init.sh
#!/bin/bash
# The module is auto-insmod'd before this runs; the double-free happens in its init.
# With KASAN active the second kfree is caught as "BUG: KASAN: double-free" before the
# object is reused; if KASAN were absent it would instead corrupt a reused object and
# trip a later BUG/Oops -- match either so a real crash is never reported as "clean".
pat="BUG: KASAN:|invalid opcode|general protection|Oops|kernel BUG"
if sudo dmesg | grep -qE "$pat"; then
	echo "REPRODUCED: double-free on release caught by the kernel"
	sudo dmesg | grep -E -A25 "$pat" | head -45
	exit 1     # non-zero == reproduced (per reproducer spec)
fi
echo "NOT reproduced: no KASAN report / crash in dmesg"
exit 0
```
