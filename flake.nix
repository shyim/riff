{
  description = "Standalone Composer-compatible package manager written in Rust";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "composer-rs";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "-p" "composer-rs-cli" ];
            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.perl
              pkgs.pkg-config
            ];
            buildInputs = with pkgs; [
              openssl
              zlib
            ] ++ nixpkgs.lib.optionals stdenv.hostPlatform.isDarwin [ libiconv ];
            OPENSSL_NO_VENDOR = "1";
            doCheck = false;
            postFixup = ''
              for binary in composer composer-rs; do
                wrapProgram "$out/bin/$binary" \
                  --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.git pkgs.php ]}
              done
            '';
          };
        }
      );

      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              cargo
              clippy
              git
              gnumake
              jq
              perl
              pkg-config
              rust-analyzer
              rustc
              rustfmt
              unzip
              zip
            ];
            buildInputs = with pkgs; [
              bzip2
              libssh2
              openssl
              php
              xz
              zlib
            ] ++ nixpkgs.lib.optionals stdenv.hostPlatform.isDarwin [ libiconv ];

            shellHook = ''
              export COMPOSER_SRC_DIR="''${COMPOSER_SRC_DIR:-/workspace/composer}"
              export PHP_BIN="''${PHP_BIN:-${pkgs.php}/bin/php}"
              export COMPOSER_RS_PHP="''${COMPOSER_RS_PHP:-$PHP_BIN}"
            '';
          };
        }
      );
    };
}
