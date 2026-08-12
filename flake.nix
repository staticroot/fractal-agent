{
  description = "fractal-agent, unprivileged stateful system-configuration daemon for Fractal Linux";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      mkPackage = pkgs: pkgs.rustPlatform.buildRustPackage {
        pname = "fractal-agent";
        version = "0.1.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;

        outputs = [ "out" "cli" ];
        postInstall = ''
          moveToOutput bin/fractal "$cli"
        '';

        meta.mainProgram = "fractal-agent";
      };
    in
    {
      overlays.default = final: prev: { fractal-agent = mkPackage final; };
      nixosModules.default = import ./nix/module.nix;

      packages = forAllSystems (system: {
        default = mkPackage nixpkgs.legacyPackages.${system};
      });
    };
}
