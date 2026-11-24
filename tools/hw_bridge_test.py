#!/usr/bin/env python3
"""Banco de pruebas de extremo a extremo del puente (requiere el mando conectado).

Arranca el puente Rust con grab() del mando físico, inyecta eventos en el nodo
físico y comprueba lo que sale por los dispositivos virtuales. Al estar el
físico capturado en exclusivo, los eventos inyectados solo los ve el puente: no
llegan al escritorio ni a ningún juego.

Requisitos: python-evdev. Uso:
    python tools/hw_bridge_test.py
    GAMESIR_BIN=target/release/gamesir-rs python tools/hw_bridge_test.py
"""
import glob
import os
import select
import subprocess
import sys
import time

from evdev import InputDevice, InputEvent, ecodes as e, list_devices

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("GAMESIR_BIN", os.path.join(ROOT, "target/debug/gamesir-rs"))
BY_ID = "/dev/input/by-id/usb-*GameSir-Tegenaria*-event-joystick"
PAD_NAME = "Tegenaria Remapped (pad)"
KBD_NAME = "Tegenaria Remapped (kbd)"
MOUSE_NAME = "Tegenaria Remapped (mouse)"


def physical_node():
    matches = [os.path.realpath(p) for p in glob.glob(BY_ID)]
    if not matches:
        sys.exit(f"[!] no se encuentra el mando físico ({BY_ID})")
    return matches[0]


def find(name, timeout=6.0):
    """Busca un dispositivo evdev por nombre (los virtuales no tienen by-id)."""
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


def drain(dev, window=0.6):
    got = []
    end = time.monotonic() + window
    while time.monotonic() < end:
        ready, _, _ = select.select([dev.fd], [], [], 0.05)
        if not ready:
            continue
        for ev in dev.read():
            if ev.type != e.EV_SYN:
                got.append((ev.type, ev.code, ev.value))
    return got


def name_of(t, c):
    n = e.bytype.get(t, {}).get(c, f"0x{c:x}")
    return n[0] if isinstance(n, (list, tuple)) else str(n)


