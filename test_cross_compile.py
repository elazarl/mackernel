#!/usr/bin/env python3
"""Cross-compile decision logic: an x86_64 host cross-compiles arm64 (native
container + cross toolchain, no qemu-user); everything else keeps the
matching-arch-container path. Run: python3 test_cross_compile.py"""
import mklib


def _patch(host, selinux):
    mklib.host_arch = lambda: host
    mklib.selinux_enforcing = lambda: selinux


def test_cross_compile():
    orig_host, orig_se = mklib.host_arch, mklib.selinux_enforcing
    try:
        # x86_64 host -> cross-compile arm64, native build x86_64.
        _patch("x86_64", True)
        assert mklib.cross_compile("arm64") == "aarch64-linux-gnu-"
        assert mklib.cross_compile("x86_64") == ""
        # Cross + native both run the host-arch container: no --platform.
        assert mklib.platform_args("arm64") == []
        assert mklib.platform_args("x86_64") == []
        # Native cross-compile keeps SELinux confinement (no label=disable).
        assert "label=disable" not in mklib.hardening_args("arm64")

        # arm64 host -> native arm64, x86_64 is genuinely emulated.
        _patch("arm64", True)
        assert mklib.cross_compile("arm64") == ""
        assert mklib.cross_compile("x86_64") == ""
        assert mklib.platform_args("x86_64") == ["--platform", "linux/amd64"]
        # Emulated foreign-arch run drops the MCS label on an enforcing host.
        assert "label=disable" in mklib.hardening_args("x86_64")
    finally:
        mklib.host_arch, mklib.selinux_enforcing = orig_host, orig_se


if __name__ == "__main__":
    test_cross_compile()
    print("ok")
