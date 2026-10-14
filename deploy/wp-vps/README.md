# errex on wp-vps

Error reports from the Zeus Flasher Suite app, collected by errexd on
wp-vps and reachable only through Cloudflare.

```
app (DSN) ──HTTPS──> Cloudflare edge ──HTTPS, Origin CA cert──> nginx [::]:443 ──> errexd [::1]:9090
                     │ Access: dashboard only                  │ answers Cloudflare's ranges only
                     │ bypass: /api/suite-{prod,beta}/envelope │ POST /api/<project>/envelope/ public
                                                               │ everything else needs the Access JWT
errexd ──> Telegram Bot API (alerts)          errex-backup.timer ──> /var/backups/errex (nightly)
```

| File | Installed as |
|---|---|
| `build-release.sh` | run on the build machine; produces a static `errexd` |
| `render.sh` | run on the build machine; fills in the hostname |
| `errexd.env.in` | `/etc/errex/errexd.env` |
| `systemd/errexd.service` | `/etc/systemd/system/errexd.service` |
| `systemd/errex-backup.{service,timer}`, `errex-backup.sh` | `/etc/systemd/system/`, `/usr/local/lib/errex/` |
| `nginx/errex.conf.in` | `/etc/nginx/sites-available/<host>.conf` |
| `cloudflare-ips.sh` | writes `/etc/nginx/snippets/cloudflare-origin-geo.conf`, prints the ufw rules |

The hostname is `bugtracker.zeusmotorsport.com` unless you pass another to
`render.sh` (or set `ERREX_HOSTNAME`). The steps below write it as `$H`.
Projects: `suite-prod` and `suite-beta`.

Everything on wp-vps needs `sudo`. Nothing here touches another site:
nginx gets one new server block, systemd three new units, ufw rules for
443 from Cloudflare.

## 0. On the build machine

```bash
export H=bugtracker.zeusmotorsport.com
export WP6=$(ssh -G wp-vps | awk '/^hostname /{print $2}')   # the origin's IPv6, never committed
cd errex                                   # this repository, branch feat/zeus-ready
CARGO_TARGET_DIR=/var/tmp/errex-target deploy/wp-vps/build-release.sh ./errex-release
deploy/wp-vps/render.sh "$H" ./errex-deploy
scp -r errex-release errex-deploy wp-vps:~/
```

`build-release.sh` builds the SPA and a statically linked binary. With the
musl target (`rustup target add x86_64-unknown-linux-musl`, `apt install
musl-tools`) the binary has no runtime dependencies; without it the script
builds static glibc, which is fine while the build machine runs glibc 2.39
like wp-vps (Ubuntu 24.04).

## 1. Telegram bot and chat id

1. In Telegram, open **@BotFather**, send `/newbot`, choose a name and a
   username ending in `bot`. BotFather replies with the token
   (`123456789:AA...`). Keep it out of shell history and chat logs.
2. Create the group (or channel) that should receive alerts and add the
   bot. For a **channel**, make the bot an administrator with *Post
   messages*. For a group, a plain member is enough.
3. Send a message in the group that mentions the bot, e.g.
   `/start@your_bot` (in a channel, just post anything), then read the id:

   ```bash
   read -rs TG && echo
   curl -s "https://api.telegram.org/bot$TG/getUpdates" \
     | jq '.result[] | (.message // .channel_post).chat | {id, type, title}'
   ```

   A group or channel id looks like `-1001234567890`. If the group was
   upgraded to a supergroup, use the newest id.
4. Check the bot can post there:

   ```bash
   curl -s "https://api.telegram.org/bot$TG/sendMessage" -d chat_id=-1001234567890 -d text="errex test"
   ```

## 2. Install errexd on wp-vps

```bash
ssh wp-vps
export H=bugtracker.zeusmotorsport.com
cd ~/errex-release && sha256sum -c errexd.sha256
sudo useradd --system --home-dir /var/lib/errex --shell /usr/sbin/nologin errex
sudo install -m 0755 -o root -g root errexd /usr/local/bin/errexd

sudo install -d -m 0750 -o root -g errex /etc/errex
sudo install -m 0640 -o root -g errex ~/errex-deploy/errexd.env /etc/errex/errexd.env
sudoedit /etc/errex/errexd.env            # set ERREX_TELEGRAM_CHAT_ID

# The bot token, readable by root only; systemd hands it to errexd.
read -rs TG && printf '%s\n' "$TG" | sudo install -m 0600 -o root -g root /dev/stdin /etc/errex/telegram_bot_token; unset TG

sudo install -d -m 0750 -o errex -g errex /var/backups/errex
sudo install -d -m 0755 /usr/local/lib/errex
sudo install -m 0755 ~/errex-deploy/errex-backup.sh /usr/local/lib/errex/errex-backup.sh
sudo install -m 0644 ~/errex-deploy/systemd/* /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now errexd.service errex-backup.timer
systemctl status errexd --no-pager
curl -sg 'http://[::1]:9090/health'       # {"status":"ok"}
```

