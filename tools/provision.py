#!/usr/bin/env python3
"""Provisions a picocell-gw over its USB console (sketch.md section 7).

    tools/provision.py [-d DEVICE] [--no-commit] CONFIG [key=value ...]

CONFIG has one "key = value" per line; lines starting with "#" are comments. A value
"@path" is read from that file, relative to CONFIG: tls.ca0 and tls.ca1 take a PEM
certificate this way, secrets can stay in their own file. A value starting with "@@"
is taken literally, minus one "@" (a password starting with "@"). key=value arguments
override CONFIG (their @paths are relative to the current directory).

console.password is hashed here (PBKDF2-HMAC-SHA256, random salt), so only the hash
goes to the gateway; a value that already is a hash (iterations$salt$hash) is sent as
it is. If the console is locked, the tool logs in with the console password from
$PICOCELL_CONSOLE_PASSWORD, else it asks for it.

The edited config starts empty ("clear"), so keys not given keep their defaults.
Then "show" is printed (secrets as ***) and "commit" stores the config, upon which
the gateway restarts with it. With --no-commit the changes stay uncommitted on the
console, to look at or "discard" there.

DEVICE defaults to the one picocell-gw console under /dev/serial/by-id/.
"""

import argparse
import getpass
import glob
import hashlib
import os
import re
import select
import sys
import time
import tty

PROMPT = b"picocell> "
DEVICES = "/dev/serial/by-id/usb-picocell_picocell-gw_console_*-if00"
CA_KEYS = ("tls.ca0", "tls.ca1")
# As the firmware (fw/src/login.rs).
ITERATIONS = 10000
HASH = re.compile(r"\d{1,6}\$[0-9a-f]{32}\$[0-9a-f]{64}")


def die(msg):
    sys.exit(f"provision: {msg}")


def load(path, overrides):
    """The config as an ordered {key: value}, @files read."""
    items = {}
    # Directory that @paths are relative to; the override's for an overridden key.
    bases = {}
    with open(path) as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, sep, value = line.partition("=")
            if not sep:
                die(f"{path}:{n}: key = value expected")
            items[key.strip()] = value.strip()
            bases[key.strip()] = os.path.dirname(path)
    for o in overrides:
        key, sep, value = o.partition("=")
        if not sep:
            die(f"{o}: key=value expected")
        items[key.strip()] = value.strip()
        bases[key.strip()] = "."
    for key, base in bases.items():
        value = items[key]
        if value.startswith("@@"):
            items[key] = value[1:]
        elif value.startswith("@"):
            name = os.path.join(base, value[1:])
            try:
                with open(name) as f:
                    value = f.read()
            except OSError as e:
                die(f"{key}: {e}")
            items[key] = value if key in CA_KEYS else value.strip()
    for key in CA_KEYS:
        if key in items and "-----BEGIN CERTIFICATE-----" not in items[key]:
            die(f"{key}: a PEM certificate file (@path) expected")
    pw = items.get("console.password")
    if pw is not None and not HASH.fullmatch(pw):
        if not re.fullmatch(r"[!-~]{1,64}", pw):
            die("console.password: 1 to 64 printable characters expected, no blanks")
        items["console.password"] = password_hash(pw)
    return items


def password_hash(password):
    """The stored form of a console password: iterations$salt$hash."""
    salt = os.urandom(16)
    dk = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, ITERATIONS)
    return f"{ITERATIONS}${salt.hex()}${dk.hex()}"


def console_password():
    return os.environ.get("PICOCELL_CONSOLE_PASSWORD") or getpass.getpass("console password: ")


class Console:
    def __init__(self, path):
        self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY)
        tty.setraw(self.fd)
        self.buf = b""

    def read_until(self, marker, timeout):
        end = time.monotonic() + timeout
        while marker not in self.buf:
            left = end - time.monotonic()
            if left <= 0:
                die(f"no answer from the gateway (waiting for {marker!r})")
            if select.select([self.fd], [], [], left)[0]:
                self.buf += os.read(self.fd, 4096)
        i = self.buf.index(marker) + len(marker)
        out, self.buf = self.buf[:i], self.buf[i:]
        return out

    def run(self, line, more=(), timeout=10):
        """Sends a command and further input lines; returns its output lines and the
        error message, or None after "ok"."""
        for l in (line, *more):
            os.write(self.fd, l.encode() + b"\r")
        lines = []
        while True:
            text = self.read_until(b"\r\n", timeout)[:-2].decode(errors="replace")
            if text == "ok":
                return lines, None
            if text.startswith("error: "):
                return lines, text[len("error: "):]
            # Skip the echo of the command.
            if not text.startswith(PROMPT.decode()):
                lines.append(text)

    def greet(self):
        """Reads what opening the port brings, up to the prompt; returns whether the
        console is locked."""
        self.read_until(b'"help" for commands', 5)
        return b"console locked" in self.read_until(PROMPT, 2)

    def login(self, password):
        """Returns the error message, or None. A wrong password delays the answer
        (up to 60 s)."""
        return self.run("login", [password], timeout=90)[1]


def main():
    p = argparse.ArgumentParser(description="Provision a picocell-gw over its USB console.")
    p.add_argument("-d", "--device", help="console device (default: the one under /dev/serial/by-id/)")
    p.add_argument("--no-commit", action="store_true", help="leave the changes uncommitted")
    p.add_argument("config")
    p.add_argument("overrides", nargs="*", metavar="key=value")
    a = p.parse_args()

    items = load(a.config, a.overrides)
    device = a.device
    if device is None:
        found = glob.glob(DEVICES)
        if len(found) != 1:
            die(f"{len(found)} consoles found, pass -d")
        device = found[0]

    c = Console(device)
    # Opening the port replays the log history, then the banner and the prompt.
    if c.greet():
        err = c.login(console_password())
        if err is not None:
            die(f"login: {err}")

    def run(line, more=(), shown=None):
        out, err = c.run(line, more)
        if err is not None:
            # Leave the gateway as it was.
            c.run("discard")
            c.run("log info")
            die(f"{shown or line}: {err}")
        return out

    run("log off")
    run("clear")
    for key, value in items.items():
        if key in CA_KEYS:
            run(f"ca {key[-1]}", value.splitlines(), shown=key)
        else:
            run(f"set {key} {value}", shown=f"set {key}")
    print("\n".join(run("show")))
    if a.no_commit:
        run("log info")
        print('not committed; "commit" or "discard" on the console')
        return
    run("commit")
    print("committed, the gateway restarts with this config")


if __name__ == "__main__":
    main()
