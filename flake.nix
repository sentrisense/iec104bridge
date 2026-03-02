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
