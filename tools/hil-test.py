#!/usr/bin/env python3
"""Hardware-in-the-loop test of a picocell-gw dev board over its USB console only.

    tools/hil-test.py [-d DEVICE] [--flash] [--no-provision] [--broker-ip IP] [--ota]
                      [--ota-fallback] [--erase]

Needs the board on USB and Ethernet, and the test broker running on this machine
(tools/test-broker/setup.sh, then mosquitto -c tools/test-broker/out/mosquitto.conf).

1. --flash: builds fw/ (release), sends "bootsel", waits for the RP2350 drive (desktop
   automount) and copies the UF2 to it.
2. Console: banner, "help", errors for an unknown command and key, "status" fields
   (the boot line, i.e. slot and boot type, is logged), the config key's OTP page read
   at boot and soft-locked since, "otp-key init" refused in a dev build.
3. Provisions tools/test-broker/gateway.conf with net.hosts pointing at this machine
   (--broker-ip, default: the address of the default route) and registers the gateway
   with the broker. With --no-provision the stored config is kept and the board is
   rebooted instead, so both ways the duty cycle starts from zero.
4. Waits for "mqtt connected" in "status", then checks ConnState, GatewayStats and an
   UplinkFrame on the broker.
5. Downlinks via tools/test-downlink.sh and tools/test-classc.sh against the statuses
   from fw/README.md, and "TX (no radio)" in the console log.
6. --ota (needs the partition table): tools/ota-push.py with a wrong SHA-256 and
   truncated (both refused, same slot), then the real update into the other slot:
   flash update boot, bought after the broker connection, kept after a reboot.
7. --ota-fallback (about 7 minutes): updates whose trial cannot reach the broker (the
   gateway's password on the test broker is changed meanwhile and restored after):
   a reboot during the trial, and the 5-minute buy deadline. Both boot the old slot.
8. --erase: deletes the config at the end (provisioning mode).

Exits with the number of failed checks. Without a debug probe the board has to run a
firmware with a working console: if it hangs, hold BOOTSEL while plugging in USB and
flash by hand.
"""

import argparse
import glob
import getpass
import os
import re
import shutil
import socket
import subprocess
import sys
import time

TOOLS = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(TOOLS)
sys.path.insert(0, TOOLS)
# No __pycache__ next to the tools.
sys.dont_write_bytecode = True
import provision  # noqa: E402

ELF = os.path.join(ROOT, "fw/target/thumbv8m.main-none-eabihf/release/picocell-gw")
BROKER = os.path.join(TOOLS, "test-broker")
PASSWD = os.path.join(BROKER, "out/passwd")
CA = os.path.join(BROKER, "out/ca.pem")


class Failed(Exception):
    pass


def fail(msg):
    raise Failed(msg)


# Console errors (no answer) fail the current check instead of exiting.
provision.die = fail


def log(msg):
    print(f"  {msg}", flush=True)


# --- console ---------------------------------------------------------------------


def find_device():
    found = glob.glob(provision.DEVICES)
    if len(found) != 1:
        fail(f"{len(found)} consoles found, pass -d")
    return found[0]


def holders(device):
    """Other processes (of this user) that have the console open: they would read
    part of the output."""
    real = os.path.realpath(device)
    found = []
    for fd in glob.glob("/proc/[0-9]*/fd/*"):
        pid = fd.split("/")[2]
        try:
            if pid != str(os.getpid()) and os.readlink(fd) == real:
                with open(f"/proc/{pid}/comm") as f:
                    found.append(f"{f.read().strip()} (pid {pid})")
        except OSError:
            pass
    return sorted(set(found))


def wait_gone(device, timeout=5):
    end = time.monotonic() + timeout
    while os.path.exists(device) and time.monotonic() < end:
        time.sleep(0.1)


def open_console(device, timeout=30):
    """Waits for the console to (re)appear, opens it and reads up to the prompt."""
    end = time.monotonic() + timeout
    while True:
        try:
            c = provision.Console(device)
            break
        except OSError:
            # Not there yet, or udev has not set the permissions yet.
            if time.monotonic() > end:
                fail(f"console {device} did not appear within {timeout} s")
            time.sleep(0.2)
    c.read_until(b'"help" for commands', 5)
    c.read_until(provision.PROMPT, 2)
    return c


