---
commit: v6.18-rc6
patch-compare: true
search-dmesg: BUG: kernel NULL pointer dereference
search-dmesg: generic_hwtstamp_ioctl_lower
summary: NULL deref in generic_hwtstamp_ioctl_lower() via ethtool TSCONFIG_GET on a VLAN
tag: net
tag: null-deref
---

# NULL-deref in `generic_hwtstamp_ioctl_lower()` via ethtool tsconfig netlink

Reproducer for the bug fixed by upstream commit `f796a8dec9be` ("net: core:
prevent NULL deref in `generic_hwtstamp_ioctl_lower()`").

## The bug

`6e9e2eed4f39` ("net: ethtool: Add support for tsconfig command") added a netlink
tsconfig GET path. For a VLAN device whose lower device has **no**
`ndo_hwtstamp_get` (e.g. virtio-net), the call chain

```
tsconfig_prepare_data() -> dev_get_hwtstamp_phylib() -> vlan_hwtstamp_get()
  -> generic_hwtstamp_get_lower() -> generic_hwtstamp_ioctl_lower()
```

reaches `generic_hwtstamp_ioctl_lower()` with `kernel_cfg->ifr == NULL` (the
netlink path never sets `ifr`, unlike the legacy ioctl path). That function
dereferences `kernel_cfg->ifr` unconditionally:

```c
ifrr.ifr_ifru = kernel_cfg->ifr->ifr_ifru;   /* NULL deref -> oops */
```

`f796a8dec9be` adds a `if (!kernel_cfg->ifr) return -EINVAL;` guard.

## Why this kernel

The bug landed in `v6.18-rc3` and the fix landed in `v6.18-rc7`, so `v6.18-rc6`
is the last release that is buggy **and** fix-free. `patch-compare` runs two
kernels in parallel: a **baseline** at `v6.18-rc6` (must oops) and a **patched**
variant with the fix applied (must return `-EINVAL` cleanly).

## Trigger

A small C program sends an `ETHTOOL_MSG_TSCONFIG_GET` generic-netlink *doit* for
a VLAN interface created on top of the guest's virtio-net device (`eth0`). The
genl doit runs `tsconfig_prepare_data` synchronously in the sender's context
under `rtnl_lock`, so the NULL deref oopses the `tsconfig_get` process itself
(killed by SIGSEGV during `sendto`); the oops is logged to the serial console.

On the fixed kernel the new guard returns `-EINVAL` before the deref, so the
netlink request completes with a clean error reply and the process exits 0.

```user:tsconfig_get.c
/* Send an ETHTOOL_MSG_TSCONFIG_GET (doit) for a named interface and print the
 * netlink reply. On a buggy kernel the generic_hwtstamp_ioctl_lower() NULL-deref
 * oops fires during sendto() and this process is killed by SIGSEGV before a
 * reply arrives. On a fixed kernel we get a clean -EINVAL error reply. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <sys/socket.h>
#include <linux/netlink.h>
#include <linux/genetlink.h>

/* ethtool netlink constants from include/uapi/linux/ethtool_netlink_generated.h
 * (too new for the build container's system headers, so hardcoded). */
#define ETHTOOL_GENL_NAME         "ethtool"
#define ETHTOOL_MSG_TSCONFIG_GET  46
#define ETHTOOL_A_TSCONFIG_HEADER 1
#define ETHTOOL_A_HEADER_DEV_NAME 2

static unsigned int seq;

static int nl_open(void)
{
	int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_GENERIC);
	if (fd < 0) { perror("socket"); return -1; }
	struct sockaddr_nl sa = { .nl_family = AF_NETLINK };
	if (bind(fd, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
		perror("bind"); close(fd); return -1;
	}
	return fd;
}

/* append an nlattr (type, data, len) at *p and advance *p past it */
static void put_attr(void **p, unsigned short type, const void *data, size_t len)
{
	struct nlattr *na = (struct nlattr *)*p;
	na->nla_type = type;
	na->nla_len = (unsigned short)(NLA_HDRLEN + len);
	memcpy((char *)na + NLA_HDRLEN, data, len);
	*p = (char *)*p + NLA_ALIGN(na->nla_len);
}

/* resolve a genl family id by name; returns 0 on failure */
static unsigned int resolve_family(int fd, const char *name)
{
	char buf[256];
	memset(buf, 0, sizeof(buf));
	struct nlmsghdr *nh = (struct nlmsghdr *)buf;
	nh->nlmsg_type = GENL_ID_CTRL;
	nh->nlmsg_flags = NLM_F_REQUEST;
	nh->nlmsg_seq = ++seq;
	struct genlmsghdr *gh = (struct genlmsghdr *)NLMSG_DATA(nh);
	gh->cmd = CTRL_CMD_GETFAMILY;
	void *p = (char *)gh + GENL_HDRLEN;
	put_attr(&p, CTRL_ATTR_FAMILY_NAME, name, strlen(name) + 1);
	nh->nlmsg_len = (unsigned)((char *)p - buf);

	struct sockaddr_nl dst = { .nl_family = AF_NETLINK };
	if (sendto(fd, buf, nh->nlmsg_len, 0, (struct sockaddr *)&dst, sizeof(dst)) < 0) {
		perror("sendto(getfamily)"); return 0;
	}

	char rbuf[4096];
	ssize_t n = recv(fd, rbuf, sizeof(rbuf), 0);
	if (n < 0) { perror("recv(getfamily)"); return 0; }
	struct nlmsghdr *rnh = (struct nlmsghdr *)rbuf;
	if (!NLMSG_OK(rnh, (unsigned)n)) { fprintf(stderr, "getfamily: bad reply\n"); return 0; }
	if (rnh->nlmsg_type == NLMSG_ERROR) {
		fprintf(stderr, "getfamily: error %d\n", ((struct nlmsgerr *)NLMSG_DATA(rnh))->error);
		return 0;
	}
	struct genlmsghdr *rgh = (struct genlmsghdr *)NLMSG_DATA(rnh);
	int rlen = (int)(rnh->nlmsg_len - NLMSG_LENGTH(GENL_HDRLEN));
	for (struct nlattr *na = (struct nlattr *)((char *)rgh + GENL_HDRLEN);
	     rlen >= (int)NLA_HDRLEN; ) {
		if (na->nla_type == CTRL_ATTR_FAMILY_ID) {
			unsigned int id;
			memcpy(&id, (char *)na + NLA_HDRLEN, sizeof(id));
			return id;
		}
		int alen = (int)NLA_ALIGN(na->nla_len);
		if (alen <= 0) break;
		rlen -= alen;
		na = (struct nlattr *)((char *)na + alen);
	}
	fprintf(stderr, "getfamily: no CTRL_ATTR_FAMILY_ID\n");
	return 0;
}

int main(int argc, char **argv)
{
	if (argc < 2) { fprintf(stderr, "usage: %s <ifname>\n", argv[0]); return 2; }
	const char *ifname = argv[1];

	int fd = nl_open();
	if (fd < 0) return 2;

	unsigned int fam = resolve_family(fd, ETHTOOL_GENL_NAME);
	if (!fam) { fprintf(stderr, "could not resolve ethtool genl family\n"); close(fd); return 2; }
	printf("ethtool genl family id: %u\n", fam);

	/* Build ETHTOOL_MSG_TSCONFIG_GET request:
	 *   [nlmsghdr][genlmsghdr cmd=46]{ ETHTOOL_A_TSCONFIG_HEADER(1) {
	 *     ETHTOOL_A_HEADER_DEV_NAME(2) = ifname } } */
	char buf[512];
	memset(buf, 0, sizeof(buf));
	struct nlmsghdr *nh = (struct nlmsghdr *)buf;
	nh->nlmsg_type = fam;
	nh->nlmsg_flags = NLM_F_REQUEST;
	nh->nlmsg_seq = ++seq;
	struct genlmsghdr *gh = (struct genlmsghdr *)NLMSG_DATA(nh);
	gh->cmd = ETHTOOL_MSG_TSCONFIG_GET;

	struct nlattr *outer = (struct nlattr *)((char *)gh + GENL_HDRLEN);
	outer->nla_type = ETHTOOL_A_TSCONFIG_HEADER | NLA_F_NESTED;
	void *inner = (char *)outer + NLA_HDRLEN;
	put_attr(&inner, ETHTOOL_A_HEADER_DEV_NAME, ifname, strlen(ifname) + 1);
	outer->nla_len = (unsigned short)((char *)inner - (char *)outer);
	nh->nlmsg_len = (unsigned)((char *)outer + NLA_ALIGN(outer->nla_len) - buf);

	printf("sending ETHTOOL_MSG_TSCONFIG_GET for %s ...\n", ifname);
	fflush(stdout);

	struct sockaddr_nl dst = { .nl_family = AF_NETLINK };
	/* On the buggy kernel the doit runs synchronously during sendto and the
	 * NULL deref oops kills this process (SIGSEGV) right here — we never reach
	 * recv(). On the fixed kernel sendto returns 0 and we get -EINVAL below. */
	if (sendto(fd, buf, nh->nlmsg_len, 0, (struct sockaddr *)&dst, sizeof(dst)) < 0) {
		perror("sendto(tsconfig_get)"); close(fd); return 2;
	}

	char rbuf[4096];
	ssize_t n = recv(fd, rbuf, sizeof(rbuf), 0);
	if (n < 0) { perror("recv(tsconfig_get)"); close(fd); return 2; }
	struct nlmsghdr *rnh = (struct nlmsghdr *)rbuf;
	if (rnh->nlmsg_type == NLMSG_ERROR) {
		int e = ((struct nlmsgerr *)NLMSG_DATA(rnh))->error;
		printf("tsconfig_get: netlink error %d (clean -EINVAL => fixed kernel)\n", e);
		close(fd);
		return 0;
	}
	printf("tsconfig_get: got reply type %u len %u\n", rnh->nlmsg_type, rnh->nlmsg_len);
	close(fd);
	return 0;
}
```

```patch:fix.patch
diff --git a/net/core/dev_ioctl.c b/net/core/dev_ioctl.c
index ad54b12d4b4c..8bb71a10dba0 100644
--- a/net/core/dev_ioctl.c
+++ b/net/core/dev_ioctl.c
@@ -443,6 +443,9 @@ static int generic_hwtstamp_ioctl_lower(struct net_device *dev, int cmd,
 	struct ifreq ifrr;
 	int err;
 
+	if (!kernel_cfg->ifr)
+		return -EINVAL;
+
 	strscpy_pad(ifrr.ifr_name, dev->name, IFNAMSIZ);
 	ifrr.ifr_ifru = kernel_cfg->ifr->ifr_ifru;
 
```

```kconf:extra.config
# VLAN 802.1Q must be built-in (=y): the custom kernel's modules are not
# installed in the guest cloud image's /lib/modules, so a module (=m) would
# not be loadable. Built-in lets `ip link add ... type vlan` work directly.
CONFIG_VLAN_8021Q=y
# ethtool netlink is default-y with CONFIG_NET, but pin it explicitly.
CONFIG_ETHTOOL_NETLINK=y
```

```init:init.sh
#!/bin/bash
# Reproducer for: net: core: NULL deref in generic_hwtstamp_ioctl_lower()
# A VLAN over virtio-net (no ndo_hwtstamp_get) + an ethtool TSCONFIG_GET netlink
# doit hits the legacy ioctl fallback in generic_hwtstamp_get_lower(), which
# dereferences kernel_cfg->ifr == NULL -> kernel oops.

# Find the guest's virtio-net NIC (systemd udev renames eth0 -> enp0s1, so
# don't hardcode the name). It's the first non-loopback netdev with a device.
BASE=""
for n in /sys/class/net/*; do
    n=${n##*/}
    [ "$n" = "lo" ] && continue
    [ -e "/sys/class/net/$n/device" ] && { BASE="$n"; break; }
done
[ -z "$BASE" ] && { echo "FAIL: no virtio-net interface found"; exit 1; }
IF="$BASE.100"

# Bring up the NIC and create a VLAN on top of it.
sudo ip link set "$BASE" up 2>/dev/null || true
if ! sudo ip link add link "$BASE" name "$IF" type vlan id 100; then
    echo "FAIL: could not create VLAN $IF (CONFIG_VLAN_8021Q?)"; exit 1
fi
sudo ip link set "$IF" up
echo "created VLAN $IF over $BASE (virtio-net)"

# Make an oops fatal: a NULL deref under rtnl_lock with irqs disabled wedges the
# guest, so panic_on_oops=1 turns the oops into a reboot (PANIC_TIMEOUT=3 in the
# base config), letting the runner see the guest die fast instead of hanging.
sudo sysctl -qw kernel.panic_on_oops=1

# Trigger the ethtool TSCONFIG_GET netlink doit. On the buggy kernel the NULL
# deref in generic_hwtstamp_ioctl_lower() fires during sendto -> oops -> panic
# -> reboot, so this process is killed and the SSH run drops (baseline fails).
# On the fixed kernel sendto returns 0 and recv() gets a clean -EINVAL reply.
./tsconfig_get "$IF"
rc=$?
echo "=== tsconfig_get exit code: $rc ==="

# We only reach here on the fixed kernel (the buggy one panicked above).
# Sanity-check dmesg for an oops anyway; exit 0 if none.
if sudo dmesg 2>/dev/null | grep -qiE "BUG: kernel NULL pointer dereference|Oops:"; then
    echo "REPRODUCED: kernel oops in dmesg"
    exit 1
fi
echo "no oops detected (kernel handled the request cleanly)"
exit $rc
```
