{
  description = "fractal-agent, unprivileged stateful system-configuration daemon for Fractal Linux";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      mkPackage = pkgs: pkgs.rustPlatform.buildRustPackage {
        pname = "fractal-agent";
        version = "0.1.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        meta.mainProgram = "fractal-agent";
      };
    in
    {
      overlays.default = final: prev: { fractal-agent = mkPackage final; };
    }
    // flake-utils.lib.eachDefaultSystem (system:
      let pkgs = nixpkgs.legacyPackages.${system};
      in {
        packages.default = mkPackage pkgs;
      });
}
