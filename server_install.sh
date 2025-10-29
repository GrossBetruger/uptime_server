#!/usr/bin/env bash
set -e

# 2) System deps
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev clang cmake git curl snapd

# 3) Rust (non-interactive)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# 4) PATH for Cargo + snap, for both bash and zsh; load now too
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc 2>/dev/null || true
echo 'export PATH="$PATH:/snap/bin"' >> ~/.bashrc
echo 'export PATH="$PATH:/snap/bin"' >> ~/.zshrc 2>/dev/null || true
source ~/.bashrc 2>/dev/null || true
source ~/.zshrc 2>/dev/null || true
export PATH="$HOME/.cargo/bin:$PATH:/snap/bin"

# 5) Clone and run the server
git clone https://github.com/GrossBetruger/uptime_server.git
cd uptime_server/
nohup cargo run --release > "$HOME/uptime_server_server.log" 2>&1 &

# 6) ngrok via snap
sudo snap install core
sudo snap install ngrok

# 7) Prompt for ngrok token and set it
read -rp "Enter your ngrok authtoken (or leave blank to skip): " NGROK_TOKEN
if [ -n "$NGROK_TOKEN" ]; then
  ngrok config add-authtoken "$NGROK_TOKEN"
fi

# 8) Start tunnel
nohup ngrok http 3000 > "$HOME/uptime_server_ngrok.log" 2>&1 &

# 9) Quick tails
uname -a
echo "Server logs:   $HOME/uptime_server_server.log"
echo "ngrok logs:    $HOME/uptime_server_ngrok.log"
echo "Tip: run 'tail -f $HOME/uptime_server_ngrok.log' to see the public URL."

