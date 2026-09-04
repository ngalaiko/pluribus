# Builds a plugin package: the manifest, schemas and flows under `src`, with
# `components` compiled to Wasm. Any repository can call this to publish a
# plugin `pluribus.withPlugins` accepts.
{
  lib,
  runCommand,
  packager,
}:

{
  name,
  src,
  # Component name to Wasm module, as the manifest declares them.
  components,
  # Native halves the plugin ships, installed to `bin`.
  binaries ? [ ],
  version ? null,
}:

runCommand "pluribus-plugin-${name}${lib.optionalString (version != null) "-${version}"}"
  {
    meta.description = "Pluribus ${name} plugin package";
  }
  ''
    mkdir -p $out/share/pluribus/plugins
    ${packager}/bin/pluribus-package ${src} $out/share/pluribus/plugins/${name} \
      ${lib.escapeShellArgs (
        lib.mapAttrsToList (component: module: "${component}=${module}") components
      )}
    ${lib.optionalString (binaries != [ ]) ''
      install -Dm755 -t $out/bin ${lib.escapeShellArgs binaries}
    ''}
  ''
