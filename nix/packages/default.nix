{
  pkgs,
  rust,
  root,
}: let
  inherit (pkgs) lib;
  unicodeLicense = "LICENSES/Unicode-3.0.txt";
  fuzzSources = [
    "tests/fuzz/Cargo.toml"
    "tests/fuzz/campaign.rs"
    "tests/fuzz/generator.rs"
    "tests/fuzz/support.rs"
    "tests/fuzz/paths.rs"
    "tests/fuzz/queue.rs"
    "tests/fuzz/streams.rs"
    "tests/fuzz/structured.rs"
    "tests/stress/queue.rs"
    "tests/stress/scheduler.rs"
    "tests/stress/task_scheduler.rs"
    "tests/fixtures/regressions/T77/campaign.json"
    "crates/parrot-storage/tests/history_files/fixtures/duplicate-fields.json"
    "tests/fixtures/regressions/T77/path-regressions.json"
    "tests/fixtures/regressions/T77/stream-regressions.json"
  ];
  source = lib.cleanSourceWith {
    src = root;
    filter = path: type: let
      relative = lib.removePrefix (toString root + "/") (toString path);
      parts = lib.splitString "/" relative;
      top = builtins.head parts;
    in
      path
      == toString root
      || builtins.elem relative ["Cargo.toml" "Cargo.lock" "rust-toolchain.toml"]
      || builtins.elem top ["assets" "proto" "vendor" "packaging"]
      || relative == "THIRD_PARTY_NOTICES.md"
      || relative == unicodeLicense
      || (type == "directory" && lib.hasPrefix (relative + "/") unicodeLicense)
      || builtins.elem relative fuzzSources
      || (type == "directory" && lib.any (file: lib.hasPrefix (relative + "/") file) fuzzSources)
      || relative == "crates"
      || builtins.match "crates/[^/]+" relative != null
      || builtins.match "crates/[^/]+/(Cargo.toml|build.rs)" relative != null
      || builtins.match "crates/[^/]+/(src|build|native)(/.*)?" relative != null
      || (top == "web" && !lib.any (part: builtins.elem part ["node_modules" "dist" ".tmp" ".git"]) parts);
  };

  web = pkgs.buildNpmPackage {
    pname = "parrot-web-assets";
    version = "0.0.0-dev";
    src = source;
    sourceRoot = "source/web";
    npmDepsHash = "sha256-xjFBESklDvax+mZAI4r9bZBXAJnqUH/TxdOEny1AahM=";
    nativeBuildInputs = [pkgs.protobuf];
    npmBuildScript = "build";
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/dist"
      cp -R dist/. "$out/dist/"
      node ../packaging/collect-web-notices.mjs node_modules "$out/licenses"
      runHook postInstall
    '';
  };

  build = buildPkgs: target:
    (buildPkgs.makeRustPlatform {
      cargo = rust;
      rustc = rust;
    }).buildRustPackage {
      pname = "parrot";
      version = "0.0.0-dev";
      src = source;
      cargoLock.lockFile = root + "/Cargo.lock";
      cargoBuildFlags = ["--locked" "-p" "parrot-cli" "--bin" "parrot"];
      CARGO_NET_OFFLINE = "true";
      CARGO_TARGET_DIR = "target";
      # The cross hook defaults musl to dynamic CRT; change only the target flags,
      # leaving host build scripts/proc macros dynamically linked for the builder.
      CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = lib.optionalString (target == "x86_64-linux-musl") "-Ctarget-feature=+crt-static";
      doCheck = false;
      nativeBuildInputs = [pkgs.protobuf pkgs.nodejs pkgs.perl];
      PROTOC = "${pkgs.protobuf}/bin/protoc";
      CC = "${buildPkgs.stdenv.cc}/bin/${buildPkgs.stdenv.cc.targetPrefix or ""}cc";
      preBuild = ''
        mkdir -p web/dist
        cp -R ${web}/dist/. web/dist/
        cargo metadata --offline --locked --format-version 1 > package-metadata.json
      '';
      installPhase = ''
        runHook preInstall
        mkdir -p "$out/bin" "$out/share/parrot"
        install -m755 "target/${buildPkgs.stdenv.hostPlatform.rust.rustcTargetSpec}/release/parrot" "$out/bin/parrot"
        ${lib.optionalString buildPkgs.stdenv.hostPlatform.isLinux ''
          "$CC" -std=c11 -O2 -Wall -Wextra -Werror ${lib.optionalString (target == "x86_64-linux-musl") "-static"} \
            crates/parrot-tools/native/parrot-pty-attach.c -o "$out/bin/parrot-pty-attach"
        ''}
        cp -R assets/config assets/skills proto "$out/share/parrot/"
        mkdir -p "$out/share/parrot/native" "$out/share/parrot/web"
        cp crates/parrot-tools/native/parrot-pty-attach.c "$out/share/parrot/native/"
        cp -R ${web}/dist/. "$out/share/parrot/web/"
        mkdir -p "$out/share/parrot/licenses"
        cp -R ${web}/licenses "$out/share/parrot/licenses/web"
        mkdir -p "$out/share/parrot/licenses/rust"
        cp ${rust}/share/doc/rust/{COPYRIGHT,LICENSE-APACHE,LICENSE-MIT} "$out/share/parrot/licenses/rust/"
        cp ${unicodeLicense} "$out/share/parrot/licenses/UNICODE-LICENSE.txt"
        cp THIRD_PARTY_NOTICES.md "$out/share/parrot/THIRD_PARTY_NOTICES.md"
        cp packaging/README.md "$out/share/parrot/PACKAGING.md"
        printf '%s\n' '${target}' > "$out/share/parrot/target"
        node packaging/collect-notices.mjs package-metadata.json "$out/share/parrot/licenses/cargo"
        (cd "$out/share/parrot"; find config skills proto native web -type f -print0 \
          | sort -z | xargs -0 sha256sum) > "$out/share/parrot/resources.sha256"
        runHook postInstall
      '';
      meta = {
        description = "Native Parrot CLI with local agent, serve and web support";
        mainProgram = "parrot";
        platforms = [buildPkgs.stdenv.hostPlatform.system];
      };
    };
in
  if pkgs.stdenv.hostPlatform.isLinux
  then {
    parrot-linux = build pkgs "x86_64-linux";
    parrot-musl = build pkgs.pkgsCross.musl64 "x86_64-linux-musl";
    default = build pkgs "x86_64-linux";
  }
  else {
    parrot-darwin = build pkgs "aarch64-darwin";
    default = build pkgs "aarch64-darwin";
  }
