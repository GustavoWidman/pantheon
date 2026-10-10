{
  lib,
  rustPlatform,
  stdenv,
  cacert,
  python3,
  poppler-utils,
}:
rustPlatform.buildRustPackage {
  pname = "pantheon-unwrapped";
  version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version;
  # Only inputs consumed by Cargo belong in the Rust compilation cache key.
  # Python/browser code and documentation are validated separately and packaged
  # by package.nix, without recompiling this executable.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../skills
      (lib.fileset.fileFilter (file: file.hasExt "rs") ../tests)
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  nativeCheckInputs = [
    python3
    poppler-utils
  ];
  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
  # cargo test cannot use panic=abort, so release tests rebuild dependencies
  # anyway. Use Cargo's normal test profile instead of paying for thin LTO and
  # codegen-units=1 a second time; the installed binary remains release optimized.
  checkType = "debug";
  cargoTestFlags = [ "--all-targets" ];
  # --all-targets omits doctests; retain the old default cargo test coverage too.
  postCheck = ''
    cargo test --doc --offline --target ${stdenv.hostPlatform.rust.rustcTarget}
  '';
  meta = {
    description = "Unwrapped Pantheon executable";
    license = lib.licenses.mit;
    mainProgram = "pantheon";
    platforms = lib.platforms.linux;
  };
}