def close(c):
    if c is not None:
        os.close(c.fd)


def run(c, line, more=()):
    out, err = c.run(line, more)
    if err is not None:
        fail(f"{line}: error: {err}")
    # The first command after opening is echoed without the prompt.
    if out and out[0] == line:
        out = out[1:]
    return out


def run_error(c, line):
    out, err = c.run(line)
    if err is None:
        fail(f"{line}: ok, error expected")
    return err


def status(c):
    """`status` as {field: value}; the duty-cycle lines are left out."""
    fields = {}
    for line in run(c, "status"):
        key, value = line[:10].strip(), line[11:]
        if key and key != "duty cycle":
            fields[key] = value
    return fields


def restart(c, device, line):
    """Sends a restarting command ("reboot", "commit") and reopens the console."""
    run(c, line)
    close(c)
    wait_gone(device)
    return open_console(device)


# --- broker ----------------------------------------------------------------------


def mqtt_sub(topic, count=1, wait=10, fmt=None):
    """Payload(s) of `count` messages on `topic`, or fails after `wait` seconds."""
    cmd = ["mosquitto_sub", "-h", "127.0.0.1", "-p", "8883", "--cafile", CA, "--insecure",
           "-u", "admin", "-P", "admin-pw", "-V", "mqttv5", "-t", topic, "-C", str(count),
           "-W", str(wait)]
    cmd += ["-F", fmt] if fmt else ["-N"]
    r = subprocess.run(cmd, capture_output=True)
    if r.returncode != 0 or not r.stdout:
        fail(f"nothing on {topic} within {wait} s")
    return r.stdout


def pb_decode(kind, data):
    r = subprocess.run([os.path.join(TOOLS, "pb-decode.sh"), kind], input=data, capture_output=True)
    if r.returncode != 0:
        fail(f"{kind} does not decode: {r.stderr.decode().strip()}")
    return r.stdout.decode()


def script(name, *args, timeout=60):
    r = subprocess.run([os.path.join(TOOLS, name), *map(str, args)], capture_output=True,
                       text=True, timeout=timeout)
    if r.returncode != 0:
        fail(f"{name} {' '.join(map(str, args))}: {(r.stderr or r.stdout).strip()}")
    return r.stdout


def ack_statuses(text):
    """Item statuses of a decoded DownlinkTxAck; IGNORED decodes as an empty item."""
    items = re.findall(r"items \{([^}]*)\}", text)
    return [(re.search(r"status: (\w+)", i) or [None, "IGNORED"])[1] for i in items]


def expect(what, got, want):
    log(f"{what}: {' '.join(got)}")
    if got != want:
        fail(f"{what}: {' '.join(want)} expected")


