#!/usr/bin/env python3
"""Stand-in broker that refuses a subscription, for the gateway's SUBACK error path.

Mosquitto's acl_file grants every SUBSCRIBE (it filters on delivery instead), so a
refused SUBACK never happens with the test broker. This one speaks just enough MQTT 5
over TLS (the test broker's certificate from setup.sh): it accepts any CONNECT, answers
SUBSCRIBEs whose filter contains the --topic string with 0x87 (Not authorized) for the
first --count connections and grants everything after that, acknowledges QoS 1
PUBLISHes and PINGREQs. Every packet is logged with the time since the start, so the
reconnect backoff is visible.

Stop Mosquitto first (same port):
    refuse-suback.py [--topic command/down] [--count 3] [--port 8883]
"""

import argparse
import os
import socket
import ssl
import threading
import time

DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out")
T0 = time.monotonic()


def log(conn, msg):
    print(f"{time.monotonic() - T0:8.1f}  #{conn}  {msg}", flush=True)


def read_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def read_varint(s):
    value, shift = 0, 0
    while True:
        b = read_exact(s, 1)[0]
        value |= (b & 0x7F) << shift
        if not b & 0x80:
            return value
        shift += 7


def varint_at(data, i):
    value, shift = 0, 0
    while True:
        b = data[i]
        i += 1
        value |= (b & 0x7F) << shift
        if not b & 0x80:
            return value, i
        shift += 7


def string_at(data, i):
    n = int.from_bytes(data[i : i + 2], "big")
    return data[i + 2 : i + 2 + n].decode(errors="replace"), i + 2 + n


def session(conn, s, refuse, topic):
    while True:
        first = read_exact(s, 1)[0]
        body = read_exact(s, read_varint(s))
        kind = first >> 4
        if kind == 1:
            log(conn, "CONNECT")
            s.sendall(bytes([0x20, 3, 0, 0, 0]))
        elif kind == 3:
            qos = (first >> 1) & 3
            name, i = string_at(body, 0)
            log(conn, f"PUBLISH {name} qos {qos}")
            if qos == 1:
                s.sendall(bytes([0x40, 2]) + body[i : i + 2])
        elif kind == 8:
            pid = body[:2]
            props, i = varint_at(body, 2)
            i += props
            codes = b""
            while i < len(body):
                name, i = string_at(body, i)
                qos = body[i] & 3
                i += 1
                code = 0x87 if refuse and topic in name else qos
                log(conn, f"SUBSCRIBE {name} -> 0x{code:02x}")
                codes += bytes([code])
            s.sendall(bytes([0x90, 3 + len(codes)]) + pid + b"\0" + codes)
        elif kind == 12:
            log(conn, "PINGREQ")
            s.sendall(bytes([0xD0, 0]))
        elif kind == 14:
            log(conn, "DISCONNECT")
            return
        else:
            log(conn, f"packet type {kind}, ignored")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--topic", default="command/down", help="refuse filters containing this")
    p.add_argument("--count", type=int, default=3, help="connections to refuse it on")
    p.add_argument("--port", type=int, default=8883)
    a = p.parse_args()

    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    ctx.load_cert_chain(os.path.join(DIR, "broker.pem"), os.path.join(DIR, "broker.key"))

    srv = socket.create_server(("", a.port), reuse_port=False)
    print(f"listening on {a.port}, refusing '{a.topic}' on the first {a.count} connections", flush=True)
    n = 0
    while True:
        raw, peer = srv.accept()
        n += 1
        conn = n

        def serve(raw=raw, peer=peer, conn=conn):
            try:
                with ctx.wrap_socket(raw, server_side=True) as s:
                    log(conn, f"connected from {peer[0]}")
                    session(conn, s, conn <= a.count, a.topic)
            except (EOFError, OSError, ssl.SSLError) as e:
                log(conn, f"closed ({type(e).__name__})")

        threading.Thread(target=serve, daemon=True).start()


if __name__ == "__main__":
    main()
