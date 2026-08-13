// Client-side parser for reproducer bundles (see docs/reproducer-spec.md). Used to
// render a structured preview (frontmatter metadata + per-role code tabs) instead of
// the raw text. Display-only: the raw bundle is still POSTed and re-parsed server-side.

export interface BundleMeta { key: string; value: string; }
export interface BundleFile { role: string; name: string; body: string; }
export interface ParsedBundle { meta: BundleMeta[]; files: BundleFile[]; }

// Recognized metadata keys and the canonical tab order (per the spec). Roles not in
// this list still get a tab, ordered after the known ones.
const RECOGNIZED_META = ["url", "commit", "patch", "thread", "arch", "patch-compare", "thread-compare", "commit-compare", "search-dmesg", "regex-dmesg", "search-user", "regex-user", "tools", "summary", "tag"];
export const ROLE_ORDER = ["user", "module", "kconf", "patch", "init"];

const FENCE_OPEN = /^(`{3,})(.*)$/;     // ```role:filename  (or any info string)
const ROLE_INFO = /^(\w+):(.+)$/;       // role:filename
const KV = /^([A-Za-z][\w-]*):\s*(.+)$/;

export function parseBundle(text: string): ParsedBundle {
  const lines = text.split(/\r?\n/);
  const meta: BundleMeta[] = [];
  const files: BundleFile[] = [];

  let i = 0;
  let fence: string | null = null; // backtick run that opened the current fence
  let cur: BundleFile | null = null;
  let metaParsed = false;

  while (i < lines.length) {
    const line = lines[i];

    if (fence === null) {
      // Outside a code fence.
      const open = line.match(FENCE_OPEN);
      if (open) {
        fence = open[1];
        const info = open[2].trim();
        const role = info.match(ROLE_INFO);
        cur = role ? { role: role[1], name: role[2].trim(), body: "" } : null;
        i++;
        continue;
      }

      // Frontmatter: a `---` at column 0 (not indented — the spec is explicit), with
      // key:value lines, closed by another column-0 `---`. Only the first one counts.
      if (!metaParsed && line === "---") {
        const block: string[] = [];
        let j = i + 1;
        let closed = false;
        while (j < lines.length) {
          if (lines[j] === "---") { closed = true; break; }
          block.push(lines[j]);
          j++;
        }
        const nonBlank = block.filter((l) => l.trim() !== "");
        if (closed && nonBlank.length > 0 && nonBlank.every((l) => KV.test(l.trim()))) {
          for (const l of nonBlank) {
            const m = l.trim().match(KV)!;
            if (RECOGNIZED_META.includes(m[1])) meta.push({ key: m[1], value: m[2].trim() });
          }
          metaParsed = true;
          i = j + 1; // skip past the closing ---
          continue;
        }
        // Not a metadata block (e.g. a thematic break) — fall through as prose.
      }
      i++;
    } else {
      // Inside a code fence: close on a line of >= the opening number of backticks.
      if (new RegExp("^`{" + fence.length + ",}\\s*$").test(line)) {
        if (cur) files.push(cur);
        cur = null;
        fence = null;
        i++;
        continue;
      }
      if (cur) cur.body += (cur.body ? "\n" : "") + line;
      i++;
    }
  }

  files.sort((a, b) => roleRank(a.role) - roleRank(b.role));
  return { meta, files };
}

function roleRank(role: string): number {
  const idx = ROLE_ORDER.indexOf(role);
  return idx === -1 ? ROLE_ORDER.length : idx;
}

// Does a parsed bundle request a baseline-vs-patched comparison? Returns the mode
// ("patch" strips the bundle's patch:; "thread" git-ams a lore series; "commit"
// builds two commit-ishes) or null. All modes produce baseline/patched runs, so the
// UI renders either side by side. Precedence matches the runner: patch > thread > commit.
const TRUTHY = ["1", "true", "yes", "on"];
export function compareMode(parsed: ParsedBundle): "patch" | "thread" | "commit" | null {
  const get = (k: string) => parsed.meta.find((m) => m.key === k)?.value;
  const pc = get("patch-compare");
  // A patch can come from the patch: key or an inline ```patch:… fence.
  const hasPatch = !!get("patch") || parsed.files.some((f) => f.role === "patch");
  if (pc && TRUTHY.includes(pc.trim().toLowerCase()) && hasPatch) return "patch";
  if (get("thread-compare")) return "thread";
  // Two whitespace-separated commit-ishes: first baseline, second patched.
  if (get("commit-compare")?.trim().split(/\s+/).length === 2) return "commit";
  return null;
}

// Distinct roles present, in canonical order — the set of tabs to show.
export function rolesOf(parsed: ParsedBundle): string[] {
  const seen: string[] = [];
  for (const f of parsed.files) if (!seen.includes(f.role)) seen.push(f.role);
  return seen.sort((a, b) => roleRank(a) - roleRank(b));
}

// --- hardened-mode constants (must match KERNEL_URL in run-kernel.py) ----------
// The kernel is always built from Linus's tree, so a commit tree-ish maps to GitHub.
export const KERNEL_URL = "https://github.com/torvalds/linux.git";
export const githubTree = (ref: string) =>
  `https://github.com/torvalds/linux/tree/${encodeURIComponent(ref)}`;

// --- raw-mode editing helpers --------------------------------------------------

// Line indices of the column-0 `---…---` frontmatter block (outside any fence), or
// null. Reuses the fence tracking from parseBundle so `---` inside code is ignored.
function frontmatterRange(lines: string[]): { open: number; close: number } | null {
  const dashes: number[] = [];
  let fence: string | null = null;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (fence === null) {
      const m = line.match(FENCE_OPEN);
      if (m) { fence = m[1]; continue; }
      if (line === "---") dashes.push(i);
    } else if (new RegExp("^`{" + fence.length + ",}\\s*$").test(line)) {
      fence = null;
    }
  }
  return dashes.length >= 2 ? { open: dashes[0], close: dashes[1] } : null;
}

