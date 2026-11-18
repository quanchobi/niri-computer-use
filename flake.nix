{
  description = "MCP server for AI agents to observe and drive a niri Wayland desktop";

  # nixos-unstable: the crate needs Rust 1.99, newer than the latest stable
  # NixOS release ships.
  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      # The architectures there are release builds for.
      forAllSystems = lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = self.packages.${system}.niri-computer-use;

          niri-computer-use = pkgs.rustPlatform.buildRustPackage {
            pname = "niri-computer-use";
            inherit ((lib.importTOML ./Cargo.toml).package) version;

            # The harness is a workspace member, so cargo needs its manifest
            # and sources to load the workspace.
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./harness
                ./skills
                ./noctalia/niri-computer-use
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;

            cargoBuildFlags = [
              "--package"
              "niri-computer-use"
            ];
            # `make test` runs the tests. Even the unit tests need the host: files
            # root owns, programs in /usr/bin and a niri to drive, which the build
            # sandbox doesn't have.
            doCheck = false;

            buildInputs = [ pkgs.libxkbcommon ];

            # The same layout the install script gives ~/.local/share.
            postInstall = ''
              mkdir -p $out/share/niri-computer-use/noctalia
              cp -r skills $out/share/niri-computer-use/
              cp -r noctalia/niri-computer-use $out/share/niri-computer-use/noctalia/
            '';

            meta = {
              description = "MCP server for AI agents to observe and drive a niri Wayland desktop";
              homepage = "https://ayagmar.github.io/niri-computer-use/";
              license = lib.licenses.mit;
              platforms = lib.platforms.linux;
              mainProgram = "niri-computer-use";
            };
          };
        }
      );
    };
}
