#!/usr/bin/env python3
"""Banco de pruebas del modo stick-a-ratón (requiere el mando conectado).

Con `stick_to_mouse` activo y `suppress_axis = true`, el stick derecho debe
mover el ratón virtual al caudal configurado y dejar de llegar al gamepad
virtual. Usa un lector concurrente para no perder eventos del búfer del kernel.

Mueve el cursor real durante la prueba.

Requisitos: python-evdev. Uso:
    python tools/stick_to_mouse_test.py
"""
import glob
import os
import select
import subprocess
import sys
import tempfile
import threading
import time

from evdev import InputDevice, InputEvent, ecodes as e, list_devices

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("GAMESIR_BIN", os.path.join(ROOT, "target/debug/gamesir-rs"))
BY_ID = "/dev/input/by-id/usb-*GameSir-Tegenaria*-event-joystick"
SPEED = 900.0

PROFILE = f"""
name = "prueba stick-a-raton"

[right_stick]
deadzone = 0.10
sensitivity = 1.0

[stick_to_mouse]
enabled = true
stick = "right"
speed = {SPEED}
suppress_axis = true
"""


def physical_node():
    matches = [os.path.realpath(p) for p in glob.glob(BY_ID)]
    if not matches:
        sys.exit(f"[!] no se encuentra el mando físico ({BY_ID})")
    return matches[0]


def find(name, timeout=6.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        for path in list_devices():
            try:
                dev = InputDevice(path)
            except OSError:
                continue
            if dev.name == name:
                return dev
            dev.close()
        time.sleep(0.1)
    return None


def main():
    profile_path = os.path.join(tempfile.gettempdir(), "gamesir_mouse_profile.toml")
    with open(profile_path, "w") as f:
        f.write(PROFILE)

    proc = subprocess.Popen(
        [BIN, "--bridge", "--profile", profile_path, "-t", "9"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        mouse = find("Tegenaria Remapped (mouse)")
        pad = find("Tegenaria Remapped (pad)")
        if not mouse or not pad:
            print("[FALLO] no aparecieron los dispositivos virtuales")
            return 1
        print(f"[ok] ratón virtual {mouse.path}, pad {pad.path}")

        mouse_events, pad_events = [], []
        stop = threading.Event()

        def reader(dev, sink):
            while not stop.is_set():
                ready, _, _ = select.select([dev.fd], [], [], 0.05)
                if ready:
                    sink.extend(dev.read())

        threads = [
            threading.Thread(target=reader, args=(mouse, mouse_events), daemon=True),
            threading.Thread(target=reader, args=(pad, pad_events), daemon=True),
        ]
        for t in threads:
            t.start()

        inj = InputDevice(physical_node())

        def send(t_, c, v):
            inj.write_event(InputEvent(0, 0, t_, c, v))
            inj.write_event(InputEvent(0, 0, e.EV_SYN, e.SYN_REPORT, 0))

        send(e.EV_ABS, e.ABS_Z, 255)  # stick derecho a tope a la derecha
        t0 = time.monotonic()
        time.sleep(1.0)
        send(e.EV_ABS, e.ABS_Z, 128)  # al centro
        dt = time.monotonic() - t0
        time.sleep(0.5)
        send(e.EV_ABS, e.ABS_Z, 0)    # a tope a la izquierda
        time.sleep(0.3)
        send(e.EV_ABS, e.ABS_Z, 128)
        time.sleep(0.3)

        stop.set()
        for t in threads:
            t.join(timeout=1)

        rel_events = [ev for ev in mouse_events if ev.type == e.EV_REL]
        rel_x = [ev.value for ev in rel_events if ev.code == e.REL_X]
        pos_x = sum(v for v in rel_x if v > 0)
        neg_x = sum(v for v in rel_x if v < 0)
        total_y = sum(ev.value for ev in rel_events if ev.code == e.REL_Y)
        rx_on_pad = [ev for ev in pad_events if ev.type == e.EV_ABS and ev.code == e.ABS_RX]

        esperado = SPEED * dt
        print(f"[..] eventos REL: {len(rel_events)}")
        print(f"[..] REL_X a la derecha: {pos_x} (esperado ~{esperado:.0f} en {dt:.2f} s)")
        print(f"[..] REL_X a la izquierda: {neg_x}   REL_Y total: {total_y}")
        print(f"[..] ABS_RX en el pad virtual: {len(rx_on_pad)} (esperado 0)")

        fallos = []
        if not (0.8 * esperado <= pos_x <= 1.2 * esperado):
            fallos.append(f"caudal fuera de tolerancia: {pos_x} vs ~{esperado:.0f}")
        if neg_x >= -0.2 * SPEED * 0.3:
            fallos.append(f"el stick a la izquierda no movió el cursor: {neg_x}")
        if abs(total_y) > 5:
            fallos.append(f"movimiento vertical inesperado: {total_y}")
        if rx_on_pad:
            fallos.append("suppress_axis no impidió que el eje llegara al pad")

        for fallo in fallos:
            print(f"[FALLO] {fallo}")
        if not fallos:
            print("[ok] stick-a-ratón correcto: caudal, dirección y supresión del eje")
        inj.close()
        return 1 if fallos else 0
    finally:
        try:
            out, _ = proc.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            out, _ = proc.communicate()
        print("=== puente ===")
        print(out.strip().splitlines()[-1] if out.strip() else "(sin salida)")


if __name__ == "__main__":
    sys.exit(main())
