# Stage 1: Build the binary on Alpine
FROM alpine:latest AS builder

# Install dependencies
RUN apk update && apk add --no-cache \
    bash \
    build-base \
    curl \
    git \
    musl-dev \
    openssl-dev \
    pkgconfig \
    perl

# Install Rust using rustup
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Set the working directory
WORKDIR /usr/src/app

# Copy the source code into the container
COPY . .

# Build the binary using the Makefile
RUN make release

# Stage 2: Create a minimal runtime image with Alpine
FROM alpine:latest

# Install necessary runtime dependencies
RUN apk add --no-cache \
    bash \
    musl

# Copy the binary from the builder stage
COPY --from=builder /usr/src/app/target/debug/your_binary /usr/local/bin/your_binary

# Optional: Set the entrypoint if you want to run the binary directly
# ENTRYPOINT ["/usr/local/bin/your_binary"]

# This stage is used to export the binary
FROM scratch AS export-stage
COPY --from=builder /usr/src/app/target/debug/your_binary /your_binary

