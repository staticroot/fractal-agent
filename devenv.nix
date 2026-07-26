{ pkgs, ... }:

{
  languages.rust = {
    enable = true;
    channel = "stable";
  };

  packages = [ pkgs.git ];

  tasks = {
    "agent:build".exec = "cargo build --workspace";
    "agent:test".exec = "cargo test --workspace";
    "agent:clippy".exec = "cargo clippy --workspace --all-targets -- -D warnings";
    "agent:run-vm-test".exec = "nix build .#checks.aarch64-linux.vm -L";
  };
}
