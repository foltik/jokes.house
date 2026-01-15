{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    ffmpeg.dev
    pkg-config
    clang
    llvmPackages.libclang
  ];

  # ffmpeg-sys-next bindgen needs these
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

  # Tell ffmpeg-sys-next where to find headers
  FFMPEG_DIR = "${pkgs.ffmpeg.dev}";

  # Also set for bindgen
  BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.ffmpeg.dev}/include";
}
