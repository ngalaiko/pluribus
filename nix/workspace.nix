# One cargo build of everything the repository ships: the CLI, the native half
# of every plugin that has one, the packager, and every Wasm component.
{
  lib,
  lld,
  rustPlatform,
  source,
  version,
}:

rustPlatform.buildRustPackage {
  pname = "pluribus-workspace";
  inherit version;

  src = source;
  cargoLock.lockFile = ../Cargo.lock;

  outputs = [
    "out"
    "components"
    "helpers"
    "packager"
  ];

  # rustc links wasm32-unknown-unknown with `lld` from PATH.
  nativeBuildInputs = [ lld ];

  buildPhase = ''
    runHook preBuild

    # Panic messages embed source paths, so an unremapped build differs between
    # machines. Remapping keeps every component byte-identical.
    export RUSTFLAGS="--remap-path-prefix=$PWD=/pluribus --remap-path-prefix=$CARGO_HOME=/cargo"
    wasm=target/wasm32-unknown-unknown/release

    # Builds one plugin crate for wasm32 and places it at components/<path>.
    component() {
      local crate="$1" path="$2"
      shift 2
      cargo build --locked --release --target wasm32-unknown-unknown --lib \
        -p "pluribus-plugin-$crate" "$@"
      install -D "$wasm/pluribus_plugin_''${crate//-/_}.wasm" "components/$path"
    }

    for plugin in cli echo memory openai-codex openrouter shell; do
      component "$plugin" "$plugin/main.wasm"
    done

    component http http/listen.wasm
    component github github/receive.wasm

    # The telegram plugin ships one component per role.
    component telegram-receive telegram/receive.wasm
    component telegram-send telegram/send.wasm

    component rlm-cognition rlm/cognition.wasm
    # `getrandom` has no wasm32-unknown-unknown backend; the REPL component
    # supplies one. Its RUSTFLAGS would rebuild every other component, so it
    # builds under its own target directory.
    (
      export CARGO_TARGET_DIR=target/repl
      export RUSTFLAGS="$RUSTFLAGS --cfg getrandom_backend=\"custom\""
      wasm=target/repl/wasm32-unknown-unknown/release
      component rlm-repl rlm/repl.wasm
    )

    cargo build --locked --release -p pluribus-plugin-http --bin pluribus-http-listener
    cargo build --locked --release -p pluribus-cli
    cargo build --locked --release -p pluribus-plugin-shell --bin pluribus-shell-executor
    cargo build --locked --release -p pluribus-plugin-cli --bin pluribus-cli-bridge
    cargo build --locked --release -p pluribus-plugin-package --bin pluribus-package

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    install -Dm755 target/release/pluribus $out/bin/pluribus
    install -Dm755 -t $helpers/bin \
      target/release/pluribus-shell-executor target/release/pluribus-cli-bridge \
      target/release/pluribus-http-listener
    install -Dm755 target/release/pluribus-package $packager/bin/pluribus-package
    mkdir -p $components
    cp -R components/. $components/
    runHook postInstall
  '';

  doCheck = false;

  meta = {
    description = "Plugin-based agent runtime";
    license = with lib.licenses; [
      mit
      asl20
    ];
    mainProgram = "pluribus";
    platforms = lib.platforms.unix;
  };
}
