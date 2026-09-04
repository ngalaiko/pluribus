# Every derivation the repository builds, for one package set.
{
  pkgs,
  repository ? "ngalaiko/pluribus",
}:

let
  inherit (pkgs) lib;

  version = (lib.importTOML ../Cargo.toml).workspace.package.version;

  # The workspace without the outputs a build writes into it.
  source = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.lock
      ../Cargo.toml
      ../crates
      ../plugins
      ../rust-toolchain.toml
      ../schemas
      ../wit
    ];
  };

  workspace = pkgs.callPackage ./workspace.nix { inherit source version; };

  # The builder a plugin in another repository calls.
  buildPluginPackage = pkgs.callPackage ./plugin.nix { inherit (workspace) packager; };

  package =
    {
      name,
      components,
      binaries ? [ ],
    }:
    buildPluginPackage {
      inherit name binaries version;
      src = ../plugins + "/${name}";
      components = lib.genAttrs components (
        component: "${workspace.components}/${name}/${if component == "" then "main" else component}.wasm"
      );
    };
in
rec {
  inherit buildPluginPackage workspace;

  plugins = {
    cli = package {
      name = "cli";
      components = [ "main" ];
      # The bridge is the terminal half of this plugin.
      binaries = [ "${workspace.helpers}/bin/pluribus-cli-bridge" ];
    };
    memory = package {
      name = "memory";
      components = [ "main" ];
    };
    openai-codex = package {
      name = "openai-codex";
      components = [ "main" ];
    };
    openrouter = package {
      name = "openrouter";
      components = [ "main" ];
    };
    rlm = package {
      name = "rlm";
      components = [
        "cognition"
        "js"
      ];
    };
    shell = package {
      name = "shell";
      components = [ "main" ];
      # The executor is the shell plugin's native half.
      binaries = [ "${workspace.helpers}/bin/pluribus-shell-executor" ];
    };
    telegram = package {
      name = "telegram";
      components = [
        "receive"
        "send"
      ];
    };
  };

  # A test fixture, never installed.
  echo = package {
    name = "echo";
    components = [ "" ];
  };

  # `selector` is a list of plugin packages, or a function taking `plugins`.
  withPlugins =
    selector:
    let
      chosen = if lib.isFunction selector then selector plugins else selector;
    in
    pkgs.runCommand "pluribus-${version}"
      {
        nativeBuildInputs = [ pkgs.makeWrapper ];
        passthru = {
          inherit plugins version withPlugins;
          unwrapped = workspace;
        };
        inherit (workspace) meta;
      }
      ''
        mkdir -p $out/bin $out/share/pluribus/plugins
        for path in ${lib.escapeShellArgs ([ workspace ] ++ chosen)}; do
          if [ -d "$path/bin" ]; then
            ln -s "$path"/bin/* $out/bin/
          fi
          if [ -d "$path/share/pluribus/plugins" ]; then
            # A package holds no symlinks: the loader rejects them.
            cp -RL "$path"/share/pluribus/plugins/* $out/share/pluribus/plugins/
          fi
        done
        chmod -R u+w $out/share/pluribus/plugins

        # `pluribus` resolves packages from the directory holding its own
        # binary, which a tree of store paths is not.
        wrapProgram $out/bin/pluribus \
          --set PLURIBUS_PLUGIN_DIR $out/share/pluribus/plugins

        # A selection carrying the example packages proves they load and that
        # `bundled:` names reach them: the example installs each one.
        example=1
        for name in cli shell openrouter rlm; do
          [ -d "$out/share/pluribus/plugins/$name" ] || example=
        done
        if [ -n "$example" ]; then
          check=$(mktemp -d)
          $out/bin/pluribus --data-dir "$check" init --example > /dev/null
          rm -rf "$check"
        fi
      '';

  pluribus = withPlugins (lib.attrValues plugins);

  release = pkgs.callPackage ./release.nix {
    inherit
      plugins
      repository
      source
      version
      ;
  };

  # Every package in one tree, for `sync-plugins` and nothing else.
  pluginTree = pkgs.symlinkJoin {
    name = "pluribus-plugins-${version}";
    paths = lib.attrValues plugins ++ [ echo ];
  };

  # Places the built packages where the workspace tests read them. It builds
  # them on demand, so opening the shell does not.
  sync-plugins = pkgs.writeShellApplication {
    name = "pluribus-sync-plugins";
    text = ''
      if [ ! -f Cargo.toml ]; then
        echo "run from the repository root" >&2
        exit 1
      fi
      tree=$(nix-build --no-out-link -A pluginTree)
      rm -rf target/plugins
      mkdir -p target/plugins
      cp -RL "$tree"/share/pluribus/plugins/* target/plugins/
      chmod -R u+w target/plugins
    '';
  };

  # The toolchain the workspace builds with outside Nix.
  shell = pkgs.mkShell {
    packages = [
      pkgs.cargo
      pkgs.clippy
      pkgs.lld
      pkgs.rust-analyzer
      pkgs.rustc
      pkgs.rustfmt
      sync-plugins
    ];
  };
}
