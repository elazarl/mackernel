#!/usr/bin/env python3
"""validate_qemu_extra: a bundle's qemu-device / qemu-machine / append are
untrusted (a bundle can come from a URL), so they must be bare, allowlisted
tokens -- never able to inject a new qemu flag or shell-escape.
Run: python3 test_qemu_extra.py"""
import importlib.util
import sys
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    "run_kernel", Path(__file__).resolve().parent / "run-kernel.py")
rk = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = rk   # so @dataclass can resolve cls.__module__
_spec.loader.exec_module(rk)


def _rejects(meta) -> bool:
    try:
        rk.validate_qemu_extra(meta)
        return False
    except SystemExit:      # die() -> sys.exit
        return True


def test_qemu_extra():
    # Good bundles pass through unchanged (order preserved).
    dev, mach, ap = rk.validate_qemu_extra({
        "qemu-device": ["intel-iommu,intremap=on,caching-mode=on",
                        "pcie-root-port,id=rp0,bus=pcie.0,chassis=1",
                        "edu,bus=rp0"],
        "qemu-machine": "q35,kernel-irqchip=split",
        "append": ["intel_iommu=on"],
    })
    assert dev[0].startswith("intel-iommu") and dev[-1] == "edu,bus=rp0"
    assert mach == "q35,kernel-irqchip=split"
    assert ap == "intel_iommu=on"

    # Absent keys -> empty, no machine override.
    assert rk.validate_qemu_extra({}) == ([], None, "")

    # A single (scalar) append line still works.
    assert rk.validate_qemu_extra({"append": "nokaslr quiet"})[2] == "nokaslr quiet"

    # Injection / escape attempts are rejected (die).
    assert _rejects({"qemu-device": ["-drive file=/etc/passwd,if=virtio"]})  # new qemu flag
    assert _rejects({"qemu-device": ["edu -snapshot"]})                      # 2nd argv token
    assert _rejects({"qemu-device": ["foo;reboot"]})                         # shell metachar
    assert _rejects({"qemu-device": ["x`id`"]})                              # backtick
    assert _rejects({"qemu-device": ["a$(whoami)"]})                         # $()
    assert _rejects({"qemu-machine": "-nographic"})                          # leading '-'
    assert _rejects({"append": ["-drive"]})                                  # leading '-'
    assert _rejects({"append": ["init=/bin/sh;reboot"]})                     # shell metachar
    assert _rejects({"qemu-device": ["edu,bus=rp0"] * (rk.MAX_QEMU_DEVICES + 1)})  # too many


if __name__ == "__main__":
    test_qemu_extra()
    print("ok")
