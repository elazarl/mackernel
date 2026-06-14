#!/usr/bin/env bash
# (5) Build a cloud-init "NoCloud" seed disk that configures the guest's network
# (DHCP on the virtio NIC) and drops in an SSH key + login user, so the Mac host
# can SSH straight into the booted kernel.
#
# The seed is an ISO9660+Joliet image labelled CIDATA (what cloud-init's NoCloud
# datasource scans for). On macOS we build it with the bundled `hdiutil`; on Linux
# with `xorriso` (or `genisoimage`). Joliet preserves the lowercase
# user-data/meta-data names that NoCloud requires.
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
elif [ ! -f "$SSH_KEY.pub" ]; then
  # Private key exists but its .pub is gone -- re-derive the public key from it.
  echo "deriving missing public key $SSH_KEY.pub ..."
  ssh-keygen -y -f "$SSH_KEY" > "$SSH_KEY.pub"
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
# Build the ISO9660+Joliet image. Joliet keeps the lowercase filenames; CIDATA is
# the volume label NoCloud scans for. macOS ships hdiutil; Linux uses xorriso or
# genisoimage (whichever is installed).
if command -v hdiutil >/dev/null 2>&1; then
  hdiutil makehybrid -quiet -o "$SEED" -iso -joliet -default-volume-name CIDATA "$work"
elif command -v xorriso >/dev/null 2>&1; then
  xorriso -as mkisofs -quiet -o "$SEED" -V CIDATA -J -r "$work"
elif command -v genisoimage >/dev/null 2>&1; then
  genisoimage -quiet -o "$SEED" -V CIDATA -J -r "$work"
else
  echo "error: need hdiutil (macOS) or xorriso/genisoimage (Linux) to build the seed ISO" >&2
  exit 1
fi

echo "=== built cloud-init seed: $SEED ==="
echo "    user: $GUEST_USER   ssh key: $SSH_KEY   (password: $GUEST_PASS)"
