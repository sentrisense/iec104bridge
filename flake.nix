{
  description = "IEC-104 ↔ NATS bridge development environment";

  inputs = {
    nixpkgs.url     = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay = {
      url    = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs     = import nixpkgs { inherit system overlays; };

        # Stable Rust toolchain with the extras useful during development.
        # Edition 2024 requires Rust ≥ 1.85.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" "rust-analyzer" ];
        };

        # Use a single LLVM version across clang, libclang and compiler-rt
        # so that the headers that bindgen sees match the compiler being used.
        llvm = pkgs.llvmPackages_latest;

        cargoCrap =
          let
            version = "0.2.2";
            assets = {
              x86_64-linux = {
                archive = "cargo-crap-x86_64-unknown-linux-gnu.tar.gz";
                hash = "sha256-vdcnvjpTCwEO60dYFVAmWG0rJuvjMpAmqK7BsBCjhp0=";
              };
              aarch64-linux = {
                archive = "cargo-crap-aarch64-unknown-linux-gnu.tar.gz";
                hash = "sha256-pznYBvqPXc/560N8viGbRdp6HZAWdR+kfjSrR6wluIE=";
              };
              x86_64-darwin = {
                archive = "cargo-crap-x86_64-apple-darwin.tar.gz";
                hash = "sha256-ruF2a3nE9l78+NLYDD+rrt9fyO6C4uHk+qVwFe6lyro=";
              };
              aarch64-darwin = {
                archive = "cargo-crap-aarch64-apple-darwin.tar.gz";
                hash = "sha256-SRIYiphTQRcZ9VkHubp5UcdTm/O9mRViXdcWa6ryNuY=";
              };
            };
            asset = assets.${system} or null;
          in
          if asset == null then null else pkgs.stdenvNoCC.mkDerivation {
            pname = "cargo-crap";
            inherit version;

            src = pkgs.fetchurl {
              url = "https://github.com/minikin/cargo-crap/releases/download/v${version}/${asset.archive}";
              hash = asset.hash;
            };

            dontUnpack = true;
            nativeBuildInputs = [ pkgs.autoPatchelfHook ];
            buildInputs = [ pkgs.stdenv.cc.cc.lib ];

            installPhase = ''
              runHook preInstall
              tar -xzf "$src"
              install -Dm755 cargo-crap "$out/bin/cargo-crap"
              runHook postInstall
            '';

            meta = {
              description = "CRAP metric reporting for Rust codebases";
              homepage = "https://github.com/minikin/cargo-crap";
              license = pkgs.lib.licenses.mit;
              platforms = builtins.attrNames assets;
            };
          };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "iec104bridge";

          packages = [
            # ── Rust toolchain ─────────────────────────────────────
            rustToolchain

            # ── C build toolchain (lib60870 is compiled from source) ─
            pkgs.cmake
            llvm.clang
            llvm.libclang          # provides libclang.so for bindgen
            llvm.llvm              # llvm-ar etc.

            # ── Misc build helpers ────────────────────────────────
            pkgs.pkg-config
            pkgs.git               # cargo may fetch git sources
            pkgs.cacert            # TLS certs for cargo / git fetches
            pkgs.openssl           # some crates link against it

            # ── Python (demo scripts: publisher, scraper) ─────────
            (pkgs.python3.withPackages (ps: with ps; [
              nats-py              # async NATS / JetStream client
              influxdb-client      # InfluxDB v2 write API
            ]))
          ] ++ pkgs.lib.optionals (cargoCrap != null) [
            cargoCrap
          ];

          # ── bindgen environment ───────────────────────────────────
          # bindgen needs to locate libclang.so at build time.
          LIBCLANG_PATH = "${llvm.libclang.lib}/lib";

          # Pass glibc and clang's own C headers so that bindgen can
          # resolve <stdlib.h>, <stdint.h> etc. when processing the
          # lib60870 C headers.
          BINDGEN_EXTRA_CLANG_ARGS = with pkgs; ''
            -I${glibc.dev}/include \
            -I${llvm.clang}/resource-root/include
          '';

          # Force cargo to use clang as the C compiler so that the
          # same headers / ABI are used for both compilation and binding
          # generation.
          CC  = "${llvm.clang}/bin/clang";
          CXX = "${llvm.clang}/bin/clang++";

          # ── Friendly shell banner ─────────────────────────────────
          shellHook = ''
            echo "iec104bridge dev shell – Rust $(rustc --version)"
            echo "  CC            = $CC"
            echo "  LIBCLANG_PATH = $LIBCLANG_PATH"
          '';
        };
      }
    );
}