def default_ip():
    """This machine's address on the default route (no packet is sent)."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.connect(("192.0.2.1", 9))
        return s.getsockname()[0]


def broker_running():
    try:
        socket.create_connection(("127.0.0.1", 8883), timeout=2).close()
        return True
    except OSError:
        return False


# --- test ------------------------------------------------------------------------


def main():
    p = argparse.ArgumentParser(description="Hardware-in-the-loop test of a picocell-gw over USB.")
    p.add_argument("-d", "--device", help="console device (default: the one under /dev/serial/by-id/)")
    p.add_argument("--flash", action="store_true", help="build and flash the firmware first")
    p.add_argument("--no-provision", action="store_true", help="keep the stored config")
    p.add_argument("--broker-ip", help="address of this machine as seen by the board")
    p.add_argument("--ota", action="store_true", help="firmware update over MQTT")
    p.add_argument("--ota-fallback", action="store_true", help="trial images that never reach the broker")
    p.add_argument("--erase", action="store_true", help="delete the config at the end")
    a = p.parse_args()

    results = []
    # "queued": downlinks acked with OK, to be found as "TX (no radio)" in the log.
    state = {"c": None, "device": a.device, "id": None, "queued": 0}

    def check(name, fn, critical=False):
        print(f"{name} ...", flush=True)
        t = time.monotonic()
        try:
            fn()
            ok = True
        except (Failed, subprocess.TimeoutExpired) as e:
            ok = False
            print(f"  FAIL: {e}", flush=True)
        results.append((name, ok, time.monotonic() - t))
        if not ok and critical:
            summary(results)
            sys.exit(sum(not ok for _, ok, _ in results))

    def prerequisites():
        for tool in ("mosquitto_sub", "mosquitto_pub", "mosquitto_passwd"):
            if shutil.which(tool) is None:
                fail(f"{tool} not found")
        if not os.path.exists(CA):
            fail("no test broker: run tools/test-broker/setup.sh")
        if not broker_running():
            fail("broker not listening on 8883: mosquitto -c tools/test-broker/out/mosquitto.conf")
        if state["device"] is None:
            state["device"] = find_device()
        log(state["device"])
        others = holders(state["device"])
        if others:
            fail(f"console also open in {', '.join(others)}; close it first")

    def flash():
        r = subprocess.run(["cargo", "build", "--release"], cwd=os.path.join(ROOT, "fw"))
        if r.returncode != 0:
            fail("build failed")
        c = open_console(state["device"])
        run(c, "bootsel")
        close(c)
        user = getpass.getuser()
        drives = [f"/media/{user}/RP2350", f"/run/media/{user}/RP2350"]
        end = time.monotonic() + 20
        while not any(os.path.isdir(d) for d in drives):
            if time.monotonic() > end:
                fail("RP2350 drive not mounted within 20 s (automount off?)")
            time.sleep(0.2)
        r = subprocess.run([sys.executable, os.path.join(TOOLS, "elf2uf2-rp2350.py"), ELF])
        if r.returncode != 0:
            fail("UF2 conversion or copy failed")

    def console():
        c = state["c"] = open_console(state["device"])
        run(c, "log off")
        if not run(c, "log show 5"):
            fail("log history empty")
        if "bootsel" not in "\n".join(run(c, "help")):
            fail("help does not list bootsel")
        err = run_error(c, "frobnicate")
        if "unknown command" not in err:
            fail(f"frobnicate: {err}")
        run_error(c, "set no.such.key 1")
        run_error(c, "ca 2")
        s = status(c)
        for key in ("firmware", "boot", "security", "config key", "gateway id", "config", "mac", "ip", "time",
                    "uptime", "mqtt"):
            if key not in s:
                fail(f"status has no {key} line")
        if not re.fullmatch(r"[0-9a-f]{16}", s["gateway id"]):
            fail(f"gateway id {s['gateway id']!r}")
        state["id"] = s["gateway id"]
        key = s.get("config key", "")
        if "dev build" in s["security"] and not re.fullmatch(r"chip ID \(dev build\); OTP page \d+ empty, soft-locked", key):
            fail(f"config key {key!r}")
        err = run_error(c, "otp-key init")
        if "dev build" in s["security"] and "dev build" not in err:
            fail(f"otp-key init: {err}")
        log(f"firmware {s['firmware']}, gateway {s['gateway id']}, config {s['config']}")
        log(f"boot {s['boot']}")

    def provisioning():
        close(state["c"])
        state["c"] = None
        ip = a.broker_ip or default_ip()
        log(f"broker at {ip}")
        script(os.path.join("test-broker", "add-gateway.sh"), state["id"])
        r = subprocess.run([sys.executable, os.path.join(TOOLS, "provision.py"), "-d", state["device"],
                            os.path.join(BROKER, "gateway.conf"), f"net.hosts=mqtt.test={ip}"],
                           capture_output=True, text=True)
        if r.returncode != 0:
            fail((r.stderr or r.stdout).strip())
        wait_gone(state["device"])
        state["c"] = open_console(state["device"])
        run(state["c"], "log off")

    def reboot():
        state["c"] = restart(state["c"], state["device"], "reboot")
        run(state["c"], "log off")

    def connected():
        end = time.monotonic() + 90
        last = None
        while True:
            s = status(state["c"])
            if s["mqtt"] != last:
                last = s["mqtt"]
                log(f"mqtt {last}, ip {s['ip']}, time {s['time']}")
            if s["mqtt"].startswith("connected"):
                break
            if time.monotonic() > end:
                fail(f"not connected within 90 s; config {s['config']}")
            time.sleep(2)
        if not s["config"].startswith("stored"):
            fail(f"config {s['config']}")
        if s["ip"] == "none" or s["time"] == "time unknown":
            fail(f"ip {s['ip']}, time {s['time']}")

    topic = lambda t: f"eu868/gateway/{state['id']}/{t}"  # noqa: E731

    def conn_state():
        text = pb_decode("ConnState", mqtt_sub(topic("state/conn"), wait=5))
        if "ONLINE" not in text:
            fail(f"ConnState not ONLINE: {text.strip()}")

    def stats():
        text = pb_decode("GatewayStats", mqtt_sub(topic("event/stats"), wait=35))
        if state["id"] not in text:
            fail("GatewayStats without the gateway id")
        if 'key: "secure_boot"' not in text or 'key: "config_key"' not in text:
            fail("GatewayStats without the security metadata")

    def uplink():
        text = pb_decode("UplinkFrame", mqtt_sub(topic("event/up"), wait=15))
        if "context" not in text:
            fail("UplinkFrame without context")

    def class_a(delay, want):
        def fn():
            out = script("test-downlink.sh", state["id"], delay, timeout=90)
            got = ack_statuses(out)
            state["queued"] += got.count("OK")
            expect(f"RX1 +{delay} s, RX2", got, want)
        return fn

    def class_c(args, want):
        def fn():
            out = script("test-classc.sh", state["id"], *args)
            got = re.findall(r"^downlink \d+: (\w+)$", out, re.M)
            state["queued"] += got.count("OK")
            expect("statuses", got, want)
        return fn

    def tx_logged():
        # The last Class C frames go out back to back, seconds after their acks.
        end = time.monotonic() + 15
        while True:
            lines = run(state["c"], "log show 200")
            n = sum("TX (no radio)" in l for l in lines)
            if n >= state["queued"] or time.monotonic() > end:
                break
            time.sleep(1)
        log(f"{n} of {state['queued']} queued downlinks sent")
        if n != state["queued"]:
            fail('"TX (no radio)" missing for some downlinks')
        # Not a failure: a connection attempt before DHCP or SNTP may log one.
        for l in lines:
            if "] error: " in l:
                log(l)

    def reopen():
        """After the gateway restarted on its own (OTA reboot, buy deadline)."""
        close(state["c"])
        state["c"] = None
        wait_gone(state["device"], timeout=10)
        state["c"] = open_console(state["device"])
        run(state["c"], "log off")

    def slot():
        m = re.match(r"slot ([AB]),", status(state["c"])["boot"])
        if not m:
            fail(f"not booted from slot A or B: {status(state['c'])['boot']} (partition table?)")
        return m[1]

    def push(*args, want_rc=0, want=None):
        r = subprocess.run([sys.executable, os.path.join(TOOLS, "ota-push.py"), state["id"], *args],
                           capture_output=True, text=True, timeout=180)
        out = (r.stdout + r.stderr).strip()
        last = out.splitlines()[-1] if out else ""
        log(last)
        if r.returncode != want_rc or (want and want not in out):
            fail(f"ota-push.py {' '.join(args)}: exit {r.returncode}, {want!r} expected: {last}")

    def blocked(on):
        """Changes the gateway's password on the test broker, or restores it."""
        saved = PASSWD + ".hil"
        if on:
            shutil.copy2(PASSWD, saved)
            subprocess.run(["mosquitto_passwd", "-b", PASSWD, state["id"], "hil-test-blocked"], check=True)
        elif os.path.exists(saved):
            os.replace(saved, PASSWD)
        else:
            return
        subprocess.run(["pkill", "-HUP", "-x", "mosquitto"], check=True)
        log("broker: gateway " + ("locked out" if on else "back in"))

    def uptime():
        d, hms = status(state["c"])["uptime"].split("d ")
        h, m, sec = map(int, hms.split(":"))
        return ((int(d) * 24 + h) * 60 + m) * 60 + sec

    def ota_refused(*args, want):
        def fn():
            before, up = slot(), uptime()
            push(*args, want_rc=int("--bad-sha256" in args), want=want)
            if slot() != before or uptime() < up:
                fail("gateway changed slot or restarted")
        return fn

    def ota_update():
        before = slot()
        push(want="verified")
        reopen()
        s = status(state["c"])
        log(f"boot {s['boot']}")
        if slot() == before or "flash update boot" not in s["boot"]:
            fail(f"slot {before} → {slot()}, flash update boot expected")
        if "trial" not in s:
            fail("no trial line (bought before the broker connection?)")
        connected()
        s = status(state["c"])
        if "trial" in s:
            fail(f"still on trial after the broker connection: {s['trial']}")
        if not any("explicit_buy: ok" in l for l in run(state["c"], "log show 100")):
            fail('"explicit_buy: ok" not in the log')
        state["slot"] = slot()

    def ota_kept():
        reboot()
        s = status(state["c"])
        if slot() != state["slot"] or "normal boot" not in s["boot"]:
            fail(f"after reboot: {s['boot']}, slot {state['slot']} expected")
        connected()

    def ota_fallback(deadline):
        def fn():
            before = slot()
            try:
                push(want="verified")
                blocked(True)
                reopen()
                s = status(state["c"])
                if slot() == before or "trial" not in s:
                    fail(f"no trial in the new slot: {s['boot']}")
                if deadline:
                    # The trial reboots itself after 300 s of uptime; USB comes back
                    # within a second or two, so watch the uptime, not the device.
                    end = time.monotonic() + 360
                    last = uptime()
                    while True:
                        time.sleep(5)
                        try:
                            now = uptime()
                        except (Failed, OSError):
                            reopen()
                            break
                        if now < last:
                            break
                        if time.monotonic() > end:
                            fail("no reboot within 360 s")
                        last = now
                else:
                    reboot()
                s = status(state["c"])
                log(f"boot {s['boot']}")
                if slot() != before or "normal boot" not in s["boot"]:
                    fail(f"back in slot {before} with a normal boot expected")
            finally:
                blocked(False)
            connected()
        return fn

    def erase():
        state["c"] = restart(state["c"], state["device"], "erase")
        if not status(state["c"])["config"].startswith("provisioning mode"):
            fail("still provisioned after erase")

    check("prerequisites", prerequisites, critical=True)
    if a.flash:
        check("flash", flash, critical=True)
    check("console", console, critical=True)
    if a.no_provision:
        check("reboot", reboot, critical=True)
    else:
        check("provision", provisioning, critical=True)
    check("mqtt connect", connected, critical=True)
    check("ConnState ONLINE", conn_state)
    check("GatewayStats", stats)
    check("UplinkFrame", uplink)
    check("class A, RX1", class_a(1, ["OK", "IGNORED"]))
    check("class A, RX1 too late", class_a(0, ["TOO_LATE", "OK"]))
    check("class A, too early", class_a(61, ["TOO_EARLY", "TOO_EARLY"]))
    check("class C, frequency between sub-bands", class_c([1, 868650000], ["TX_FREQ"]))
    check("class C, power above sub-band limit", class_c([1, 868850000, 12, 20], ["TX_POWER"]))
    check("class C, duty cycle 0.1 %", class_c([], ["OK", "OK"] + ["DUTY_CYCLE_OVERFLOW"] * 3))
    check("class C, RX2 band 10 %", class_c([3, 869525000, 12, 27], ["OK"] * 3))
    check("console log", tx_logged)
    if a.ota or a.ota_fallback:
        check("OTA, wrong SHA-256", ota_refused("--bad-sha256", want="SHA-256 mismatch"))
        check("OTA, truncated", ota_refused("--truncate", "100000", want="stopped after"))
    if a.ota:
        check("OTA update", ota_update)
        check("OTA, bought image kept", ota_kept)
    if a.ota_fallback:
        check("OTA trial, reboot before the buy", ota_fallback(False))
        check("OTA trial, buy deadline", ota_fallback(True))
    if a.erase:
        check("erase", erase)
    if state["c"] is not None:
        try:
            run(state["c"], "log info")
        except (Failed, OSError):
            pass
    summary(results)
    sys.exit(sum(not ok for _, ok, _ in results))


def summary(results):
    print()
    for name, ok, secs in results:
        print(f"{'PASS' if ok else 'FAIL'}  {secs:5.1f} s  {name}")
    failed = sum(not ok for _, ok, _ in results)
    print(f"\n{len(results) - failed} passed, {failed} failed")


if __name__ == "__main__":
    main()