def main():
    phys_path = physical_node()
    print(f"[..] mando físico: {phys_path}")
    proc = subprocess.Popen(
        [BIN, "--bridge", "-t", "14", "--verbose"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        pad = find(PAD_NAME)
        kbd = find(KBD_NAME, timeout=2.0)
        mouse = find(MOUSE_NAME, timeout=2.0)
        if not pad:
            print("[FALLO] no apareció el gamepad virtual")
            return 1
        print(f"[ok] virtual pad   : {pad.path}")
        print(f"[ok] virtual kbd   : {kbd.path if kbd else 'NO CREADO'}")
        print(f"[ok] virtual mouse : {mouse.path if mouse else 'NO CREADO'}")

        inj = InputDevice(phys_path)

        def send(t, c, v):
            inj.write_event(InputEvent(0, 0, t, c, v))
            inj.write_event(InputEvent(0, 0, e.EV_SYN, e.SYN_REPORT, 0))
            time.sleep(0.03)

        print("\n[..] inyectando en el nodo físico:")
        send(e.EV_KEY, e.BTN_SOUTH, 1)   # bind -> BTN_SOUTH en el pad virtual
        send(e.EV_KEY, e.BTN_C, 1)       # bind -> BTN_C (el pad virtual lo declara)
        send(e.EV_KEY, e.BTN_TL, 1)      # sin bind -> passthrough
        send(e.EV_KEY, e.BTN_TL2, 1)     # digital del gatillo -> descartado
        send(e.EV_KEY, e.BTN_Z, 1)       # bind ignore -> descartado
        send(e.EV_KEY, e.BTN_NORTH, 1)   # bind -> KEY_SPACE en el teclado virtual
        send(e.EV_KEY, e.BTN_WEST, 1)    # bind -> BTN_LEFT en el ratón virtual
        send(e.EV_ABS, e.ABS_X, 255)     # stick izq. a tope -> 32767
        send(e.EV_ABS, e.ABS_X, 128)     # centro -> 0
        send(e.EV_ABS, e.ABS_Z, 0)       # stick der. X a tope negativo -> -32767
        send(e.EV_ABS, e.ABS_GAS, 255)   # gatillo der. a tope -> ABS_RZ 255
        send(e.EV_ABS, e.ABS_GAS, 50)    # hair trigger: 50/255=0.196 -> ~22
        send(e.EV_ABS, e.ABS_GAS, 20)    # por debajo del umbral -> 0
        send(e.EV_ABS, e.ABS_HAT0X, 1)   # hat passthrough
        for code in (e.BTN_SOUTH, e.BTN_C, e.BTN_TL, e.BTN_TL2, e.BTN_Z,
                     e.BTN_NORTH, e.BTN_WEST):
            send(e.EV_KEY, code, 0)

        pad_events = drain(pad)
        kbd_events = drain(kbd) if kbd else []
        mouse_events = drain(mouse) if mouse else []

        print("\n=== gamepad virtual ===")
        for t, c, v in pad_events:
            print(f"  {name_of(t, c):<12} = {v}")
        print("=== teclado virtual ===")
        for t, c, v in kbd_events:
            print(f"  {name_of(t, c):<12} = {v}")
        print("=== ratón virtual ===")
        for t, c, v in mouse_events:
            print(f"  {name_of(t, c):<12} = {v}")

        checks = [
            ("BTN_SOUTH reenviado al pad", (e.EV_KEY, e.BTN_SOUTH, 1) in pad_events),
            ("BTN_C declarado y reenviado", (e.EV_KEY, e.BTN_C, 1) in pad_events),
            ("BTN_TL passthrough", (e.EV_KEY, e.BTN_TL, 1) in pad_events),
            ("BTN_TL2 descartado", (e.EV_KEY, e.BTN_TL2, 1) not in pad_events),
            ("BTN_Z descartado", (e.EV_KEY, e.BTN_Z, 1) not in pad_events),
            ("BTN_NORTH no llega al pad", (e.EV_KEY, e.BTN_NORTH, 1) not in pad_events),
            ("BTN_NORTH -> KEY_SPACE (kbd)", (e.EV_KEY, e.KEY_SPACE, 1) in kbd_events),
            ("BTN_WEST -> BTN_LEFT (mouse)", (e.EV_KEY, e.BTN_LEFT, 1) in mouse_events),
            ("ABS_X 255 -> 32767", (e.EV_ABS, e.ABS_X, 32767) in pad_events),
            ("ABS_X 128 -> 0", (e.EV_ABS, e.ABS_X, 0) in pad_events),
            ("ABS_Z 0 -> ABS_RX -32767", (e.EV_ABS, e.ABS_RX, -32767) in pad_events),
            ("ABS_GAS 255 -> ABS_RZ 255", (e.EV_ABS, e.ABS_RZ, 255) in pad_events),
            ("ABS_GAS 50 -> ABS_RZ 22", (e.EV_ABS, e.ABS_RZ, 22) in pad_events),
            ("ABS_GAS 20 -> ABS_RZ 0", (e.EV_ABS, e.ABS_RZ, 0) in pad_events),
            ("HAT0X passthrough", (e.EV_ABS, e.ABS_HAT0X, 1) in pad_events),
        ]
        print("\n=== comprobaciones ===")
        failed = 0
        for label, ok in checks:
            print(f"  [{'ok ' if ok else 'FAIL'}] {label}")
            failed += 0 if ok else 1

        inj.close()
        return 1 if failed else 0
    finally:
        try:
            out, _ = proc.communicate(timeout=20)
        except subprocess.TimeoutExpired:
            proc.kill()
            out, _ = proc.communicate()
        print("\n=== salida del puente ===")
        print(out.strip()[-1500:])


if __name__ == "__main__":
    sys.exit(main())
