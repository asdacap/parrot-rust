{
  pkgs,
  root,
}: {
  web = pkgs.buildNpmPackage {
    pname = "parrot-web-assets";
    version = "0.0.0-dev";
    src = pkgs.lib.cleanSourceWith {
      src = root;
      filter = path: _type: let
        relative = pkgs.lib.removePrefix (toString root + "/") (toString path);
      in
        path == toString root || builtins.elem relative ["web" "proto"] || pkgs.lib.hasPrefix "web/" relative || pkgs.lib.hasPrefix "proto/" relative;
    };
    sourceRoot = "source/web";
    npmDepsHash = "sha256-xjFBESklDvax+mZAI4r9bZBXAJnqUH/TxdOEny1AahM=";
    NO_COLOR = "1";
    nativeBuildInputs = [pkgs.protobuf];
    buildPhase = ''
      cd ..
      npm --prefix web test
      npm --prefix web run build
      cd web
    '';
    installPhase = ''
      mkdir -p "$out"
      cp -R dist "$out/"
    '';
  };
}
