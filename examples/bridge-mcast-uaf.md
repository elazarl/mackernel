---
commit: v6.16-rc3
patch-compare: true
search-dmesg: BUG: KASAN: slab-use-after-free
search-dmesg: br_multicast_add_router
summary: UAF in the bridge multicast router-port list after a per-VLAN port is deleted
tag: net
tag: bridge
tag: uaf
tag: kasan
---

# Use-after-free in bridge multicast router port configuration

Reproducer for the bug fixed by upstream commit `7544f3f5b0b5` ("bridge: mcast:
Fix use-after-free during router port configuration").

## The bug

Commit `4b30ae9adb04` ("net: bridge: mcast: re-implement
`br_multicast_{enable,disable}_port` functions") changed multicast disablement so
that, when per-VLAN multicast snooping is enabled, the per-port multicast context
is disabled — but the port is **not removed from the global router port list**.

The port can then be re-added to the global list by toggling `mcast_router`:
`br_multicast_add_router()` traverses the stale list entry, which references the
freed per-port multicast context → **slab-use-after-free** (read of size 8).

syzbot splat:
```
BUG: KASAN: slab-use-after-free in br_multicast_add_router.part.0+0x378/0x560
Read of size 8 at addr ... by task bridge/391
Call Trace:
 br_multicast_add_router.part.0+0x378/0x560
 br_multicast_set_port_router+0x6f9/0xac0
 br_vlan_process_options+0x8b6/0x1430
 rtnetlink_rcv_msg+0x2f7/0xb70
```

`7544f3f5b0b5` fixes it by removing the port from the global router port list in
`br_multicast_port_ctx_deinit()` (under `multicast_lock`) when per-VLAN snooping
disables the per-port context.

## Why this kernel

The bug landed in `v6.16-rc1` (`4b30ae9adb04`, 2025-04-23) and the fix landed in
`v6.16-rc4` (`7544f3f5b0b5`, 2025-06-23). `v6.16-rc3` is the last release that is
buggy **and** fix-free. `patch-compare` runs two kernels in parallel: a
**baseline** at `v6.16-rc3` (UAF detected by KASAN) and a **patched** variant with
the fix applied (no UAF).

## Trigger

Pure shell — the exact sequence from the fix commit message:

```bash
ip link add br1 up type bridge vlan_filtering 1 mcast_snooping 1
ip link add dummy1 up master br1 type dummy
ip link set dev dummy1 type bridge_slave mcast_router 2
ip link set dev br1 type bridge mcast_vlan_snooping 1   # disables per-port mcast ctx, but port stays in global list (bug)
ip link set dev dummy1 type bridge_slave mcast_router 0
ip link set dev dummy1 type bridge_slave mcast_router 2  # traverses stale list entry -> UAF
```

`CONFIG_KASAN=y` is required: the UAF is a read of freed slab memory that does
**not** crash without a sanitizer (the page is still mapped). KASAN quarantines
freed objects and reports the access.

```patch:fix.patch
diff --git a/net/bridge/br_multicast.c b/net/bridge/br_multicast.c
index 0224ef3dfec0..1377f31b719c 100644
--- a/net/bridge/br_multicast.c
+++ b/net/bridge/br_multicast.c
@@ -2015,10 +2015,19 @@ void br_multicast_port_ctx_init(struct net_bridge_port *port,
 
 void br_multicast_port_ctx_deinit(struct net_bridge_mcast_port *pmctx)
 {
+	struct net_bridge *br = pmctx->port->br;
+	bool del = false;
+
 #if IS_ENABLED(CONFIG_IPV6)
 	timer_delete_sync(&pmctx->ip6_mc_router_timer);
 #endif
 	timer_delete_sync(&pmctx->ip4_mc_router_timer);
+
+	spin_lock_bh(&br->multicast_lock);
+	del |= br_ip6_multicast_rport_del(pmctx);
+	del |= br_ip4_multicast_rport_del(pmctx);
+	br_multicast_rport_del_notify(pmctx, del);
+	spin_unlock_bh(&br->multicast_lock);
 }
 
 int br_multicast_add_port(struct net_bridge_port *port)
```

```kconf:extra.config
# Bridge + VLAN filtering + multicast snooping — all built-in (=y) because the
# custom kernel's modules are not installed in the guest cloud image.
CONFIG_BRIDGE=y
CONFIG_BRIDGE_IGMP_SNOOPING=y
CONFIG_BRIDGE_VLAN_FILTERING=y
CONFIG_VLAN_8021Q=y
CONFIG_DUMMY=y

# KASAN: the UAF is a read of freed slab memory that doesn't crash without a
# sanitizer. KASAN quarantines freed objects and reports the stale access.
# tinyconfig sets SLUB_TINY=y, which KASAN depends on being disabled.
# CONFIG_SLUB_TINY is not set
CONFIG_KASAN=y
CONFIG_KASAN_GENERIC=y
CONFIG_STACKTRACE=y
```

```init:init.sh
#!/bin/bash
# Reproducer for: bridge: mcast: use-after-free during router port configuration
#
# Bug: when per-VLAN mcast snooping is enabled, br_multicast_port_ctx_deinit()
# (called on port deletion) only deletes timers — it does NOT remove the port
# from the global router port list.  After the port (and its per-VLAN structs)
# are freed, the stale list entry references freed slab memory.  A subsequent
# list traversal (adding another router port) reads the freed entry → UAF.
#
# Fix: 7544f3f5b0b5 adds rport_del + notify in br_multicast_port_ctx_deinit().

set -e

# 1. Create a bridge with VLAN filtering + multicast snooping.
sudo ip link add name br1 up type bridge vlan_filtering 1 mcast_snooping 1

# 2. Add a dummy port and put it in multicast-router mode.
sudo ip link add name dummy1 up master br1 type dummy
sudo ip link set dev dummy1 type bridge_slave mcast_router 2

# 3. Enable per-VLAN multicast snooping. This disables the per-port mcast
#    context (removing it from the router list) and enables per-VLAN contexts.
sudo ip link set dev br1 type bridge mcast_vlan_snooping 1

# 4. Re-add the port to the global router list via the per-VLAN context.
sudo ip link set dev dummy1 type bridge_slave mcast_router 0 || true
sudo ip link set dev dummy1 type bridge_slave mcast_router 2 || true

# 5. Delete the port. On the buggy kernel, br_multicast_port_ctx_deinit()
#    does NOT remove the per-VLAN context from the global router port list,
#    so the freed per-VLAN struct stays on the list as a stale entry.
sudo ip link del dev dummy1 || true

# 6. Add a new port and set mcast_router — this traverses the global router
#    port list and reads the stale (freed) entry → KASAN slab-use-after-free.
sudo ip link add name dummy2 up master br1 type dummy
sudo ip link set dev dummy2 type bridge_slave mcast_router 2 || true

echo "=== trigger sequence complete ==="

# Check dmesg for the KASAN use-after-free report.
if sudo dmesg 2>/dev/null | grep -q "BUG: KASAN: slab-use-after-free"; then
    echo "REPRODUCED: KASAN slab-use-after-free in dmesg"
    sudo dmesg | grep -A5 "BUG: KASAN: slab-use-after-free" | head -10
    exit 1
fi

echo "no KASAN UAF detected (kernel handled the request cleanly)"
exit 0
```
