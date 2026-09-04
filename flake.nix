{
  description = "Pluribus: a plugin-based agent runtime";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forEachSystem =
        f: lib.genAttrs systems (system: f (import ./nix { pkgs = nixpkgs.legacyPackages.${system}; }));
    in
    {
      overlays.default = final: _: { inherit (import ./nix { pkgs = final; }) pluribus; };

      packages = forEachSystem (
        pluribus:
        {
          default = pluribus.pluribus;
          inherit (pluribus) release;
        }
        # Each plugin installs on its own, and composes through `withPlugins`.
        // lib.mapAttrs' (name: lib.nameValuePair "plugin-${name}") pluribus.plugins
      );

      # `buildPluginPackage` builds a plugin package in another repository.
      lib = forEachSystem (pluribus: {
        inherit (pluribus) buildPluginPackage withPlugins;
      });

      devShells = forEachSystem (pluribus: {
        default = pluribus.shell;
      });
    };
}
