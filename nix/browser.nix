{
  lib,
  stdenv,
  fetchurl,
  unzip,
  autoPatchelfHook,
  patchelfUnstable,
  makeWrapper,
  python3,
  xvfb,
  x11vnc,
  novnc,
  alsa-lib,
  at-spi2-atk,
  at-spi2-core,
  cairo,
  cups,
  dbus,
  fontconfig,
  freetype,
  gdk-pixbuf,
  glib,
  gtk3,
  libdrm,
  libgbm,
  libGL,
  libxkbcommon,
  libX11,
  libXcomposite,
  libXcursor,
  libXdamage,
  libXext,
  libXfixes,
  libXi,
  libXrandr,
  libXrender,
  libXt,
  libXtst,
  libxcb,
  libxshmfence,
  nspr,
  nss,
  pango,
  pciutils,
  zlib,
}:
let
  architecture = if stdenv.hostPlatform.isAarch64 then "arm64" else "x86_64";
  # GitHub release asset digests, fetched at build time and fixed by SHA256.
  sourceHashes = {
    x86_64 = "09effb44efec6f0617b6940b787054b4b8773f28d6573bcbabefe3163e75d0cc";
    arm64 = "c30d865d1ef14e33231a92d256cdb78b31933acb38f1b090a374a2db6c68e0b1";
  };
  driver = python3.withPackages (p: [
    p.playwright
    p.websockify
  ]);
  libraries = [
    alsa-lib
    at-spi2-atk
    at-spi2-core
    cairo
    cups
    dbus
    fontconfig
    freetype
    gdk-pixbuf
    glib
    gtk3
    libdrm
    libgbm
    libGL
    libxkbcommon
    libX11
    libXcomposite
    libXcursor
    libXdamage
    libXext
    libXfixes
    libXi
    libXrandr
    libXrender
    libXt
    libXtst
    libxcb
    libxshmfence
    nspr
    nss
    pango
    pciutils
    zlib
    stdenv.cc.cc.lib
  ];
in
stdenv.mkDerivation {
  pname = "pantheon-browser-runtime";
  version = "156.0.1-beta.34";
  src = fetchurl {
    url = "https://github.com/daijro/camoufox/releases/download/v156.0.1-beta.34/camoufox-156.0.1-beta.34-lin.${architecture}.zip";
    sha256 = sourceHashes.${architecture};
  };
  nativeBuildInputs = [
    unzip
    patchelfUnstable
    autoPatchelfHook
    makeWrapper
  ];
  # Firefox's custom ELF layout is corrupted by ordinary section relocation.
  # This matches nixpkgs' firefox-bin patching policy.
  patchelfFlags = [ "--no-clobber-old-sections" ];
  buildInputs = libraries;
  sourceRoot = ".";
  # Upstream font entries contain differing local/central ZIP filenames;
  # Python reads the authoritative central directory without unzip's fatal status.
  unpackPhase = ''
    ${python3}/bin/python3 -m zipfile -e "$src" .
  '';
  dontBuild = true;
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/libexec/camoufox" "$out/bin" "$out/share"
    ln -s ${novnc}/share/webapps/novnc "$out/share/novnc"
    ln -s ${xvfb}/bin/Xvfb "$out/bin/Xvfb"
    ln -s ${x11vnc}/bin/x11vnc "$out/bin/x11vnc"
    ln -s ${driver}/bin/websockify "$out/bin/websockify"
    cp -r . "$out/libexec/camoufox/"
    chmod +x "$out/libexec/camoufox/camoufox-bin"
    makeWrapper ${driver}/bin/python3 "$out/bin/pantheon-browser-python" \
      --set PANTHEON_CAMOUFOX "$out/libexec/camoufox/camoufox-bin" \
      --set PANTHEON_NOVNC_WEB "${novnc}/share/webapps/novnc" \
      --set PANTHEON_XVFB "${xvfb}/bin/Xvfb" \
      --set PANTHEON_X11VNC "${x11vnc}/bin/x11vnc" \
      --set PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD 1 \
      --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath libraries}"
    runHook postInstall
  '';
  postInstall = ''
    # Run after autoPatchelfPostFixup, rather than before setup-hook callbacks.
    checkCamoufoxExecutable() {
      "$out/libexec/camoufox/camoufox-bin" --version
    }
    postFixupHooks+=(checkCamoufoxExecutable)
  '';
  meta = {
    description = "Pinned Camoufox, Playwright driver and per-browser Xvfb/noVNC runtime";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    license = lib.licenses.mpl20;
  };
}