// Set a frontmatter key: update it in place if present, insert into the existing
// block, or prepend a new `---` block when the bundle has none.
export function upsertMeta(text: string, key: string, value: string): string {
  const lines = text.split(/\r?\n/);
  const kv = `${key}: ${value}`;
  const keyRe = new RegExp(`^${key}\\s*:`);
  const fm = frontmatterRange(lines);
  if (fm) {
    for (let i = fm.open + 1; i < fm.close; i++) {
      if (keyRe.test(lines[i].trim())) { lines[i] = kv; return lines.join("\n"); }
    }
    lines.splice(fm.close, 0, kv);
    return lines.join("\n");
  }
  return `---\n${kv}\n---\n\n${text}`;
}

// Boilerplate for the "add file" buttons, keyed by role.
export const BOILERPLATE: Record<string, { name: string; body: string }> = {
  user: { name: "repro.c", body: '#include <stdio.h>\n\nint main(void) {\n    return 0;\n}\n' },
  module: {
    name: "mod.c",
    body: '#include <linux/module.h>\n#include <linux/kernel.h>\n\n' +
      'static int __init mod_init(void) {\n    pr_info("mod: loaded\\n");\n    return 0;\n}\n' +
      'static void __exit mod_exit(void) { }\n\n' +
      'module_init(mod_init);\nmodule_exit(mod_exit);\nMODULE_LICENSE("GPL");\n',
  },
  kconf: { name: "extra.config", body: 'CONFIG_DEBUG_INFO=y\n' },
  init: { name: "init.sh", body: '#!/bin/bash\nset -e\n' },
};

// Ready-to-run example reproducers, loadable from the UI with one click. Each is a
// complete bundle string (see docs/reproducer-spec.md); `label`/`blurb` describe it.
export interface Example { label: string; blurb: string; bundle: string; }

