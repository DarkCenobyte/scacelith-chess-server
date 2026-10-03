#!/bin/sh
# certbot deploy hook of the Scacelith dedicated server (docs/DEPLOY.md, "TLS certificate").
#
# Install as /etc/letsencrypt/renewal-hooks/deploy/scacelith-server (mode 0755) and set
# CERT_NAME below. certbot runs it as root after every renewal, with RENEWED_LINEAGE set to the
# certificate's directory (/etc/letsencrypt/live/<name>); run it once by hand after the first
# issuance:
#   sudo RENEWED_LINEAGE=/etc/letsencrypt/live/play.example.org \
#        /etc/letsencrypt/renewal-hooks/deploy/scacelith-server
#
# It copies the certificate chain and the key where the service reads them (root-owned, group
# scacelith: the key is mode 0640, never readable by other accounts), then reloads the service:
# with Type=notify-reload, "systemctl reload" sends SIGHUP and returns once the server has
# reloaded. A certificate the server cannot load is refused (logged at error level) and the
# current one stays in use.
set -eu

# The certificate name: the directory under /etc/letsencrypt/live/.
CERT_NAME=play.example.org
DEST=/etc/scacelith/tls
GROUP=scacelith
UNIT=scacelith-server.service

# certbot runs every deploy hook for every renewed certificate: ignore the others.
[ "${RENEWED_LINEAGE:-}" = "/etc/letsencrypt/live/$CERT_NAME" ] || exit 0

# Write each file under a temporary name in the same directory, then rename it into place, so
# that the server never reads a partly written file.
install -m 0644 -o root -g "$GROUP" "$RENEWED_LINEAGE/fullchain.pem" "$DEST/.fullchain.pem.new"
install -m 0640 -o root -g "$GROUP" "$RENEWED_LINEAGE/privkey.pem" "$DEST/.privkey.pem.new"
mv -f "$DEST/.fullchain.pem.new" "$DEST/fullchain.pem"
mv -f "$DEST/.privkey.pem.new" "$DEST/privkey.pem"

if systemctl is-active --quiet "$UNIT"; then
    systemctl reload "$UNIT"
fi
