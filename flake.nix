{
  description = "viaos: an immutable, Docker-first Linux for edge devices";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # One OS per architecture. `debug` adds a root shell on the serial
      # console, for QEMU and bring-up; it is never what a fleet runs.
      mkViaos =
        {
          system,
          debug ? false,
          modules ? [ ],
        }:
        nixpkgs.lib.nixosSystem {
          inherit system;
          specialArgs = { inherit self; };
          modules = [
            ./nix/viaos.nix
            { viaos.debug = debug; }
          ]
          ++ modules;
        };
    in
    {
      packages = forAll (pkgs: {
        via-agent = pkgs.callPackage ./nix/via-agent.nix { };
        default = self.packages.${pkgs.system}.via-agent;
      });

      nixosConfigurations = {
        viaos-x86_64 = mkViaos { system = "x86_64-linux"; };
        viaos-x86_64-debug = mkViaos {
          system = "x86_64-linux";
          debug = true;
        };
        viaos-aarch64 = mkViaos { system = "aarch64-linux"; };
      };

      # `nix build .#image-x86_64` -> result/viaos_<version>.raw
      images = {
        x86_64 = self.nixosConfigurations.viaos-x86_64.config.system.build.image;
        x86_64-debug = self.nixosConfigurations.viaos-x86_64-debug.config.system.build.image;
        aarch64 = self.nixosConfigurations.viaos-aarch64.config.system.build.image;
      };

      # `nix build .#updates.x86_64` -> the files a device updates itself from
      updates = {
        x86_64 = self.nixosConfigurations.viaos-x86_64.config.system.build.update;
        x86_64-debug = self.nixosConfigurations.viaos-x86_64-debug.config.system.build.update;
        aarch64 = self.nixosConfigurations.viaos-aarch64.config.system.build.update;
      };

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            mtools
            qemu
          ];
        };
      });
    };
}