To run without Telegram, comment out the `LoadCredential=` and
`ERREX_TELEGRAM_BOT_TOKEN_FILE` lines in the unit and leave
`ERREX_TELEGRAM_CHAT_ID` empty. errexd refuses to start with only half of
the Telegram settings.

### Projects

```bash
for p in suite-prod suite-beta; do
  sudo -u errex env ERREX_DATA_DIR=/var/lib/errex /usr/local/bin/errexd project add "$p" --public-url "https://$H"
done
```

Each prints the DSN (`https://<key>@$H/<project>`) that goes into the
app's runtime configuration (`diagnostics_dsn`) for that channel.
`errexd project list` shows them again; `errexd project rotate <name>`
replaces a key.

### First admin

Create the admin before the hostname exists, over an SSH tunnel:

```bash
T=$(openssl rand -hex 16); echo "setup token: $T"
echo "ERREX_ADMIN_TOKEN=$T" | sudo tee -a /etc/errex/errexd.env >/dev/null
sudo systemctl restart errexd
```

On the build machine: `ssh -N -L 9090:[::1]:9090 wp-vps`, open
<http://localhost:9090/setup> in Chrome or Firefox (they accept errex's
`Secure` cookie on localhost), paste the token, choose a username and a
strong password. Then remove the `ERREX_ADMIN_TOKEN=` line again
(`sudoedit /etc/errex/errexd.env`) and `sudo systemctl restart errexd`.

## 3. Cloudflare

All of this can be done in the dashboard or with the API. The API calls
read a scoped token from `~/.access-keys/cloudflare-token` on the build
machine and need these permissions on `zeusmotorsport.com`: *Zone · Zone ·
Read*, *Zone · DNS · Edit*, *Zone · SSL and Certificates · Edit*, *Zone ·
Config Rules · Edit*, and on the account *Access: Apps and Policies ·
Edit*.

```bash
export CF_API_TOKEN=$(cat ~/.access-keys/cloudflare-token)
API=https://api.cloudflare.com/client/v4
cf() { curl -sS -H "Authorization: Bearer $CF_API_TOKEN" -H "Content-Type: application/json" "$@"; }
cf "$API/user/tokens/verify" | jq .result.status                    # "active"
ZONE_ID=$(cf "$API/zones?name=zeusmotorsport.com" | jq -r '.result[0].id')
ACCOUNT_ID=$(cf "$API/zones/$ZONE_ID" | jq -r '.result.account.id')
```

Do the steps in this order: the record that makes the hostname resolve
comes last, after the certificate, nginx, the firewall, Access and the SSL
mode are in place.

### 3.1 Origin certificate

The private key is generated on wp-vps and never leaves it; only the CSR
and the certificate travel.

```bash
# on wp-vps
sudo install -d -m 0750 -o root -g root /etc/ssl/cloudflare
sudo openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "/etc/ssl/cloudflare/$H.key" -subj "/CN=$H" -out "/tmp/$H.csr"
sudo chmod 0600 "/etc/ssl/cloudflare/$H.key"
cat "/tmp/$H.csr"
```

**Dashboard:** *zeusmotorsport.com → SSL/TLS → Origin Server → Create
Certificate → Use my private key and CSR*, paste the CSR, hostnames `$H`
only, validity 15 years, *Create*. Copy the PEM.

**API** (on the build machine, with the CSR copied to `./$H.csr`):

```bash
cf -X POST "$API/certificates" --data "$(jq -n --arg h "$H" --rawfile csr "$H.csr" \
  '{hostnames: [$h], requested_validity: 5475, request_type: "origin-ecc", csr: $csr}')" \
  | jq -r .result.certificate > "$H.pem"
```

If this endpoint refuses the token, use the dashboard path. Then on
wp-vps: `sudo install -m 0644 <pem> /etc/ssl/cloudflare/$H.pem`.

### 3.2 nginx and the firewall (wp-vps)

