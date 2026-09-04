# Entry point for a local checkout: `nix-build -A pluribus`, or `nix build -f .`.
# It filters the source before copying, which the flake cannot do for a working
# tree holding a `target` directory.
{
  system ? builtins.currentSystem,
  nixpkgs ? builtins.fetchTree (builtins.fromJSON (builtins.readFile ./flake.lock))
    .nodes.nixpkgs.locked,
  pkgs ? import nixpkgs { inherit system; },
}:

import ./nix { inherit pkgs; }
