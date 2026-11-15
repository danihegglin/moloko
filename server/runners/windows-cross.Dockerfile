# moloko's runner for `windows-*` jobs: Windows can't run in a container on a Mac or
# Linux host, so these jobs run on Ubuntu with a toolchain that cross-compiles to
# x86_64-pc-windows-msvc (cargo-xwin + clang/lld), plus NSIS for Tauri installers.
FROM catthehacker/ubuntu:act-22.04

ARG CARGO_XWIN_VERSION=0.23.1

RUN apt-get update \
 && apt-get install -y --no-install-recommends nsis clang lld llvm xdg-utils \
 && rm -rf /var/lib/apt/lists/* \
 && for t in clang-cl lld-link llvm-lib llvm-rc llvm-dlltool llvm-ar; do \
      src=$(ls /usr/lib/llvm-*/bin/$t 2>/dev/null | sort -V | tail -n1 || true); \
      if [ ! -e "/usr/bin/$t" ] && [ -n "$src" ]; then ln -s "$src" "/usr/bin/$t"; fi; \
    done \
 # clang-cl is clang in MSVC-compatible mode, selected by the name it's called with.
 && { [ -e /usr/bin/clang-cl ] || ln -s "$(readlink -f /usr/bin/clang)" /usr/bin/clang-cl; }

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
      --default-toolchain stable --target x86_64-pc-windows-msvc \
 && curl -fsSL "https://github.com/rust-cross/cargo-xwin/releases/download/v${CARGO_XWIN_VERSION}/cargo-xwin-v${CARGO_XWIN_VERSION}.$(uname -m)-unknown-linux-musl.tar.gz" \
      | tar xz -C /usr/local/cargo/bin cargo-xwin \
 && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"
