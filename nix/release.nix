# The release assets: one archive per plugin package, an archive of the
# binaries, and a digest beside each. The binaries embed a catalog naming every
# plugin archive's URL and hash; that catalog stays a build input.
{
  lib,
  stdenv,
  darwin,
  file,
  gnutar,
  gzip,
  jq,
  rustPlatform,
  plugins,
  repository,
  source,
  version,
}:

let
  target = stdenv.hostPlatform.rust.rustcTarget;
  url = "https://github.com/${repository}/releases/download/v${version}";

  # Writes a gzipped tar whose bytes depend only on file contents and names.
  # The catalog pins what the release publishes, so both archives are built
  # here and the same way.
  tarball = ''
    tarball() {
      local root="$1" archive="$2" mode="$3"
      ( cd "$root" && find . -type f | sed 's|^\./||' | LC_ALL=C sort ) > names
      if [ ! -s names ]; then
        echo "no files to archive in $root" >&2
        exit 1
      fi
      tar --create --format=ustar --no-recursion --directory "$root" --files-from names \
        --numeric-owner --owner=0 --group=0 --mtime=@0 --mode="$mode" \
        | gzip --no-name > "$archive"
      rm names
    }
  '';

  # A release binary runs off a Nix store. Linux links it statically; darwin
  # rewrites store libraries to their system copies and signs the result.
  portable =
    if stdenv.hostPlatform.isDarwin then
      ''
        for binary in tree/bin/*; do
          otool -L "$binary" | awk 'NR > 1 { print $1 }' | grep '^/nix/store' | while read -r library; do
            install_name_tool -change "$library" "/usr/lib/$(basename "$library")" "$binary"
          done
          codesign --force --sign - "$binary"
          if otool -L "$binary" | tail -n +2 | grep -q /nix/store; then
            echo "$binary loads a store library" >&2
            exit 1
          fi
        done
      ''
    else
      ''
        for binary in tree/bin/*; do
          if ! file "$binary" | grep -q "statically linked"; then
            echo "$binary is not statically linked" >&2
            exit 1
          fi
        done
      '';
in
rustPlatform.buildRustPackage {
  pname = "pluribus-release";
  inherit version;

  src = source;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    file
    gnutar
    gzip
    jq
  ]
  # `install_name_tool` invalidates the signature the binary needs on darwin.
  ++ lib.optional stdenv.hostPlatform.isDarwin darwin.sigtool;

  passthru = { inherit target; };

  buildPhase = ''
    runHook preBuild
    ${tarball}
    mkdir -p assets

    ${lib.concatStringsSep "\n" (
      lib.mapAttrsToList (name: package: ''
        tarball "${package}/share/pluribus/plugins/${name}" \
          "assets/pluribus-plugin-${name}-${version}.tar.gz" u=rw,go=r
      '') plugins
    )}
    for name in ${lib.escapeShellArgs (lib.attrNames plugins)}; do
      asset="pluribus-plugin-$name-${version}.tar.gz"
      jq -n --arg name "$name" --arg url "${url}/$asset" \
        --arg sha256 "$(sha256sum "assets/$asset" | cut -d' ' -f1)" \
        '{($name): {url: $url, sha256: $sha256}}'
    done | jq -s '{version: 1, plugins: add}' > assets/plugins.json

    export PLURIBUS_RELEASE_CATALOG=$PWD/assets/plugins.json
    cargo build --locked --release -p pluribus-plugin-http --bin pluribus-http-listener
    cargo build --locked --release -p pluribus-cli
    cargo build --locked --release -p pluribus-plugin-shell --bin pluribus-shell-executor
    cargo build --locked --release -p pluribus-plugin-cli --bin pluribus-cli-bridge
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    ${tarball}
    mkdir -p $out tree/bin

    # A cross build, the static Linux release included, names its target directory.
    built=target/''${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}release
    install -m755 "$built/pluribus" "$built/pluribus-shell-executor" \
      "$built/pluribus-cli-bridge" "$built/pluribus-http-listener" \
      tree/bin/
    ${portable}
    tarball tree "$out/pluribus-${version}-${target}.tar.gz" u=rwx,go=rx

    cp assets/*.tar.gz $out/
    # Every asset ships the digest it is checked against.
    ( cd $out && for asset in *.tar.gz; do sha256sum "$asset" > "$asset.sha256"; done )
    runHook postInstall
  '';

  doCheck = false;

  # Exercises the archive without Rust or its source checkout on PATH.
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    smoke=$(mktemp -d)
    tar -xf "$out/pluribus-${version}-${target}.tar.gz" -C "$smoke"

    config=$smoke/agent/config.json
    pluribus() {
      env -u PLURIBUS_PLUGIN_DIR PATH="" "$binary" --data-dir "$smoke/agent" "$@"
    }

    binary=$smoke/bin/pluribus
    pluribus init

    # An agent starts with no plugins.
    jq -e '.plugin_instances == {}' "$config" > /dev/null

    # Install packages the way a release installation does: by URL and digest.
    digest() { cut -d' ' -f1 < "$out/pluribus-plugin-$1-${version}.tar.gz.sha256"; }
    for name in memory rlm; do
      pluribus install "file://$out/pluribus-plugin-$name-${version}.tar.gz" \
        --sha256 "$(digest "$name")"
    done
    jq -e '.plugin_instances | has("memory") and has("rlm")' "$config" > /dev/null

    # A digest already resolved must be served from the cache, not the network.
    pluribus install https://example.invalid/unavailable \
      --sha256 "$(digest memory)" --id cached

    runHook postInstallCheck
  '';

  meta.description = "Pluribus release assets";
}
