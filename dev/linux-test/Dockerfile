# Test Zerobrew on a clean Linux environment (Fedora)
FROM fedora:43

# Install minimum dependencies
RUN dnf install -y git curl file xz findutils procps-ng binutils gcc openssl-devel

# Install rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Set up workdir
WORKDIR /zerobrew

# Copy the source code
COPY . .

# Build Zerobrew
RUN cargo build --release

# Initialize Zerobrew
RUN ./target/release/zb init
ENV PATH="~/.zerobrew/prefix/bin:${PATH}"

CMD ["/bin/bash"]
