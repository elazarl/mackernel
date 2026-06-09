#!/usr/bin/env bash
# (5) Build a cloud-init "NoCloud" seed disk that configures the guest's network
# (DHCP on the virtio NIC) and drops in an SSH key + login user, so the Mac host
# can SSH straight into the booted kernel.
#
# The seed is an ISO9660+Joliet image labelled CIDATA (what cloud-init's NoCloud
# datasource scans for). We build it with macOS's own `hdiutil` -- no genisoimage /
# cloud-localds needed. Joliet preserves the lowercase user-data/meta-data names.
set -euo pipefail
cd "$(dirname "$0")"

SEED="${SEED:-seed.iso}"
SSH_KEY="${SSH_KEY:-id_mackernel}"          # private key on the host
GUEST_USER="${GUEST_USER:-mac}"
GUEST_PASS="${GUEST_PASS:-mackernel}"
HOSTNAME_GUEST="${HOSTNAME_GUEST:-mackernel}"

# Reuse an existing keypair, otherwise mint a passphrase-less one for the demo.
if [ ! -f "$SSH_KEY" ]; then
  echo "generating SSH keypair $SSH_KEY ..."
  ssh-keygen -t ed25519 -N "" -C "mackernel-host" -f "$SSH_KEY" >/dev/null
fi
PUBKEY="$(cat "$SSH_KEY.pub")"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# --- meta-data: instance identity + hostname ---
cat > "$work/meta-data" <<EOF
instance-id: $HOSTNAME_GUEST-001
local-hostname: $HOSTNAME_GUEST
EOF

# --- user-data: login user + SSH key + passwordless sudo ---
cat > "$work/user-data" <<EOF
#cloud-config
hostname: $HOSTNAME_GUEST
ssh_pwauth: true
disable_root: false
users:
  - name: $GUEST_USER
    groups: [sudo]
    sudo: "ALL=(ALL) NOPASSWD:ALL"
    shell: /bin/bash
    lock_passwd: false
    plain_text_passwd: "$GUEST_PASS"
    ssh_authorized_keys:
      - $PUBKEY
EOF

# --- network-config (v2): DHCP the virtio NIC, whatever it gets named ---
cat > "$work/network-config" <<EOF
version: 2
ethernets:
  zz-all-en:
    match:
      name: "en*"
    dhcp4: true
  zz-all-eth:
    match:
      name: "eth*"
    dhcp4: true
EOF

rm -f "$SEED"
# -joliet keeps the lowercase filenames; CIDATA is the volume label NoCloud looks for.
hdiutil makehybrid -quiet -o "$SEED" -iso -joliet -default-volume-name CIDATA "$work"

echo "=== built cloud-init seed: $SEED ==="
echo "    user: $GUEST_USER   ssh key: $SSH_KEY   (password: $GUEST_PASS)"
