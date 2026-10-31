# Firmware: the blobs common edge hardware needs, not all of linux-firmware.
#
# All of linux-firmware is about 800 MB, a third of the OS, and most of it is
# for datacenter NICs, GPUs and phones. viaos ships Ethernet, Wi-Fi and
# Bluetooth for the usual chips, plus Intel graphics; `viaos.firmware` adds
# more paths under lib/firmware for other hardware.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  paths = [
    "rtl_nic"
    "rtl_bt"
    "rtlwifi"
    "rtw88"
    "rtw89"
    "intel/iwlwifi"
    "intel/ibt-*"
    "intel/ice"
    "i915"
    "brcm"
    "cypress"
    "mediatek/*.bin*"
    "tigon"
    "bnx2"
    "regulatory.db*"
  ]
  ++ config.viaos.firmware;

  firmware = pkgs.runCommand "viaos-firmware" { } ''
    cd ${pkgs.linux-firmware}/lib/firmware
    mkdir -p $out/lib/firmware
    shopt -s nullglob
    for p in ${lib.escapeShellArgs paths}; do
      for f in $p; do
        cp -dR --no-preserve=mode --parents "$f" $out/lib/firmware/
      done
    done
  '';
in
{
  options.viaos.firmware = lib.mkOption {
    type = lib.types.listOf lib.types.str;
    default = [ ];
    example = [ "amdgpu" ];
    description = "More paths (globs) under linux-firmware's lib/firmware to ship.";
  };

  config.hardware.firmware = [
    firmware
    pkgs.wireless-regdb
  ]
  ++ lib.optional pkgs.stdenv.hostPlatform.isAarch64 pkgs.raspberrypiWirelessFirmware;
}
