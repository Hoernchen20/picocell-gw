#!/bin/sh
# Second CA for the CA rotation test (sketch.md 5.3, 8.1 8e).
#
#   rotate-ca.sh issue    creates out/next/: a new P-256 root CA and a broker
#                         certificate signed by it (once; kept for later runs)
#   rotate-ca.sh switch   swaps the CA and broker certificate in out/ with those in
#                         out/next/ and reloads the broker; again to switch back
#
# Roll the new CA out first (tools/ca-push.py <id> out/next/ca.pem), then switch.
# Mosquitto reloads the certificate on SIGHUP but keeps existing connections, so a
# gateway only sees the new one on its next connection. out/ca.pem is always the CA
# the broker uses, as the other tools expect.
set -eu

DIR=$(cd "$(dirname "$0")" && pwd)/out
NEXT=$DIR/next
FILES="ca.key ca.pem ca.der broker.key broker.pem"

case ${1:-} in
issue)
    if [ -e "$NEXT" ]; then
        echo "$NEXT exists" >&2
        exit 0
    fi
    HOST=$(openssl x509 -in "$DIR/broker.pem" -noout -subject -nameopt multiline | sed -n 's/^ *commonName *= //p')
    mkdir "$NEXT"
    cd "$NEXT"
    openssl ecparam -name prime256v1 -genkey -noout -out ca.key
    openssl req -x509 -new -key ca.key -sha256 -days 3650 -subj "/CN=picocell test broker CA $(date +%s)" \
        -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
        -out ca.pem
    openssl x509 -in ca.pem -outform der -out ca.der
    openssl ecparam -name prime256v1 -genkey -noout -out broker.key
    openssl req -new -key broker.key -subj "/CN=$HOST" -out broker.csr
    openssl x509 -req -in broker.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 -sha256 \
        -extfile "$DIR/leaf.ext" -out broker.pem 2>/dev/null
    rm broker.csr
    echo "new CA in $NEXT (broker host name $HOST)"
    ;;
switch)
    [ -d "$NEXT" ] || { echo "no $NEXT: run rotate-ca.sh issue" >&2; exit 1; }
    for f in $FILES; do
        mv "$DIR/$f" "$NEXT/$f.swap"
        mv "$NEXT/$f" "$DIR/$f"
        mv "$NEXT/$f.swap" "$NEXT/$f"
    done
    if pkill -HUP -x mosquitto; then
        echo "broker switched to $(openssl x509 -in "$DIR/ca.pem" -noout -subject)"
    else
        echo "switched (broker not running)"
    fi
    ;;
*)
    echo "usage: rotate-ca.sh issue|switch" >&2
    exit 1
    ;;
esac
