#!/usr/bin/env bash
# Boot a viaos image in QEMU without touching it.
#
#   scripts/qemu.sh <image.raw|image.raw.zst> [workdir]
#
# The disk is a 20G copy-on-write overlay, so /var has room to grow on first
# boot. Serial console on stdio; the host is 10.0.2.2 from inside; SSH on
# localhost:2222. With SERIAL_SOCK=<path> the console is a unix socket instead.
set -euo pipefail
image=$(realpath "$1")
work=${2:-$(mktemp -d)}
mkdir -p "$work"

if [[ $image == *.zst ]]; then
  zstd -dqf "$image" -o "$work/base.raw"
  image="$work/base.raw"
fi

ovmf=${OVMF:-$(nix build --no-link --print-out-paths nixpkgs#OVMF.fd)/FV}
cp --no-preserve=mode "$ovmf/OVMF_VARS.fd" "$work/vars.fd"
qemu-img create -q -f qcow2 -F raw -b "$image" "$work/disk.qcow2" 20G

if [[ -n ${SERIAL_SOCK:-} ]]; then
  console=(-chardev "socket,id=ser0,path=$SERIAL_SOCK,server=on,wait=off,logfile=$work/serial.log"
           -serial chardev:ser0 -display none -monitor none)
else
  console=(-nographic)
fi

exec qemu-system-x86_64 \
  -enable-kvm -cpu host -smp 2 -m 2048 \
  -drive if=pflash,format=raw,readonly=on,file="$ovmf/OVMF_CODE.fd" \
  -drive if=pflash,format=raw,file="$work/vars.fd" \
  -drive if=virtio,format=qcow2,file="$work/disk.qcow2" \
  -nic user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:2222-:22 \
  "${console[@]}"
