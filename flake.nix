{
  description = "Caravan — an agent-in-the-loop GitHub merge queue built on mcp-cli, updatable-cli, and feedback-cli";

  inputs = {
    # Harry's current system pin. Keeping this exact source makes local builds
    # reuse the existing Nix store instead of evaluating/downloading another tree.
    nixpkgs.url = "https://api.flakehub.com/f/pinned/NixOS/nixpkgs/0.2511.914302%2Brev-1d4e0f865d68258aada31e68e6d79c8c463f3b34/019f1c78-b5ab-7af2-8516-c0d5406b0646/source.tar.gz";
    flake-utils.url = "github:numtide/flake-utils/11707dc2f618dd54ca8739b309ec4fc024de578b";

    # Pin the public ecosystem repositories as source inputs. `flake = false`
    # deliberately avoids importing their own nixpkgs graphs; Cargo is patched
    # to these exact store paths below. If one is later consumed as a flake,
    # its nixpkgs input must follow this flake's `nixpkgs`.
    mcp-cli = {
      url = "git+https://github.com/harryaskham/mcp-cli?ref=main";
      flake = false;
    };
    updatable-cli = {
      url = "git+https://github.com/harryaskham/updatable-cli?ref=main";
      flake = false;
    };
    feedback-cli = {
      url = "git+https://github.com/harryaskham/feedback-cli?ref=main&shallow=1";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      mcp-cli,
      updatable-cli,
      feedback-cli,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        lib = pkgs.lib;
        darwinLibs = lib.optionals pkgs.stdenv.isDarwin [ pkgs.libiconv ];

        updatableCliPatched = pkgs.runCommand "updatable-cli-patched" { } ''
          cp -R ${updatable-cli} "$out"
          chmod -R u+w "$out"
          substituteInPlace "$out/Cargo.toml" \
            --replace-fail 'mcp-cli = { git = "https://github.com/harryaskham/mcp-cli", branch = "main" }' \
                           'mcp-cli = { path = "${mcp-cli}" }'
        '';

        feedbackCliPatched = pkgs.runCommand "feedback-cli-patched" { } ''
          cp -R ${feedback-cli} "$out"
          chmod -R u+w "$out"
          substituteInPlace "$out/Cargo.toml" \
            --replace-fail 'mcp-cli = { git = "https://github.com/harryaskham/mcp-cli", branch = "main" }' \
                           'mcp-cli = { path = "${mcp-cli}" }'
        '';

        # Replace the three public git dependencies with the pinned flake-input
        # sources. The sandbox therefore performs no network or Git authentication.
        caravanSrc = pkgs.runCommand "caravan-src" { } ''
          cp -R ${lib.cleanSource ./.} "$out"
          chmod -R u+w "$out"
          substituteInPlace "$out/Cargo.toml" \
            --replace-fail 'mcp-cli = { git = "https://github.com/harryaskham/mcp-cli", branch = "main" }' \
                           'mcp-cli = { path = "${mcp-cli}" }' \
            --replace-fail 'updatable-cli = { git = "https://github.com/harryaskham/updatable-cli", branch = "main" }' \
                           'updatable-cli = { path = "${updatableCliPatched}" }' \
            --replace-fail 'feedback-cli = { git = "https://github.com/harryaskham/feedback-cli", branch = "main" }' \
                           'feedback-cli = { path = "${feedbackCliPatched}" }'
          # Direct Cargo records these as git packages. The Nix source rewrite
          # turns them into path packages, whose lock entries carry no source.
          ${pkgs.gnused}/bin/sed -i \
            '\#^source = "git+https://github.com/harryaskham/\(mcp-cli\|updatable-cli\|feedback-cli\)#d' \
            "$out/Cargo.lock"
        '';

        caravan = pkgs.rustPlatform.buildRustPackage {
          pname = "caravan";
          version = "0.0.24";
          src = caravanSrc;

          cargoLock.lockFile = "${caravanSrc}/Cargo.lock";
          doCheck = true;

          nativeBuildInputs = [ pkgs.pkg-config ];
          nativeCheckInputs = [ pkgs.gitMinimal ];
          buildInputs = darwinLibs;

          meta = {
            description = "Agent-in-the-loop GitHub merge queue";
            license = lib.licenses.mit;
            mainProgram = "cara";
          };
        };

        # bd-0629ce: the published Linux release asset must not depend on the
        # host's dynamic loader. A glibc-dynamic build segfaults on NixOS when
        # nix-ld substitutes a different glibc, and it does so with empty
        # stdout/stderr, so a crashing Cara looks like a quiet queue. Linux
        # release artifacts are therefore statically linked against musl.
        muslCaravan =
          let
            muslPkgs =
              if system == "x86_64-linux" then
                pkgs.pkgsCross.musl64
              else if system == "aarch64-linux" then
                pkgs.pkgsCross.aarch64-multiplatform-musl
              else
                null;
          in
          if muslPkgs == null then
            null
          else
            muslPkgs.rustPlatform.buildRustPackage {
              pname = "caravan-static";
              version = "0.0.10";
              src = caravanSrc;

              cargoLock.lockFile = "${caravanSrc}/Cargo.lock";
              # Cross/static toolchains cannot run the host-linked test binaries
              # here; the native package above keeps `doCheck = true` and the
              # release lane still runs the complete suite before publishing.
              doCheck = false;

              nativeBuildInputs = [ pkgs.pkg-config ];

              RUSTFLAGS = "-C target-feature=+crt-static";

              meta = {
                description = "Agent-in-the-loop GitHub merge queue (static musl)";
                license = lib.licenses.mit;
                mainProgram = "cara";
              };
            };
      in
      {
        packages = {
          default = caravan;
          caravan = caravan;
        }
        // lib.optionalAttrs (muslCaravan != null) { caravan-static = muslCaravan; };

        apps.default = {
          type = "app";
          program = "${caravan}/bin/cara";
        };

        checks = {
          default = caravan;
          workflow-lint = pkgs.runCommand "caravan-workflow-lint" {
            nativeBuildInputs = [
              pkgs.actionlint
              pkgs.shellcheck
            ];
          } ''
            cp -R ${lib.cleanSource ./.} source
            chmod -R u+w source
            cd source
            ${pkgs.bash}/bin/bash ./scripts/check-workflows.sh
            touch "$out"
          '';
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ caravan ];
          packages = with pkgs; [
            cargo
            python3
            rustc
            rustfmt
            clippy
            rust-analyzer
            git
            gh
            actionlint
            shellcheck
          ];
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
