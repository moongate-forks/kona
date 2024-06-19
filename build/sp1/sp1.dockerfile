FROM --platform=linux/amd64 ubuntu:22.04

ENV SHELL=/bin/bash
ENV DEBIAN_FRONTEND noninteractive

# todo: pin `nightly` version
ENV RUST_VERSION nightly

RUN apt-get update && apt-get install --assume-yes --no-install-recommends \
  ca-certificates \
  build-essential \
  curl \
  llvm \
  clang \
  make \
  cmake \
  git 



# Install Rustup and Rust
RUN curl https://sh.rustup.rs -sSf | bash -s -- -y --default-toolchain ${RUST_VERSION} --component rust-src
ENV PATH="/root/.cargo/bin:${PATH}"

# Install the Succinct toolchain
RUN curl -L https://sp1.succinct.xyz | bash
RUN /root/.sp1/bin/sp1up

# Install the RISC-V toolchain for gcc, used for compiling libraries with C dependencies.
RUN mkdir -p /opt/riscv
RUN wget -O /tmp/riscv32-unknown-elf.gcc-13.2.0.tar.gz https://github.com/stnolting/riscv-gcc-prebuilt/releases/download/rv32i-131023/riscv32-unknown-elf.gcc-13.2.0.tar.gz
RUN tar -xzf /tmp/riscv32-unknown-elf.gcc-13.2.0.tar.gz -C /opt/riscv/


# Set up the env vars to instruct rustc to use the correct compiler and linker
# and to build correctly to support the Cannon processor
ENV CC="gcc" \
  CC_riscv32im_succinct_zkvm_elf="/opt/riscv/bin/riscv32-unknown-elf-gcc -mstrict-align" \
  CXX_riscv64_unknown_none_elf=riscv64-linux-gnu-g++ \
  CARGO_TARGET_RISCV64_UNKNOWN_NONE_ELF_LINKER=riscv64-linux-gnu-gcc \
  RUSTFLAGS="-C passes=loweratomic -C link-arg=-Ttext=0x00200800 -C panic=abort" \
  CARGO_BUILD_TARGET="riscv32im-succinct-zkvm-elf" \
  RUSTUP_TOOLCHAIN="succinct"
