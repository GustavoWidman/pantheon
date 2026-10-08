{
  description = "Pantheon: durable Rust agents and an always-on Discord service";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/ec2d622de0773551768cf98f3fc50cbcc003b9c5";
  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      packagesFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          browser = pkgs.callPackage ./nix/browser.nix { };
          rustBinary = pkgs.callPackage ./nix/rust.nix { };
          package = pkgs.callPackage ./nix/package.nix { inherit browser rustBinary; };
        in
        {
          default = package;
          pantheon = package;
          browser-runtime = browser;
        };
    in
    {
      packages = eachSystem packagesFor;
      nixosModules.default = import ./nix/module.nix self;
      devShells = eachSystem (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          browser = (packagesFor system).browser-runtime;
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.pkg-config
              pkgs.nixfmt
              pkgs.bash
              pkgs.coreutils
              pkgs.git
              pkgs.ripgrep
              pkgs.curl
              pkgs.codex
              pkgs.findutils
              browser
            ];
            shellHook = ''
              export PANTHEON_BROWSER_PYTHON="${browser}/bin/pantheon-browser-python"
              export PANTHEON_BROWSER_WORKER="${self}/scripts/browser-worker.py"
              export PANTHEON_CAMOUFOX="${browser}/libexec/camoufox/camoufox-bin"
              export PANTHEON_NOVNC_WEB="${pkgs.novnc}/share/webapps/novnc"
              export PANTHEON_XVFB="${pkgs.xvfb}/bin/Xvfb"
              export PANTHEON_X11VNC="${pkgs.x11vnc}/bin/x11vnc"
            '';
          };
        }
      );
      formatter = eachSystem (system: (import nixpkgs { inherit system; }).nixfmt);
      checks = eachSystem (system: {
        pantheon = (packagesFor system).pantheon;
        browser-runtime = (packagesFor system).browser-runtime;
      });
    };
}
