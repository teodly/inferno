#!/usr/bin/env python3
"""Regression test for teodly/inferno#41: no stale audio after a disconnect.

Two plugin instances on one host: one transmits a counting ramp (aplay),
the other captures it (arecord). The receiver is subscribed to the
transmitter with a raw ARC request, then, mid-capture, unsubscribed with a
raw 0x3014. After the disconnect the capture must hold the real end of the
ramp (at most one latency) and then only silence. Before the fix it held
short bursts of audio from one ring-buffer cycle earlier, replayed as the
reader overtook the silence.

Needs: the release plugin (cargo build --release -p alsa_pcm_inferno),
aplay/arecord, a usrvclock server on /tmp/ptp-usrvclock (e.g. inferno's
fake_usrvclock_server), and the netaudio Python package (its encoder builds
the subscribe request). Run as a user that may bind the instances' ports.

Usage: test_disconnect_tail.py [--lib PATH] [--ip HOST_IP] [--runs N]
Exit 0 when every run is clean, 1 on stale audio, 2 on setup failure.
"""
import argparse, array, os, shutil, socket, struct, subprocess, sys, tempfile, time

RATE, CH, SECS, UNSUB_AT, LAT_MS = 48000, 2, 24, 10.0, 10
TOL = 4 * 256  # 24-bit TX dither moves a sample by +-1 LSB (256 in s32)


def host_ip():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))
        return s.getsockname()[0]
    finally:
        s.close()


def subscribe(rx_ip, rx_port, tx_device):
    sys.path.insert(0, "/usr/local/lib/python3.13/dist-packages")
    from netaudio.dante.device_commands import DanteDeviceCommands
    pkt = DanteDeviceCommands().command_add_subscriptions([(c, f"TX {c}", tx_device) for c in range(1, CH + 1)])
    pkt = pkt[0] if isinstance(pkt, tuple) else pkt
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(3)
    s.sendto(pkt, (rx_ip, rx_port))
    return s.recvfrom(65536)[0][8:10].hex() in ("0001", "8112")


def unsubscribe(rx_ip, rx_port):
    content = struct.pack(">H", CH) + b"".join(struct.pack(">I", c) for c in range(1, CH + 1))
    pkt = struct.pack(">HHHHH", 0x27FF, 10 + len(content), 0x4A1C, 0x3014, 0) + content
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(3)
    s.sendto(pkt, (rx_ip, rx_port))
    s.recvfrom(256)


def segments(v):
    def kind(i):
        if abs(v[i]) <= TOL:
            return "zero"
        if i and abs(v[i - 1]) > TOL and abs((v[i] - v[i - 1]) - 256) <= 2 * TOL:
            return "ramp"
        return "jump"
    out, cur, start = [], None, 0
    for i in range(len(v)):
        k = kind(i)
        if k != cur:
            if cur is not None:
                out.append([cur, start, i - start])
            cur, start = k, i
    out.append([cur, start, len(v) - start])
    folded = []  # a run's first sample reads as a jump; fold it in
    for s in out:
        if folded and s[0] == "ramp" and folded[-1][0] == "jump" and folded[-1][2] <= 2:
            folded[-1] = ["ramp", folded[-1][1], folded[-1][2] + s[2]]
        elif folded and folded[-1][0] == s[0]:
            folded[-1][2] += s[2]
        else:
            folded.append(s)
    return folded


def run_once(lib, ip, work):
    env = dict(os.environ, RUST_LOG="warn")
    with open(f"{work}/asoundrc", "w") as f:
        f.write(f'pcm_type.inferno {{ lib "{lib}" }}\npcm.inferno {{ type inferno }}\n')
    env["ALSA_CONFIG_PATH"] = f"/usr/share/alsa/alsa.conf:{work}/asoundrc"

    def inst(name, pid, port, tx, rx):
        return dict(env, INFERNO_NAME=name, INFERNO_PROCESS_ID=str(pid), INFERNO_ALT_PORT=str(port),
                    INFERNO_TX_CHANNELS=str(tx), INFERNO_RX_CHANNELS=str(rx), INFERNO_SAMPLE_RATE=str(RATE),
                    INFERNO_TX_LATENCY_NS=str(LAT_MS * 10**6), INFERNO_RX_LATENCY_NS=str(LAT_MS * 10**6),
                    XDG_STATE_HOME=f"{work}/state")

    ramp = array.array("i", (((i % (1 << 22)) + 1) << 8 for i in range(RATE * (SECS + 30)) for _ in range(CH)))
    with open(f"{work}/ramp.raw", "wb") as f:
        f.write(ramp.tobytes())
    tx = subprocess.Popen(["aplay", "-q", "-D", "inferno", "-f", "S32_LE", "-r", str(RATE), "-c", str(CH),
                           f"{work}/ramp.raw"], env=inst("t41tx", 41, 10500, CH, 0),
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(1)
    t0 = time.monotonic()
    rx = subprocess.Popen(["arecord", "-q", "-D", "inferno", "-d", str(SECS), "-c", str(CH), "-r", str(RATE),
                           "-f", "S32_LE", "-t", "raw", f"{work}/cap.raw"], env=inst("t41rx", 42, 10600, 0, CH),
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(4)
        if not subscribe(ip, 10600, "t41tx"):
            return 2, "subscribe refused"
        while time.monotonic() - t0 < UNSUB_AT:
            time.sleep(0.01)
        unsubscribe(ip, 10600)
        rx.wait(timeout=SECS + 20)
    finally:
        tx.terminate()
        rx.kill() if rx.poll() is None else None

    v = array.array("i")
    data = open(f"{work}/cap.raw", "rb").read()
    v.frombytes(data[: len(data) // (4 * CH) * 4 * CH])
    segs = segments(v[::CH])
    ramps = [s for s in segs if s[0] == "ramp"]
    if not ramps or max(s[2] for s in ramps) < RATE:
        return 2, "no sustained audio before the disconnect"
    stream = max(ramps, key=lambda s: s[2])
    end = stream[1] + stream[2]
    stale = [s for s in segs if s[1] >= end and s[0] != "zero"]
    if stale:
        return 1, f"stale audio after the disconnect: {len(stale)} segment(s), {sum(s[2] for s in stale) / RATE:.4f}s"
    return 0, f"clean ({stream[2] / RATE:.2f}s of stream, then silence)"


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    p = argparse.ArgumentParser()
    p.add_argument("--lib", default=os.path.realpath(f"{here}/../target/release/libasound_module_pcm_inferno.so"))
    p.add_argument("--ip", default=None)
    p.add_argument("--runs", type=int, default=3)
    p.add_argument("--keep", help="copy each run's capture here (cap_N.raw)")
    p.add_argument("--workdir", default="/var/tmp", help="where each run's temporary files go")
    a = p.parse_args()
    if not os.path.exists("/tmp/ptp-usrvclock"):
        print("no usrvclock server on /tmp/ptp-usrvclock - start one first")
        return 2
    ip = a.ip or host_ip()
    worst = 0
    for i in range(a.runs):
        # ~30 MB per run: keep it off /tmp, which is RAM on many SBCs
        with tempfile.TemporaryDirectory(dir=a.workdir) as work:
            code, msg = run_once(a.lib, ip, work)
            if a.keep:
                os.makedirs(a.keep, exist_ok=True)
                shutil.move(f"{work}/cap.raw", f"{a.keep}/cap_{i + 1}.raw")
        print(f"run {i + 1}: {msg}")
        worst = max(worst, code)
    print("PASS" if worst == 0 else "FAIL")
    return worst


if __name__ == "__main__":
    sys.exit(main())
