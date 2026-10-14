#!/usr/bin/env bash
# Fetch Cloudflare's published edge ranges and write them where the errex
# deploy reads them.
#
#   cloudflare-ips.sh geo  > /etc/nginx/snippets/cloudflare-origin-geo.conf
#   cloudflare-ips.sh ufw          # prints the ufw commands, runs nothing
#   cloudflare-ips.sh list         # one range per line
#
# The same list is compiled into errexd (crates/errexd/src/client_ip.rs) and
# sits in /etc/nginx/conf.d/cloudflare-trust.conf on wp-vps. When Cloudflare
# publishes a change, refresh all three.
set -euo pipefail

fetch() {
    local url=$1 body
    body=$(curl -fsS --max-time 20 "$url")
    # Refuse anything that is not a plain list of CIDRs.
    if [ -z "$body" ] || grep -Eqv '^[0-9a-fA-F:.]+/[0-9]{1,3}$' <<<"$body"; then
        echo "cloudflare-ips: unexpected content from $url" >&2
        exit 1
    fi
    printf '%s\n' "$body"
}

ranges() {
    fetch https://www.cloudflare.com/ips-v4
    fetch https://www.cloudflare.com/ips-v6
}

case "${1:-}" in
    geo)
        echo "# Cloudflare edge ranges, $(date -u +%F) from https://www.cloudflare.com/ips-v{4,6}"
        ranges | sed 's/$/ 1;/'
        ;;
    ufw)
        # wp-vps has no public IPv4, so Cloudflare reaches it over IPv6 only.
        fetch https://www.cloudflare.com/ips-v6 | while read -r r; do
            echo "sudo ufw allow proto tcp from $r to any port 443 comment 'cloudflare to errex'"
        done
        ;;
    list)
        ranges
        ;;
    *)
        echo "usage: $0 geo|ufw|list" >&2
        exit 2
        ;;
esac
