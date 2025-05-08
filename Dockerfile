# Builder stage
FROM rust:1.86 as builder

# Install build dependencies
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Create new empty shell project
WORKDIR /app

COPY Cargo.toml ./

# Create a dummy main.rs for dependency caching
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs

# Build dependencies only - this is the caching Docker layer
RUN cargo build --release

# Remove the dummy source
RUN rm src/main.rs

# Now copy the real source code
COPY src ./src

# Build the real application
RUN touch src/main.rs && \
    cargo build --release && \
    strip /app/target/release/tx_callbacks

# Runtime stage using distroless
FROM gcr.io/distroless/cc-debian12

# Copy SSL certificates for HTTPS support
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/tx_callbacks .

# Set environment variables
ENV RUST_LOG=info
ENV RUST_LOG=api=debug,tower_http=debug

# Expose port
EXPOSE 5000

ENTRYPOINT ["./tx_callbacks"]