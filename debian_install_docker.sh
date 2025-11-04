#!/usr/bin/env bash
# Install Docker Engine on Debian 11/12/13 (amd64) using the official apt repository.
# Default: add user 'oren.koriat' to the 'docker' group for non-root usage.

set -euo pipefail

# ---- options ---------------------------------------------------------------
# You can override on invocation, e.g.:
#   DOCKER_NONROOT_USER=myuser sudo -E bash install-docker-debian.sh
DOCKER_NONROOT_USER="${DOCKER_NONROOT_USER:-oren.koriat}"

# ---- sanity checks ---------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
  echo "Re-executing with sudo..."
  exec sudo -E bash "$0" "$@"
fi

if [[ ! -r /etc/os-release ]]; then
  echo "Cannot read /etc/os-release; this script targets Debian. Aborting."
  exit 1
fi

# Determine Debian codename (bookworm, bullseye, trixie, etc.)
CODENAME="$((. /etc/os-release >/dev/null 2>&1; echo "${VERSION_CODENAME:-}") || true)"
if [[ -z "${CODENAME}" && $(command -v lsb_release >/dev/null 2>&1; echo $?) -eq 0 ]]; then
  CODENAME="$(lsb_release -cs || true)"
fi
if [[ -z "${CODENAME}" ]]; then
  echo "Could not determine Debian codename (VERSION_CODENAME). Set it manually by running:"
  echo "  CODENAME=bookworm bash $0"
  exit 1
fi
echo ">>> Detected Debian codename: ${CODENAME}"

# ---- remove conflicting packages (safe if absent) --------------------------
echo ">>> Removing conflicting packages (if any)..."
apt-get update -y
for pkg in docker.io docker-doc docker-compose podman-docker containerd runc; do
  apt-get remove -y "$pkg" >/dev/null 2>&1 || true
done

# ---- prerequisites ---------------------------------------------------------
echo ">>> Installing prerequisites..."
apt-get install -y ca-certificates curl gnupg

# ---- add Docker's official GPG key & apt source (Deb822) -------------------
echo ">>> Adding Docker apt repository..."
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc

cat >/etc/apt/sources.list.d/docker.sources <<EOF
Types: deb
URIs: https://download.docker.com/linux/debian
Suites: ${CODENAME}
Components: stable
Signed-By: /etc/apt/keyrings/docker.asc
EOF

apt-get update -y

# ---- install Docker Engine & friends ---------------------------------------
echo ">>> Installing Docker Engine, CLI, containerd, Buildx, and Compose plugin..."
apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# ---- enable & start service -------------------------------------------------
echo ">>> Enabling and starting docker.service..."
systemctl enable --now docker

# ---- verify installation ----------------------------------------------------
echo ">>> Docker version:"
docker --version || true

echo ">>> Running hello-world test image (may pull layers)..."
docker run --rm hello-world || {
  echo "Note: 'hello-world' failed to run. If this is a permissions error, see the non-root setup below."
}

# ---- add non-root user to 'docker' group -----------------------------------
if [[ -n "${DOCKER_NONROOT_USER}" ]]; then
  if id -u "${DOCKER_NONROOT_USER}" >/dev/null 2>&1; then
    echo ">>> Adding user '${DOCKER_NONROOT_USER}' to 'docker' group..."
    usermod -aG docker "${DOCKER_NONROOT_USER}"
    echo "User '${DOCKER_NONROOT_USER}' added to 'docker' group."
    echo "You must log out and back in (or run 'newgrp docker') for group changes to take effect."
  else
    echo "User '${DOCKER_NONROOT_USER}' not found; skipping docker group addition."
    echo "Create the user, then run: usermod -aG docker '${DOCKER_NONROOT_USER}'"
  fi
else
  echo "Skipping non-root setup (DOCKER_NONROOT_USER is empty)."
fi

echo ">>> Done. Docker Engine is installed."


