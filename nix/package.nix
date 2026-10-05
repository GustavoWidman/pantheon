{
  lib,
  rustPlatform,
  makeWrapper,
  python3,
  browser,
  bash,
  coreutils,
  git,
  ripgrep,
  curl,
  findutils,
}:
rustPlatform.buildRustPackage {
  pname = "pantheon";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version;
  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ makeWrapper ];
  nativeCheckInputs = [ python3 ];
  postInstall = ''
    mkdir -p "$out/libexec/pantheon"
    cp scripts/browser-worker.py "$out/libexec/pantheon/browser-worker.py"
    wrapProgram "$out/bin/pantheon" \
      --set PANTHEON_BROWSER_PYTHON "${browser}/bin/pantheon-browser-python" \
      --set PANTHEON_NOVNC_WEB "${browser}/share/novnc" \
      --set PANTHEON_CAMOUFOX "${browser}/libexec/camoufox/camoufox-bin" \
      --set PANTHEON_XVFB "${browser}/bin/Xvfb" \
      --set PANTHEON_X11VNC "${browser}/bin/x11vnc" \
      --prefix PATH : "${
        lib.makeBinPath [
          browser
          bash
          coreutils
          git
          ripgrep
          curl
          findutils
        ]
      }" \
      --set PANTHEON_BROWSER_WORKER "$out/libexec/pantheon/browser-worker.py"
  '';
  meta = {
    description = "Durable always-on Discord agent harness";
    license = lib.licenses.mit;
    mainProgram = "pantheon";
    platforms = lib.platforms.linux;
  };
}
