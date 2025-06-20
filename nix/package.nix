{
  lib,
  rustPlatform,
}: let
  fs = lib.fileset;
in
  rustPlatform.buildRustPackage (finalAttrs: {
    pname = "watt";
    version = (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version;

    src = fs.toSource {
      root = ../.;
      fileset = fs.unions [
        (fs.fileFilter (file: builtins.any file.hasExt ["rs"]) ../src)
        ../Cargo.lock
        ../Cargo.toml
      ];
    };

    cargoLock.lockFile = "${finalAttrs.src}/Cargo.lock";
    useFetchCargoVendor = true;
    enableParallelBuilding = true;

    meta = {
      description = "Automatic CPU speed & power optimizer for Linux";
      longDescription = ''
        Watt is a CPU speed & power optimizer for Linux. It uses
        the CPU frequency scaling driver to set the CPU frequency
        governor and the CPU power management driver to set the CPU
        power management mode.

      '';
      homepage = "https://github.com/NotAShelf/watt";
      mainProgram = "watt";
      license = lib.licenses.mpl20;
      platforms = lib.platforms.linux;
    };
  })