```bash
sudo install -d /etc/nginx/snippets
~/errex-deploy/cloudflare-ips.sh geo | sudo tee /etc/nginx/snippets/cloudflare-origin-geo.conf >/dev/null
sudo install -m 0644 ~/errex-deploy/nginx/$H.conf /etc/nginx/sites-available/$H.conf
sudo ln -s /etc/nginx/sites-available/$H.conf /etc/nginx/sites-enabled/$H.conf
sudo nginx -t && sudo systemctl reload nginx

sudo ufw status verbose                      # check the default incoming policy is deny
~/errex-deploy/cloudflare-ips.sh ufw         # prints one `sudo ufw allow ... port 443` per Cloudflare IPv6 range
# review, then run the printed lines
```

The site relies on `/etc/nginx/conf.d/cloudflare-trust.conf` (already on
wp-vps) for `set_real_ip_from`; the site sets `real_ip_header
CF-Connecting-IP` itself. wp-vps has no public IPv4, so Cloudflare reaches
it over IPv6 and only IPv6 ranges are opened.

### 3.3 Access (dashboard login)

Zero Trust needs a team once: *Zero Trust → Settings* (free plan, up to
50 users). One-time PIN by e-mail is the default login method.

**Dashboard:** *Zero Trust → Access → Applications → Add an application
→ Self-hosted*:

1. *errex dashboard*: domain `$H`, path empty. Policy *Allow*, include
   *Emails* = the people who triage (the owner). Session 24 h.
2. *errex ingest*: domains `$H/api/suite-prod/envelope` and
   `$H/api/suite-beta/envelope`. Policy *Bypass*, include *Everyone*.
   The more specific path wins, so the app's POSTs never see a login page;
   errexd still requires the DSN key.

**API:**

```bash
OWNER_EMAIL=owner@example.com          # who may open the dashboard
ALLOW_ID=$(cf -X POST "$API/accounts/$ACCOUNT_ID/access/policies" --data "$(jq -n --arg e "$OWNER_EMAIL" \
  '{name: "errex admins", decision: "allow", include: [{email: {email: $e}}], session_duration: "24h"}')" | jq -r .result.id)
BYPASS_ID=$(cf -X POST "$API/accounts/$ACCOUNT_ID/access/policies" --data \
  '{"name": "errex ingest (public)", "decision": "bypass", "include": [{"everyone": {}}]}' | jq -r .result.id)
cf -X POST "$API/accounts/$ACCOUNT_ID/access/apps" --data "$(jq -n --arg d "$H" --arg p "$ALLOW_ID" \
  '{name: "errex dashboard", type: "self_hosted", domain: $d, session_duration: "24h",
    app_launcher_visible: false, policies: [{id: $p, precedence: 1}]}')" | jq '.success, .errors'
for p in suite-prod suite-beta; do
  cf -X POST "$API/accounts/$ACCOUNT_ID/access/apps" --data "$(jq -n --arg d "$H/api/$p/envelope" --arg p "$BYPASS_ID" --arg n "errex ingest $p" \
    '{name: $n, type: "self_hosted", domain: $d, app_launcher_visible: false, policies: [{id: $p, precedence: 1}]}')" | jq '.success, .errors'
done
```

nginx also refuses every non-ingest request that lacks Access's
`Cf-Access-Jwt-Assertion` header, so a missing or paused Access app shows
403, never the dashboard. errex's own login is the third layer.

