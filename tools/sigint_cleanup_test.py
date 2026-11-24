#!/usr/bin/env python3
"""Comprueba que SIGINT (lo que envía Ctrl+C) termina el puente y libera todo.

Verifica el código de retorno, la desaparición de los dispositivos virtuales y
que un segundo puente vuelve a capturar el mando sin EBUSY (es decir, que el
grab() no se quedó enganchado).

Requisitos: python-evdev. Uso:
    python tools/sigint_cleanup_test.py
"""
import os
import signal
import subprocess
import sys
import time

from evdev import InputDevice, list_devices

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("GAMESIR_BIN", os.path.join(ROOT, "target/debug/gamesir-rs"))
VIRTUAL_NAMES = (
    "Tegenaria Remapped (pad)",
    "Tegenaria Remapped (kbd)",
    "Tegenaria Remapped (mouse)",
)


def virtual_nodes():
    found = []
    for path in list_devices():
        try:
            dev = InputDevice(path)
        except OSError:
            continue
        if dev.name in VIRTUAL_NAMES:
            found.append((dev.name, path))
        else:
            dev.close()
    return sorted(found)


def main():
    # start_new_session: el proceso hereda el manejo por defecto de SIGINT
    # (bash marca SIGINT/SIGQUIT como ignorados en sus trabajos en segundo plano).
    proc = subprocess.Popen(
        [BIN, "--bridge", "-t", "60"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    time.sleep(2.0)
    durante = virtual_nodes()
    print(f"[..] nodos virtuales durante el puente: {durante}")

    with open(f"/proc/{proc.pid}/status") as f:
        masks = [l.strip() for l in f if l.startswith(("SigIgn", "SigCgt"))]
    print(f"[..] máscaras de señal: {masks}")

    proc.send_signal(signal.SIGINT)
    try:
        proc.communicate(timeout=5)
        print(f"[..] código de retorno: {proc.returncode} (-2 = muerto por SIGINT)")
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.communicate()
        print("[FALLO] no respondió a SIGINT")
        return 1

    time.sleep(1.0)
    tras = virtual_nodes()
    print(f"[..] nodos virtuales tras SIGINT: {tras or '(ninguno: liberados)'}")

    second = subprocess.run(
        [BIN, "--bridge", "-t", "1.5"], capture_output=True, text=True
    )
    print(f"[..] segundo puente -> exit {second.returncode}")
    tail = (second.stdout or second.stderr).strip().splitlines()
    print(f"    {tail[-1] if tail else '(sin salida)'}")

    fallos = []
    if proc.returncode != -signal.SIGINT:
        fallos.append(f"SIGINT no terminó el puente (retorno {proc.returncode})")
    if durante == []:
        fallos.append("no se crearon dispositivos virtuales")
    if tras:
        fallos.append(f"quedan nodos virtuales: {tras}")
    if second.returncode != 0:
        fallos.append("el segundo puente no pudo capturar el mando (¿grab pegado?)")

    for fallo in fallos:
        print(f"[FALLO] {fallo}")
    if not fallos:
        print("[ok] SIGINT libera grab() y destruye los dispositivos virtuales")
    return 1 if fallos else 0


if __name__ == "__main__":
    sys.exit(main())
