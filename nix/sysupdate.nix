# OS updates with systemd-sysupdate.
#
# The agent fetches an update over its own connection into
# /var/lib/via/os-update, checks it, then runs `systemd-sysupdate update`.
# sysupdate only ever reads local files, so the device needs no update URL
# baked in and no second path out through the firewall.
#
# An update is three files, named by version and, for partitions, by the
# partition UUID systemd expects (derived from the verity root hash):
#
#   viaos_<v>.efi
#   viaos_<v>_<uuid>.store.raw.zst
#   viaos_<v>_<uuid>.verity.raw.zst
{ config, ... }:
let
  source = "/var/lib/via/os-update";
  storeType = config.image.repart.partitions."20-store".repartConfig.Type;
  verityType = config.image.repart.partitions."10-store-verity".repartConfig.Type;
in
{
  systemd.sysupdate = {
    enable = true;
    # The agent decides when; the timers would poll a source nobody filled.
    timerConfig = { };
    reboot.enable = false;
    transfers = {
      "10-store-verity" = {
        Source = {
          Type = "regular-file";
          Path = source;
          MatchPattern = "viaos_@v_@u.verity.raw.zst";
        };
        Target = {
          Type = "partition";
          Path = "auto";
          MatchPattern = "viaos_@v_v";
          MatchPartitionType = verityType;
          ReadOnly = 1;
        };
        Transfer.ProtectVersion = "%A";
      };
      "20-store" = {
        Source = {
          Type = "regular-file";
          Path = source;
          MatchPattern = "viaos_@v_@u.store.raw.zst";
        };
        Target = {
          Type = "partition";
          Path = "auto";
          MatchPattern = "viaos_@v";
          MatchPartitionType = storeType;
          ReadOnly = 1;
        };
        Transfer.ProtectVersion = "%A";
      };
      "30-uki" = {
        Source = {
          Type = "regular-file";
          Path = source;
          MatchPattern = "viaos_@v.efi";
        };
        Target = {
          Type = "regular-file";
          Path = "/EFI/Linux";
          PathRelativeTo = "boot";
          MatchPattern = [
            "viaos_@v+@l-@d.efi"
            "viaos_@v+@l.efi"
            "viaos_@v.efi"
          ];
          Mode = "0444";
          TriesLeft = 3;
          TriesDone = 0;
          InstancesMax = 2;
        };
        Transfer.ProtectVersion = "%A";
      };
    };
  };
}
