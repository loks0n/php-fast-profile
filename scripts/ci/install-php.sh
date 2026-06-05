#!/usr/bin/env bash
# Install the Sury-packaged PHP 8.3 CLI on a Debian/Ubuntu runner.
set -euo pipefail

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  curl ca-certificates gnupg lsb-release
curl -sSLo /tmp/sury.gpg https://packages.sury.org/php/apt.gpg
sudo install -m 644 /tmp/sury.gpg /usr/share/keyrings/sury.gpg
# shellcheck disable=SC1091  # runtime file, not present at lint time
. /etc/os-release
echo "deb [signed-by=/usr/share/keyrings/sury.gpg] https://packages.sury.org/php/ ${VERSION_CODENAME} main" \
  | sudo tee /etc/apt/sources.list.d/sury.list
sudo apt-get update
sudo apt-get install -y --no-install-recommends php8.3-cli
