# Raspberry Pi 4, Pi 400 and CM4.
#
# The Pi's firmware reads the FAT ESP, loads U-Boot, and U-Boot boots the ESP
# as any EFI firmware would: systemd-boot, then a UKI. Everything after the
# firmware is the same as on a PC, A/B slots and rollback included.
#
# Pi 3 is out: its firmware cannot boot from a GPT disk. Pi 5 needs U-Boot
# support that is not in nixpkgs yet.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  fw = "${pkgs.raspberrypifw}/share/raspberrypi/boot";
  configTxt = pkgs.writeText "config.txt" ''
    [pi4]
    kernel=u-boot-rpi4.bin
    enable_gic=1
    armstub=armstub8-gic.bin
    disable_overscan=1
    arm_boost=1

    [cm4]
    otg_mode=1

    [all]
    arm_64bit=1
    # U-Boot needs the UART enabled, used or not.
    enable_uart=1
    avoid_warnings=1
  '';
  files = {
    "/config.txt" = configTxt;
    "/u-boot-rpi4.bin" = "${pkgs.ubootRaspberryPi4_64bit}/u-boot.bin";
    "/armstub8-gic.bin" = "${pkgs.raspberrypi-armstubs}/armstub8-gic.bin";
  }
  // lib.genAttrs' [
    "start4.elf"
    "fixup4.dat"
    "start4cd.elf"
    "fixup4cd.dat"
    "bcm2711-rpi-4-b.dtb"
    "bcm2711-rpi-400.dtb"
    "bcm2711-rpi-cm4.dtb"
  ] (f: lib.nameValuePair "/${f}" "${fw}/${f}");
  # The image builder runs `unshare --map-root-user fakeroot systemd-repart`.
  # Building aarch64 on an x86 host goes through qemu-user, which is
  # multithreaded, so the kernel refuses it a user namespace. fakeroot alone
  # gives the build the root view it needs; this `unshare` runs its command
  # without the namespace. It comes first on PATH because the image builder
  # puts this package first.
  repartPackage = pkgs.symlinkJoin {
    name = "systemd-repart-no-userns";
    paths = [ pkgs.buildPackages.systemd ];
    postBuild = ''
      cat > $out/bin/unshare <<'SH'
      #!${pkgs.buildPackages.runtimeShell}
      while [ "''${1#-}" != "$1" ]; do shift; done
      exec "$@"
      SH
      chmod +x $out/bin/unshare
    '';
  };
in
lib.mkIf pkgs.stdenv.hostPlatform.isAarch64 {
  image.repart.package = repartPackage;

  image.repart.partitions."00-esp".contents = lib.mapAttrs (_: source: { inherit source; }) files;

  boot.initrd.availableKernelModules = [
    "pcie_brcmstb"
    "reset-raspberrypi"
    "sdhci_iproc"
    "usbhid"
  ];
  boot.kernelParams = [ "console=ttyS0,115200" ];
}
