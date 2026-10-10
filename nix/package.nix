{
  lib,
  symlinkJoin,
  makeWrapper,
  cacert,
  rustBinary,
  browser,
  bash,
  coreutils,
  git,
  ripgrep,
  curl,
  findutils,
  codex,
  poppler-utils,
}:
symlinkJoin {
  pname = "pantheon";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version;
  paths = [ rustBinary ];
  nativeBuildInputs = [ makeWrapper ];
  postBuild = ''
    mkdir -p "$out/libexec/pantheon"
    cp ${../scripts/browser-worker.py} "$out/libexec/pantheon/browser-worker.py"
    wrapProgram "$out/bin/pantheon" \
      --set-default SSL_CERT_FILE "${cacert}/etc/ssl/certs/ca-bundle.crt" \
      --set PANTHEON_CODEX_CLI "${codex}/bin/codex" \
      --set PANTHEON_PDFTOTEXT "${poppler-utils}/bin/pdftotext" \
      --set PANTHEON_PDFTOPPM "${poppler-utils}/bin/pdftoppm" \
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
          codex
          poppler-utils
        ]
      }" \
      --set PANTHEON_BROWSER_WORKER "$out/libexec/pantheon/browser-worker.py"
  '';
  passthru.unwrapped = rustBinary;
  meta = {
    description = "Durable always-on Discord agent harness";
    license = lib.licenses.mit;
    mainProgram = "pantheon";
    platforms = lib.platforms.linux;
  };
}
