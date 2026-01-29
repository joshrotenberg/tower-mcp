# Build stage
FROM rust:1.85-slim-bookworm AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy the workspace
COPY . .

# Build the release binary
RUN cargo build --release -p crates-mcp

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /app/target/release/crates-mcp /usr/local/bin/crates-mcp

# Create non-root user
RUN useradd -m -u 1000 app
USER app

# Expose port
EXPOSE 3000

# Run the server
CMD ["crates-mcp", "--transport", "http", "--host", "0.0.0.0", "--port", "3000", "--max-concurrent", "5", "--request-timeout-secs", "30"]
