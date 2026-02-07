# E2E test environment for Zed WebSocket sync
# Provides: Zed (built from source), ACP agents, xvfb, software Vulkan
#
# Usage:
#   docker build -t zed-websocket-e2e -f e2e-test/Dockerfile ../..
#   docker run --rm zed-websocket-e2e

FROM ubuntu:25.10 AS base

ENV DEBIAN_FRONTEND=noninteractive
ENV CARGO_HOME=/usr/local/cargo
ENV RUSTUP_HOME=/usr/local/rustup
ENV PATH="/usr/local/cargo/bin:${PATH}"

# System deps for Zed build + headless testing
RUN apt-get update && apt-get install -y \
    # Build essentials
    build-essential cmake pkg-config \
    # Zed system dependencies
    libxkbcommon-dev libwayland-dev libvulkan-dev \
    libx11-xcb-dev libxcb-shape0-dev libxcb-xfixes0-dev \
    libxcb-randr0-dev libxcb-present-dev libxcb-sync-dev \
    libxcb-render0-dev libxcb-composite0-dev libxcb-damage0-dev \
    libxcb-shm0-dev libxcb-glx0-dev libxcb-xkb-dev libxcb-dpms0-dev \
    libxcb-record0-dev libxcb-dri2-0-dev libxcb-dri3-dev \
    libfontconfig-dev libfreetype-dev \
    libssl-dev libsqlite3-dev libzstd-dev \
    # Headless display
    xvfb x11-utils \
    # Software Vulkan (lavapipe)
    mesa-vulkan-drivers mesa-utils \
    # Networking
    curl wget git \
    # Runtime
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable --profile minimal

# Install Node.js (needed for some ACP agents)
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - && \
    apt-get install -y nodejs && rm -rf /var/lib/apt/lists/*

# Install Claude Code (ACP agent)
RUN npm install -g @anthropic-ai/claude-code || true

# Copy Zed source
WORKDIR /build/zed
COPY . .

# Build Zed with WebSocket sync feature
RUN cargo build --release -p zed --features external_websocket_sync 2>&1 | tail -5

# Build the mock server test binary
RUN cargo test -p external_websocket_sync --features test-support --no-run 2>&1 | tail -5

# Runtime stage
FROM base AS test

WORKDIR /test

# Copy built artifacts
COPY --from=base /build/zed/target/release/zed /usr/local/bin/zed
COPY --from=base /build/zed/crates/external_websocket_sync/e2e-test/run_e2e.sh /test/run_e2e.sh
COPY --from=base /build/zed/crates/external_websocket_sync/PROTOCOL_SPEC.md /test/PROTOCOL_SPEC.md

# Set up headless display
ENV DISPLAY=:99
ENV VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
ENV LIBGL_ALWAYS_SOFTWARE=1

# Create a minimal project directory for Zed to open
RUN mkdir -p /test/project && echo "# Test Project" > /test/project/README.md

# API keys are passed as build args or env vars at runtime
# docker run -e ANTHROPIC_API_KEY=... zed-websocket-e2e

ENTRYPOINT ["/test/run_e2e.sh"]