export const EXAMPLES: Example[] = [
  {
    label: "greeter",
    blurb: "userspace + module + init; pinned to v6.19",
    bundle: `---
commit: v6.19
---

# greeter — minimal bundle

Builds a tiny userspace program and a kernel module, boots, loads the module,
and runs the start script. The program exits 1, which becomes the run's status.

\`\`\`user:file.c
#include <stdio.h>
#include "file.h"

int main(void) {
    printf("hello from userspace, returning %d\\n", R);
    return R;
}
\`\`\`

\`\`\`user:file.h
#define R 1
\`\`\`

\`\`\`module:greeter.c
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>

static int __init greeter_init(void)
{
    pr_info("greeter: module loaded\\n");
    return 0;
}

static void __exit greeter_exit(void)
{
    pr_info("greeter: module unloaded\\n");
}

module_init(greeter_init);
module_exit(greeter_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("mackernel bundle example module");
\`\`\`

\`\`\`init:init.sh
#!/bin/bash
set -e
echo "loaded modules:"; lsmod | grep greeter || true
echo "kernel says:"; sudo dmesg | grep greeter || true
./file
\`\`\`
`,
  },
  {
    label: "null-deref panic",
    blurb: "module dereferences NULL on load; pinned to v6.12",
    bundle: `---
commit: v6.12
---

# null-deref — a module that oopses on insmod

Pins the kernel to the v6.12 tag, then loads a module whose init deliberately
writes through a NULL pointer. The oops shows up in \`dmesg\`.

\`\`\`kconf:extra.config
CONFIG_PRINTK_CALLER=y
\`\`\`

\`\`\`module:oops.c
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>

static int __init oops_init(void)
{
    int *p = NULL;
    pr_info("oops: about to dereference NULL\\n");
    *p = 0xdead;          /* NULL pointer dereference */
    return 0;
}

static void __exit oops_exit(void) { }

module_init(oops_init);
module_exit(oops_exit);
MODULE_LICENSE("GPL");
\`\`\`

\`\`\`init:init.sh
#!/bin/bash
# Reproduced (exit non-zero) if the module's NULL deref left an oops in dmesg.
if sudo dmesg | grep -i -A20 'BUG:\\|Oops:\\|null pointer'; then
    echo "oops detected — reproduced"; exit 1
fi
echo "no oops found"; exit 0
\`\`\`
`,
  },
  {
    label: "userspace syscall",
    blurb: "userspace-only repro exercising a syscall; pinned to v6.19",
    bundle: `---
commit: v6.19
---

# uname — userspace-only bundle

Pinned to v6.19, no module — just a C program run in the guest. Reports the
running kernel release via the \`uname(2)\` syscall.

\`\`\`user:repro.c
#include <stdio.h>
#include <sys/utsname.h>

int main(void) {
    struct utsname u;
    if (uname(&u) != 0) { perror("uname"); return 1; }
    printf("kernel: %s %s\\n", u.sysname, u.release);
    return 0;
}
\`\`\`
`,
  },
  {
    label: "watch-dmesg",
    blurb: "flag a custom log line like a BUG; pinned to v6.19",
    bundle: `---
commit: v6.19
search-dmesg: MK_SENTINEL_HIT
regex-dmesg: beacon: ready #\\d+
---

# watch-dmesg — surface your own log lines like a BUG

Loads a module that prints distinctive lines to the kernel log, then asks the
runner to watch for them. Matches show up at the top of the Issues view — handy
when "the bug fired" is signalled by your own \`pr_info\`, not a \`BUG:\`. Watching
does not change pass/fail; the run status still follows the start script.

\`\`\`module:beacon.c
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>

static int __init beacon_init(void)
{
    pr_info("MK_SENTINEL_HIT: beacon module loaded\\n");
    pr_info("beacon: ready #1\\n");
    return 0;
}

static void __exit beacon_exit(void) { }

module_init(beacon_init);
module_exit(beacon_exit);
MODULE_LICENSE("GPL");
\`\`\`

\`\`\`init:init.sh
#!/bin/bash
set -e
sudo dmesg | grep -i 'sentinel\\|beacon' || true
\`\`\`
`,
  },
  {
    label: "patch-compare",
    blurb: "v6.18-rc6 baseline vs the upstream fix: a real NULL-deref reproducer",
    bundle: `---
commit: v6.18-rc6
patch-compare: true
search-dmesg: BUG: kernel NULL pointer dereference
search-dmesg: generic_hwtstamp_ioctl_lower
---

# NULL-deref in \`generic_hwtstamp_ioctl_lower()\` via ethtool tsconfig netlink

Reproducer for the bug fixed by upstream commit \`f796a8dec9be\` ("net: core:
prevent NULL deref in \`generic_hwtstamp_ioctl_lower()\`").

## The bug

\`6e9e2eed4f39\` ("net: ethtool: Add support for tsconfig command") added a netlink
tsconfig GET path. For a VLAN device whose lower device has **no**
\`ndo_hwtstamp_get\` (e.g. virtio-net), the call chain

\`\`\`
tsconfig_prepare_data() -> dev_get_hwtstamp_phylib() -> vlan_hwtstamp_get()
  -> generic_hwtstamp_get_lower() -> generic_hwtstamp_ioctl_lower()
\`\`\`

reaches \`generic_hwtstamp_ioctl_lower()\` with \`kernel_cfg->ifr == NULL\` (the
netlink path never sets \`ifr\`, unlike the legacy ioctl path). That function
dereferences \`kernel_cfg->ifr\` unconditionally:

\`\`\`c
ifrr.ifr_ifru = kernel_cfg->ifr->ifr_ifru;   /* NULL deref -> oops */
\`\`\`

\`f796a8dec9be\` adds a \`if (!kernel_cfg->ifr) return -EINVAL;\` guard.

## Why this kernel

The bug landed in \`v6.18-rc3\` and the fix landed in \`v6.18-rc7\`, so \`v6.18-rc6\`
is the last release that is buggy **and** fix-free. \`patch-compare\` runs two
kernels in parallel: a **baseline** at \`v6.18-rc6\` (must oops) and a **patched**
variant with the fix applied (must return \`-EINVAL\` cleanly).

## Trigger

A small C program sends an \`ETHTOOL_MSG_TSCONFIG_GET\` generic-netlink *doit* for
a VLAN interface created on top of the guest's virtio-net device (\`eth0\`). The
genl doit runs \`tsconfig_prepare_data\` synchronously in the sender's context
under \`rtnl_lock\`, so the NULL deref oopses the \`tsconfig_get\` process itself
(killed by SIGSEGV during \`sendto\`); the oops is logged to the serial console.

On the fixed kernel the new guard returns \`-EINVAL\` before the deref, so the
netlink request completes with a clean error reply and the process exits 0.

\`\`\`user:tsconfig_get.c
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
	if (!NLMSG_OK(rnh, (unsigned)n)) { fprintf(stderr, "getfamily: bad reply\\n"); return 0; }
	if (rnh->nlmsg_type == NLMSG_ERROR) {
		fprintf(stderr, "getfamily: error %d\\n", ((struct nlmsgerr *)NLMSG_DATA(rnh))->error);
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
	fprintf(stderr, "getfamily: no CTRL_ATTR_FAMILY_ID\\n");
	return 0;
}

int main(int argc, char **argv)
{
	if (argc < 2) { fprintf(stderr, "usage: %s <ifname>\\n", argv[0]); return 2; }
	const char *ifname = argv[1];

	int fd = nl_open();
	if (fd < 0) return 2;

	unsigned int fam = resolve_family(fd, ETHTOOL_GENL_NAME);
	if (!fam) { fprintf(stderr, "could not resolve ethtool genl family\\n"); close(fd); return 2; }
	printf("ethtool genl family id: %u\\n", fam);

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

	printf("sending ETHTOOL_MSG_TSCONFIG_GET for %s ...\\n", ifname);
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
		printf("tsconfig_get: netlink error %d (clean -EINVAL => fixed kernel)\\n", e);
		close(fd);
		return 0;
	}
	printf("tsconfig_get: got reply type %u len %u\\n", rnh->nlmsg_type, rnh->nlmsg_len);
	close(fd);
	return 0;
}
\`\`\`

\`\`\`patch:fix.patch
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
 
\`\`\`

\`\`\`kconf:extra.config
# VLAN 802.1Q must be built-in (=y): the custom kernel's modules are not
# installed in the guest cloud image's /lib/modules, so a module (=m) would
# not be loadable. Built-in lets \`ip link add ... type vlan\` work directly.
CONFIG_VLAN_8021Q=y
# ethtool netlink is default-y with CONFIG_NET, but pin it explicitly.
CONFIG_ETHTOOL_NETLINK=y
\`\`\`

\`\`\`init:init.sh
#!/bin/bash
# Reproducer for: net: core: NULL deref in generic_hwtstamp_ioctl_lower()
# A VLAN over virtio-net (no ndo_hwtstamp_get) + an ethtool TSCONFIG_GET netlink
# doit hits the legacy ioctl fallback in generic_hwtstamp_get_lower(), which
# dereferences kernel_cfg->ifr == NULL -> kernel oops.

# Find the guest's virtio-net NIC (systemd udev renames eth0 -> enp0s1, so
# don't hardcode the name). It's the first non-loopback netdev with a device.
BASE=""
for n in /sys/class/net/*; do
    n=\${n##*/}
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
\`\`\`
`,
  },
  {
    label: "thread-compare",
    blurb: "v6.17 baseline vs the real LKML fix — actually triggers the BUG_ON",
    bundle: `---
commit: v6.17
arch: x86_64
thread-compare: https://lore.kernel.org/all/20251023125532.182262-1-daniel@iogearbox.net
search-dmesg: kernel BUG at
regex-dmesg: invalid opcode|general protection fault
---

# Reproducer — negative \`head_room\` in \`bpf_skb_change_head()\` -> \`BUG_ON\` in \`pskb_expand_head()\`

This is a \`thread-compare\` run: **baseline** is plain \`v6.17\`; **patched** is
\`v6.17\` with the lore series *“bpf: Reject negative head_room in
\\__bpf_skb_change_head”* applied. The two trees differ by exactly that fix.

## Why the previous bundle did not reproduce

The previous \`init.sh\` only printed \`uname -r\` and the last 20 \`dmesg\` lines and
exited \`0\` on **both** variants — so the bug was never exercised and the run
was not a reproduction. The bug is not in boot or build; it is triggered only
by actually running a BPF program that calls \`bpf_skb_change_head()\` with a
bad \`head_room\`, which the old script never did.

## The bug

\`bpf_skb_change_head()\` is exposed with \`arg2_type = ARG_ANYTHING\`, so the
verifier lets a program pass any \`u32\` \`head_room\`, including values that are
negative when read as a signed \`int\` (e.g. \`0x90000000\`).

\`__bpf_skb_change_head()\` only rejects when:

\`\`\`c
if (flags || (!skb_is_gso(skb) && new_len > max_len) || new_len < skb->len)
        return -EINVAL;
\`\`\`

A huge but **non-wrapping** \`head_room\` on a **GSO** skb skips the
\`new_len > max_len\` clause (\`skb_is_gso()\` is true) and keeps
\`new_len >= skb->len\`, so the guard does not fire. The value then reaches
\`skb_cow()\` -> \`__skb_cow()\`:

\`\`\`c
int delta = 0;
if (headroom > skb_headroom(skb))
        delta = headroom - skb_headroom(skb);     /* stored as *signed* int */
if (delta || cloned)
        return pskb_expand_head(skb, ALIGN(delta, NET_SKB_PAD), 0, ...);
\`\`\`

For \`head_room >= 0x80000000 + skb_headroom\`, \`delta\` is a **negative** \`int\`;
\`ALIGN(delta, NET_SKB_PAD)\` stays negative and is handed to
\`pskb_expand_head()\`, whose very first line is:

\`\`\`c
BUG_ON(nhead < 0);        /* net/core/skbuff.c */
\`\`\`

→ kernel \`BUG()\` → oops that kills the calling task (the loader, via
\`SIGSEGV\`). The fix rejects \`(s32)head_room < 0\` up front with \`-EINVAL\`, so no
oops.

## How the GSO skb is built

\`bpf_prog_test_run_skb()\` (the \`BPF_PROG_RUN\` path for \`SCHED_CLS\`) lets the
caller set \`__sk_buff.gso_size\`; a non-zero \`gso_size\` makes \`skb_is_gso()\`
true, which is exactly what bypasses the \`new_len > max_len\` check above. The
loader supplies a \`__sk_buff\` context with only \`gso_size = 8\` / \`gso_segs = 1\`
set (every other field zero, satisfying the test-run’s \`range_is_zero\` checks).

## Expected result

| variant  | behaviour                                                        | init.sh exit |
|----------|------------------------------------------------------------------|--------------|
| baseline | loader killed by the kernel oops (\`SIGSEGV\`); \`BUG:\` in dmesg   | **1** (reproduced) |
| patched  | helper returns \`-EINVAL\`; loader prints \`PATCHED_OK\` and exits 0 | 0 (fixed)    |

\`\`\`kconf:bpf.config
# Make sure the BPF syscall and the SCHED_CLS program type (registered under
# CONFIG_NET, test_run = bpf_prog_test_run_skb) are available.
CONFIG_NET=y
CONFIG_BPF=y
CONFIG_BPF_SYSCALL=y
CONFIG_BPF_JIT=y
\`\`\`

\`\`\`user:loader.c
// SPDX-License-Identifier: GPL-2.0
/*
 * Loader for the bpf_skb_change_head negative-head_room reproducer.
 *
 * Loads a tiny BPF_PROG_TYPE_SCHED_CLS program that calls
 * bpf_skb_change_head(ctx, 0x90000000, 0) and runs it via BPF_PROG_RUN on a
 * GSO skb (gso_size != 0).  On an unfixed kernel the huge head_room makes
 * __skb_cow() compute a negative \`delta\` (signed int overflow) which is then
 * passed as \`nhead\` to pskb_expand_head(), hitting BUG_ON(nhead < 0) -> oops.
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
		fprintf(stderr, "LOAD_FAIL errno=%d (%s)\\n", errno, strerror(errno));
		log[sizeof(log) - 1] = 0;
		fprintf(stderr, "verifier: %s\\n", log);
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
		fprintf(stderr, "RUN_FAIL errno=%d (%s)\\n", errno, strerror(errno));
		close(fd);
		return 3;
	}

	printf("PATCHED_OK retval=%u duration=%u\\n", ra.retval, ra.duration);
	close(fd);
	return 0;
}
\`\`\`

\`\`\`init:init.sh
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
# NET_SKB_PAD) stays negative and is passed as \`nhead\` to pskb_expand_head(),
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
if printf '%s\\n' "$DMESG" | grep -qE 'kernel BUG|BUG:|invalid opcode|general protection|Oops|UBSAN|\\[ cut here \\]'; then
	echo "REPRODUCED: kernel BUG/oops detected in dmesg"
	printf '%s\\n' "$DMESG" | tail -n 40
	exit 1
fi

if [ "$RC" -gt 128 ]; then
	echo "REPRODUCED: loader killed by signal $((RC-128)) (kernel oops)"
	exit 1
fi

echo "NOT-REPRODUCED: loader exited cleanly (head_room rejected by the fix)."
exit 0
\`\`\`
`,
  },
];

