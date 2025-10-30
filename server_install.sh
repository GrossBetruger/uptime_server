#!/usr/bin/env bash
set -e

# 2) System deps
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev clang cmake git curl snapd

# 3) Rust (non-interactive)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc




