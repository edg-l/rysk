#!/usr/bin/env bash
# Fetch what a Linux boot needs and boot it.
#
# None of this is in the repository: a kernel and an initramfs are tens of megabytes
# and belong to Debian rather than to rysk. So they are fetched, once, into a
# directory beside wherever this is run from, and every one of them is pinned to a
# version this has actually booted.
#
#   scripts/boot-linux.sh            the machine on the console
#   scripts/boot-linux.sh --gui      a screen and the panels beside it
#
# About 110 MB the first time and nothing after that.
set -euo pipefail

# Where the artifacts go. Overridable, since a checkout is not always the right place
# to leave a hundred megabytes.
into="${RYSK_GUEST:-$PWD/guest}"
mkdir -p "$into"

# The emulator this runs is the one built beside it, not a downloaded release: the
# point of the script is to boot what you just changed.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
rysk="$root/target/release/rysk"
[ -x "$rysk" ] || { echo "building rysk"; cargo build --release --manifest-path "$root/Cargo.toml"; }

get() {
    [ -f "$into/$2" ] && return
    echo "fetching $2"
    curl -fL --progress-bar -o "$into/$2" "$1"
}

# OpenSBI, which is what the machine hands control to first. `fw_dynamic` rather than
# `fw_jump`: fw_jump relocates the device tree to 0x82200000, which lands inside a
# 36 MB kernel.
get https://github.com/riscv-software-src/opensbi/releases/download/v1.9/opensbi-1.9-rv-bin.tar.xz opensbi.tar.xz
[ -f "$into/fw_dynamic.bin" ] || {
    mkdir -p "$into/opensbi"
    tar -xf "$into/opensbi.tar.xz" -C "$into/opensbi"
    # Found rather than reached by counting path components: these archives disagree
    # about how deep they nest and about whether they prefix entries with `./`, and a
    # count that is one out fails after the download rather than before it.
    #
    # `lp64`, and said exactly. This tarball carries three builds of this firmware and
    # one of them is `ilp32`, which is a 32-bit machine's; loading it produces a run
    # that prints nothing at all rather than one that says what is wrong.
    firmware="$(find "$into/opensbi" -path '*/lp64/generic/*' -name fw_dynamic.bin)"
    [ -f "$firmware" ] || { echo "no lp64 fw_dynamic.bin in the opensbi tarball" >&2; exit 1; }
    cp "$firmware" "$into/fw_dynamic.bin"
}

# The kernel. Debian ships it in `linux-binary`; the plain `linux-image` package is a
# metapackage of under two kilobytes and contains no kernel at all.
get 'https://deb.debian.org/debian/pool/main/l/linux/linux-binary-7.1.8%2Bdeb14.1-riscv64_7.1.8-2_riscv64.deb' kernel.deb
[ -f "$into/vmlinux" ] || {
    dpkg-deb -x "$into/kernel.deb" "$into/kernel"
    cp "$into"/kernel/boot/vmlinux-* "$into/vmlinux"
}

# And a userland to reach a shell in.
get https://deb.debian.org/debian/dists/trixie/main/installer-riscv64/current/images/netboot/netboot.tar.gz netboot.tar.gz
[ -f "$into/initrd.gz" ] || {
    mkdir -p "$into/netboot"
    tar -xzf "$into/netboot.tar.gz" -C "$into/netboot"
    cp "$(find "$into/netboot" -name initrd.gz | head -1)" "$into/initrd.gz"
}

# A window wants something to put on it, and something to type at.
extra=()
if [ "${1:-}" = "--gui" ]; then
    extra=(--display bochs --usb hid --gui)
fi

echo
exec "$rysk" -m 1024 -smp 4 --aia aplic-imsic "${extra[@]}" \
    --initrd "$into/initrd.gz" --append "console=ttyS0 rdinit=/bin/sh" \
    "$into/fw_dynamic.bin" "$into/vmlinux@0x80200000"
