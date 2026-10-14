#!/usr/bin/env bash
# Render the wp-vps deploy files for one hostname.
#
#   deploy/wp-vps/render.sh [HOSTNAME] [OUT_DIR]
#
# HOSTNAME defaults to $ERREX_HOSTNAME, else bugtracker.zeusmotorsport.com.
# OUT_DIR defaults to ./errex-deploy. Substitutes @ERREX_HOSTNAME@ in the
# *.in templates and copies the rest unchanged.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
host=${1:-${ERREX_HOSTNAME:-bugtracker.zeusmotorsport.com}}
out=${2:-$PWD/errex-deploy}

if ! [[ $host =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$ ]]; then
    echo "render: '$host' is not a hostname" >&2
    exit 1
fi

mkdir -p "$out/nginx" "$out/systemd"
sed "s/@ERREX_HOSTNAME@/$host/g" "$here/errexd.env.in" > "$out/errexd.env"
sed "s/@ERREX_HOSTNAME@/$host/g" "$here/nginx/errex.conf.in" > "$out/nginx/$host.conf"
cp "$here"/systemd/errexd.service "$here"/systemd/errex-backup.service \
    "$here"/systemd/errex-backup.timer "$out/systemd/"
install -m 0755 "$here/errex-backup.sh" "$here/cloudflare-ips.sh" "$out/"
if grep -rq '@ERREX_HOSTNAME@' "$out"; then
    echo "render: unsubstituted placeholder left in $out" >&2
    exit 1
fi
echo "render: $out ($host)"