// --- on-disk examples (the "More…" browser) ------------------------------------
// Every examples/*.md in the repo, inlined at build time by Vite (same ?raw access
// docs/reproducer-spec.md uses; vite.config.ts fs.allow covers the repo root). Each
// carries an authored `summary:` and repeatable `tag:` frontmatter (see the spec);
// the "More…" modal lists them and filters by tag.
export interface DiskExample { label: string; summary: string; tags: string[]; bundle: string }

const RAW_EXAMPLES = import.meta.glob("../../../examples/*.md", {
  query: "?raw", import: "default", eager: true,
}) as Record<string, string>;

// H1 with the common inline markdown (backticks, links, emphasis) stripped for a
// clean one-line label.
function stripMd(s: string): string {
  return s
    .replace(/`([^`]*)`/g, "$1")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/[*_]{1,2}([^*_]+)[*_]{1,2}/g, "$1")
    .trim();
}

// Turn one example file into a list entry: label from the H1 (fallback: file name),
// summary from the authored `summary:` (fallback: first prose paragraph), tags from
// the repeatable `tag:` frontmatter.
function summarizeExample(path: string, raw: string): DiskExample {
  const parsed = parseBundle(raw);
  const base = path.split("/").pop()!.replace(/\.md$/, "");
  const lines = raw.split(/\r?\n/);

  const h1 = lines.find((l) => /^#\s+/.test(l));
  const label = h1 ? stripMd(h1.replace(/^#\s+/, "")) : base;

  const tags = parsed.meta.filter((m) => m.key === "tag").map((m) => m.value.trim()).filter(Boolean);

  let summary = parsed.meta.find((m) => m.key === "summary")?.value.trim() ?? "";
  if (!summary) {
    // First prose line: skip frontmatter, headings, and fenced code.
    let fence: string | null = null;
    for (const l of lines) {
      const open = l.match(FENCE_OPEN);
      if (fence === null && open) { fence = open[1]; continue; }
      if (fence !== null) { if (new RegExp("^`{" + fence.length + ",}\\s*$").test(l)) fence = null; continue; }
      const t = l.trim();
      if (!t || t === "---" || t.startsWith("#") || KV.test(t)) continue;
      summary = stripMd(t);
      break;
    }
  }
  return { label, summary, tags, bundle: raw };
}

// Built once at module load (glob is eager). Sorted by label for a stable list.
export const DISK_EXAMPLES: DiskExample[] = Object.entries(RAW_EXAMPLES)
  .map(([path, raw]) => summarizeExample(path, raw))
  .sort((a, b) => a.label.localeCompare(b.label));

// Union of all tags across the examples, for the filter bar (sorted).
export const EXAMPLE_TAGS: string[] = [...new Set(DISK_EXAMPLES.flatMap((e) => e.tags))].sort();
