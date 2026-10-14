#!/bin/sh
# Online backup of the errex database with sqlite3's .backup, which is safe
# while errexd writes. Checks the copy, compresses it, keeps
# ERREX_BACKUP_KEEP_DAYS days. Install to /usr/local/lib/errex/errex-backup.sh.
set -eu

db=${ERREX_DB:-/var/lib/errex/errex.db}
dir=${ERREX_BACKUP_DIR:-/var/backups/errex}
keep=${ERREX_BACKUP_KEEP_DAYS:-14}
stamp=$(date -u +%Y%m%dT%H%M%SZ)
tmp="$dir/.errex-$stamp.db"

trap 'rm -f "$tmp"' EXIT
sqlite3 "$db" ".backup '$tmp'"
check=$(sqlite3 "$tmp" 'PRAGMA integrity_check;')
if [ "$check" != "ok" ]; then
    echo "errex-backup: integrity check failed on the copy" >&2
    exit 1
fi
gzip -9 -c "$tmp" > "$dir/errex-$stamp.db.gz.part"
mv "$dir/errex-$stamp.db.gz.part" "$dir/errex-$stamp.db.gz"
find "$dir" -maxdepth 1 -name 'errex-*.db.gz' -mtime "+$keep" -delete
echo "errex-backup: $dir/errex-$stamp.db.gz"
