{
  description = "Zoom C5.1t Reverse Engineering Environment";

  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            unzip
            p7zip
            innoextract
            cabextract
            ghidra
            radare2
            binutils
            wineWow64Packages.stable
            (python3.withPackages (ps: [ ps.pyusb ps.python-rtmidi ps.mido ]))
            cargo
            rustc
            rustfmt
            clippy
            clang
            pkg-config
            libusb1
            libx11
            libxcursor
            libxrandr
            libxi
            libglvnd
            libxkbcommon
          ];

          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath [
              pkgs.libx11
              pkgs.libxcursor
              pkgs.libxrandr
              pkgs.libxi
              pkgs.libglvnd
              pkgs.libxkbcommon
            ]}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            echo "Zoom C5.1t Reverse Engineering Environment Loaded!"
            echo "Available tools: ghidra, radare2, strings, unzip, 7z, innoextract, cabextract, wine, python3, clippy"
          '';
        };

        devShells.shell32 = pkgs.pkgsi686Linux.mkShell {
          name = "zfx-32bit-build-env";
          buildInputs = with pkgs.pkgsi686Linux; [
            cargo
            rustc
            rustfmt
            clippy
            clang
            pkg-config
            libx11
            libxcursor
            libxrandr
            libxi
            libGL
            python3
          ];
          shellHook = ''
            echo "Zoom ZFX 32-bit (i686) Build Environment Loaded!"
          '';
        };
      }
    );
}