**Instead of Access**, an IP allowlist: replace the `map
$http_cf_access_jwt_assertion` block in the site with
`geo $errex_dashboard_ok { default 0; 198.51.100.7 1; }` (your addresses;
after realip `$remote_addr` is the client's), `nginx -t`, reload. This
only works if those addresses are stable.

### 3.4 SSL mode Full (strict), for this hostname only

A Configuration Rule, so the zone-wide SSL mode (and every other host on
the zone) stays as it is.

**Dashboard:** *zeusmotorsport.com → Rules → Configuration Rules →
Create rule*: name *errex: SSL strict*, *Hostname equals* `$H`, *SSL* →
*Strict*, *Deploy*.

**API:**

```bash
RULE=$(jq -n --arg h "$H" '{description: "errex: SSL Full (strict)", expression: "(http.host eq \"\($h)\")",
  action: "set_config", action_parameters: {ssl: "strict"}}')
RS=$(cf "$API/zones/$ZONE_ID/rulesets/phases/http_config_settings/entrypoint" | jq -r '.result.id // empty')
if [ -n "$RS" ]; then
  cf -X POST "$API/zones/$ZONE_ID/rulesets/$RS/rules" --data "$RULE" | jq '.success, .errors'
else
  cf -X PUT "$API/zones/$ZONE_ID/rulesets/phases/http_config_settings/entrypoint" \
    --data "$(jq -n --argjson r "$RULE" '{rules: [$r]}')" | jq '.success, .errors'
fi
```

`POST .../rules` adds to an existing rule set; `PUT` only runs when the
zone has none, because a `PUT` replaces every rule in the phase.

### 3.5 DNS record (last)

An AAAA record, proxied (orange cloud). The origin address stays hidden
behind Cloudflare; do not publish it anywhere else.

**Dashboard:** *zeusmotorsport.com → DNS → Records → Add record*: type
*AAAA*, name `bugtracker` (the first label of `$H`), IPv6 address = wp-vps's
(`$WP6`), *Proxied* on, TTL Auto.

**API:**

```bash
cf -X POST "$API/zones/$ZONE_ID/dns_records" --data "$(jq -n --arg n "$H" --arg ip "$WP6" \
  '{type: "AAAA", name: $n, content: $ip, proxied: true, ttl: 1, comment: "errex on wp-vps"}')" | jq '.success, .errors'
```

### 3.6 Optional: Authenticated Origin Pulls

Makes Cloudflare present a client certificate that nginx checks, on top of
the IP lock. Download Cloudflare's CA to
`/etc/ssl/cloudflare/authenticated_origin_pull_ca.pem` from
<https://developers.cloudflare.com/ssl/origin-configuration/authenticated-origin-pull/set-up/zone-level/>,
uncomment the two `ssl_client_certificate`/`ssl_verify_client` lines in
the site, `nginx -t`, reload, then turn it on: *SSL/TLS → Origin Server →
Authenticated Origin Pulls* (API: `cf -X PATCH
"$API/zones/$ZONE_ID/settings/tls_client_auth" --data '{"value":"on"}'`).
This setting is zone-wide; origins that do not ask for the certificate are
unaffected.

## 4. Check it

```bash
curl -sI "https://$H/" | head -3                      # 302 to the Access login
curl -s -o /dev/null -w '%{http_code}\n' -X POST "https://$H/api/suite-prod/envelope/"   # 401: no DSN key
curl -s -o /dev/null -w '%{http_code}\n' -6 --max-time 5 -k "https://[$WP6]/"            # 000: ufw drops it
```

A page end to end, with the `suite-beta` key from `errexd project list`:

```bash
KEY=<suite-beta key>; NOW=$(date -u +%FT%TZ)
printf '{}\n{"type":"event"}\n{"timestamp":"%s","level":"fatal","message":"errex page test","tags":{"flash.erased":"true"}}\n' "$NOW" \
  | curl -s -w '%{http_code}\n' -X POST --data-binary @- "https://$H/api/suite-beta/envelope/?sentry_key=$KEY"
```

Expect `200`, a *PAGE* message in Telegram, and the issue in the dashboard.
If Cloudflare's *Bot Fight Mode* is on for the zone it may challenge these
POSTs; the app's requests must then be exempted (Bot Fight Mode cannot be
skipped per path on the free plan).

On wp-vps: `journalctl -u errexd -n 50`, `systemctl status
errex-backup.timer`, `sudo systemctl start errex-backup.service && ls -l
/var/backups/errex`.

## Operations

**Update:** build and copy a new `errex-release`, `sha256sum -c`, `sudo
systemctl start errex-backup.service`, `sudo install -m 0755 errexd
/usr/local/bin/errexd`, `sudo systemctl restart errexd`. Migrations run at
start.

**Restore:** `sudo systemctl stop errexd`, `gunzip -c
/var/backups/errex/errex-<stamp>.db.gz | sudo -u errex tee
/var/lib/errex/errex.db >/dev/null`, `sudo rm -f
/var/lib/errex/errex.db-wal /var/lib/errex/errex.db-shm`, `sudo systemctl
start errexd`.

Backups live on the same disk; copy them off the host if they must
survive it.

**Cloudflare ranges change:** rerun `cloudflare-ips.sh geo` into the
snippet and reload nginx, add the new ufw rules, and update
`CLOUDFLARE_RANGES` in `crates/errexd/src/client_ip.rs`.

**Remove:** `sudo systemctl disable --now errexd errex-backup.timer`, `sudo
rm /etc/nginx/sites-enabled/$H.conf && sudo nginx -t && sudo systemctl
reload nginx`, delete the ufw rules (`sudo ufw status numbered`, `sudo ufw
delete <n>`), the DNS record, the configuration rule and the Access apps.
