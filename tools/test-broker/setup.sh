#!/bin/sh
# Creates a local Mosquitto test broker in tools/test-broker/out/: a P-256 root CA that
# signs the broker certificate directly (sketch.md 4.1), TLS 1.3 only, password file
# and the per-gateway ACL. For development only: the CA key stays in out/.
#
#   setup.sh [broker-hostname]      default: mqtt.test (= PICOCELL_MQTT_HOST)
#   add-gateway.sh <gateway-id>     add a board (ID from its USB console)
#   mosquitto -c out/mosquitto.conf
set -eu

HOST=${1:-mqtt.test}
DIR=$(cd "$(dirname "$0")" && pwd)/out
PASSWORD=${PICOCELL_MQTT_PASSWORD:-secret-pw}

if [ -e "$DIR" ]; then
    echo "$DIR exists; remove it to start over" >&2
    exit 1
fi
mkdir -p "$DIR"
cd "$DIR"

openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 3650 -subj "/CN=picocell test broker CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out ca.pem
openssl x509 -in ca.pem -outform der -out ca.der

openssl ecparam -name prime256v1 -genkey -noout -out broker.key
openssl req -new -key broker.key -subj "/CN=$HOST" -out broker.csr
printf 'basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:%s\n' \
    "$HOST" > leaf.ext
openssl x509 -req -in broker.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 825 -sha256 \
    -extfile leaf.ext -out broker.pem

# Second CA, only for the negative test in tools/mqtt-host-test.
openssl ecparam -name prime256v1 -genkey -noout -out other-ca.key
openssl req -x509 -new -key other-ca.key -sha256 -days 3650 -subj "/CN=other CA" -out other-ca.pem
openssl x509 -in other-ca.pem -outform der -out other-ca.der

touch passwd
chmod 600 passwd
mosquitto_passwd -b passwd admin admin-pw
# Gateway ID used by tools/mqtt-host-test.
mosquitto_passwd -b passwd 0016c001ff10a235 secret-pw
echo "$PASSWORD" > gateway-password

cat > acl <<EOF
user admin
topic readwrite #

pattern write eu868/gateway/%u/event/#
pattern write eu868/gateway/%u/state/#
pattern read  eu868/gateway/%u/command/#
EOF

cat > mosquitto.conf <<EOF
persistence false
log_dest stdout
log_type error
log_type warning
log_type notice
log_type information
connection_messages true
allow_anonymous false
password_file $DIR/passwd
acl_file $DIR/acl

listener 8883
cafile $DIR/ca.pem
certfile $DIR/broker.pem
keyfile $DIR/broker.key
tls_version tlsv1.3
EOF

echo "broker set up in $DIR (host name $HOST)"
echo "start it with: mosquitto -c $DIR/mosquitto.conf"
