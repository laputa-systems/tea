# Linux AArch64 verification image. Build it with --platform=linux/arm64.
#
# This deliberately follows the musl/LLVM toolchain used by ~/d/e: there is no
# glibc, GCC, or libstdc++ in the image. Luau's C++ build uses Alpine's LLVM
# libc++ and the prebuilt musl LLVM runtime for compiler builtins.
FROM alpine:3.24.1

ARG TARGETARCH
ARG LLVM_VERSION=23.1.0-rc2
ARG LLVM_RELEASE_SHA=6eb5fb9
ARG TEA_RELEASE_GIT_SHA

RUN apk add --no-cache \
      bash \
      libc++ \
      libc++-dev \
      libc++-static \
      libunwind \
      make \
      musl-dev

ADD https://github.com/laputa-systems/llvm-prebuilt-musl/releases/download/llvm-musl-${LLVM_VERSION}-${LLVM_RELEASE_SHA}/clang+llvm-${LLVM_VERSION}-x86_64-linux-musl.tar.xz /tmp/llvm-x86_64.tar.xz
ADD https://github.com/laputa-systems/llvm-prebuilt-musl/releases/download/llvm-musl-${LLVM_VERSION}-${LLVM_RELEASE_SHA}/clang+llvm-${LLVM_VERSION}-aarch64-linux-musl.tar.xz /tmp/llvm-aarch64.tar.xz

RUN case "$TARGETARCH" in \
        amd64) archive=/tmp/llvm-x86_64.tar.xz ;; \
        arm64) archive=/tmp/llvm-aarch64.tar.xz ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && echo '36647cca0bf57d206a6ce757d07a9d8489ef6ccf283a2cc7f740d1cba99a088b  /tmp/llvm-x86_64.tar.xz' | sha256sum -c - \
    && echo '0c9bd6f0fefa26dbdb7d6ed568f3799b558428b1ce1264656aa328fc6fd9e32d  /tmp/llvm-aarch64.tar.xz' | sha256sum -c - \
    && mkdir -p /opt/llvm-musl \
    && tar xf "$archive" -C /opt/llvm-musl --strip-components=1 \
    && test -x /opt/llvm-musl/bin/clang \
    && test -x /opt/llvm-musl/bin/llvm-ar \
    && rm /tmp/llvm-x86_64.tar.xz /tmp/llvm-aarch64.tar.xz

# Rust and Luau need the compiler-rt builtins archive for __clear_cache and
# related low-level symbols. The archive is LLVM-built; no libgcc is needed.
RUN case "$TARGETARCH" in \
        amd64) llvm_arch=x86_64 ;; \
        arm64) llvm_arch=aarch64 ;; \
        *) echo "unsupported Alpine architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && clang_major="${LLVM_VERSION%%.*}" \
    && builtins="/opt/llvm-musl/lib/clang/$clang_major/lib/linux/libclang_rt.builtins-$llvm_arch.a" \
    && test -f "$builtins" \
    && ln -sf "$builtins" /usr/lib/libcompiler-rt-builtins.a \
    && ln -sf /usr/lib/libcompiler-rt-builtins.a /usr/lib/libgcc_s.so \
    && ln -sf /usr/lib/libunwind.so.1 /usr/lib/libgcc_s.so.1 \
    && /opt/llvm-musl/bin/llvm-ar rcs /usr/lib/libutil.a

# The musl target expects these compiler-generated startup objects. They are
# intentionally empty: Rust and the C++ runtime provide the actual entrypoint.
RUN case "$TARGETARCH" in \
        amd64) llvm_arch=x86_64 ;; \
        arm64) llvm_arch=aarch64 ;; \
        *) echo "unsupported Alpine architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && case "$TARGETARCH" in \
         amd64) rust_arch=x86_64 ;; \
         arm64) rust_arch=aarch64 ;; \
         *) echo "unsupported Alpine architecture: $TARGETARCH" >&2; exit 1 ;; \
       esac \
    && for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do \
          stub_dir="/usr/lib/e-crt/$target"; \
          mkdir -p "$stub_dir"; \
          for obj in crtbegin.o crtbeginS.o crtbeginT.o crtend.o crtendS.o; do \
              /opt/llvm-musl/bin/clang --target="$target" -x c -c /dev/null -o "$stub_dir/$obj"; \
          done; \
          if [ "$target" = "$rust_arch-unknown-linux-musl" ]; then \
              for obj in crtbegin.o crtbeginS.o crtbeginT.o crtend.o crtendS.o; do \
                  ln -sf "$stub_dir/$obj" "/usr/lib/$obj"; \
              done; \
          fi; \
      done

ADD https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-musl/rustup-init /rustup-init-x86_64
ADD https://static.rust-lang.org/rustup/dist/aarch64-unknown-linux-musl/rustup-init /rustup-init-aarch64
RUN case "$TARGETARCH" in \
        amd64) init=/rustup-init-x86_64 ;; \
        arm64) init=/rustup-init-aarch64 ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && cp "$init" /rustup-init \
    && chmod +x /rustup-init \
    && /rustup-init -y --default-toolchain none \
    && rm /rustup-init /rustup-init-x86_64 /rustup-init-aarch64

ENV PATH="/opt/llvm-musl/bin:/root/.cargo/bin:$PATH" \
    CC="/opt/llvm-musl/bin/clang" \
    CXX="/opt/llvm-musl/bin/clang++" \
    AR="/opt/llvm-musl/bin/llvm-ar" \
    RANLIB="/opt/llvm-musl/bin/llvm-ranlib" \
    CXXSTDLIB="c++" \
    CXXFLAGS="-nostdinc++ -isystem /usr/include/c++/v1" \
    RUSTFLAGS="-C link-arg=-Wl,-Bstatic -C link-arg=-lc++abi -C link-arg=-lcompiler-rt-builtins" \
    LIBRARY_PATH="/opt/llvm-musl/lib:/usr/lib:/usr/lib/e-crt/aarch64-unknown-linux-musl:/usr/lib/e-crt/x86_64-unknown-linux-musl" \
    LIBCLANG_PATH="/opt/llvm-musl/lib" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="/opt/llvm-musl/bin/clang" \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="/opt/llvm-musl/bin/clang"

RUN rustup toolchain install nightly-2026-07-24 \
      --target x86_64-unknown-linux-musl \
      --target aarch64-unknown-linux-musl \
      --component rust-src \
      --component llvm-tools-preview

RUN host_libdir="$(rustc --print target-libdir)" \
    && ln -sf /usr/lib/libc.so "$host_libdir/libc.so"

WORKDIR /src
COPY . .

# Keep the container's verification contract identical to the host's. The
# fixture manifest checker is Python and the workspace-isolation fixtures use
# real Git repositories, so install both runtimes only for this build step and
# remove them (including Python's libgcc/libstdc++ dependencies) from the
# resulting verification image.
RUN apk add --no-cache python3 git \
    && TEA_RELEASE_GIT_SHA="$TEA_RELEASE_GIT_SHA" make test \
    && apk del --no-network python3 python3-pyc pyc python3-pycache-pyc0 git \
    && test ! -e /usr/bin/python3 \
    && test ! -e /usr/bin/git \
    && test ! -e /usr/lib/libstdc++.so.6 \
    && ln -sf /usr/lib/libunwind.so.1 /usr/lib/libgcc_s.so.1
