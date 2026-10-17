{
  description = "Zoom C5.1t Reverse Engineering Environment";

  inputs = {
    nixpkgs.url = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [
            "x86_64-unknown-linux-gnu"
            "i686-unknown-linux-gnu"
            "x86_64-pc-windows-gnu"
            "i686-pc-windows-gnu"
          ];
        };
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
            rustToolchain
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

        devShells.win64 = pkgs.mkShell {
          name = "zfx-win64-cross-env";
          nativeBuildInputs = [
            rustToolchain
            pkgs.python3
            pkgs.pkgsCross.mingwW64.stdenv.cc
          ];
          shellHook = ''
            export CC_x86_64_pc_windows_gnu="x86_64-w64-mingw32-gcc"
            export CXX_x86_64_pc_windows_gnu="x86_64-w64-mingw32-g++"
            export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="x86_64-w64-mingw32-gcc"
            export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-L native=${pkgs.pkgsCross.mingwW64.windows.pthreads}/lib -L native=${pkgs.pkgsCross.mingwW64.windows.mcfgthreads}/lib -C link-arg=-lmcfgthread"
            echo "Zoom ZFX Windows 64-bit Cross-Compilation Environment Loaded!"
          '';
        };

        devShells.win32 = pkgs.mkShell {
          name = "zfx-win32-cross-env";
          nativeBuildInputs = [
            rustToolchain
            pkgs.python3
            pkgs.pkgsCross.mingw32.stdenv.cc
          ];
          shellHook = ''
            export CC_i686_pc_windows_gnu="i686-w64-mingw32-gcc"
            export CXX_i686_pc_windows_gnu="i686-w64-mingw32-g++"
            export CARGO_TARGET_I686_PC_WINDOWS_GNU_LINKER="i686-w64-mingw32-gcc"
            export CARGO_TARGET_I686_PC_WINDOWS_GNU_RUSTFLAGS="-L native=${pkgs.pkgsCross.mingw32.windows.pthreads}/lib -L native=${pkgs.pkgsCross.mingw32.windows.mcfgthreads}/lib -C link-arg=-lmcfgthread"
            echo "Zoom ZFX Windows 32-bit Cross-Compilation Environment Loaded!"
          '';
        };
      }
    );
}
