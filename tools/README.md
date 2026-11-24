# tools — bancos de pruebas con el hardware real

Pruebas de integración que requieren el **GameSir Tegenaria Lite conectado**. No
forman parte del binario: son scripts Python que usan `python-evdev` para
**inyectar** eventos en el nodo físico y leer los dispositivos virtuales.

Como el puente hace `grab()` del mando, los eventos inyectados solo los ve el
puente: no llegan al escritorio ni a ningún juego.

| Script | Qué verifica |
| --- | --- |
| `hw_bridge_test.py` | Extremo a extremo del puente: binds a gamepad/teclado/ratón/`ignore`, passthrough, descartes de `BTN_TL2`/`BTN_Z`, traducción de ejes DInput 8 bits → Xbox 16 bits, hair trigger y hat. 15 comprobaciones. |
| `stick_to_mouse_test.py` | Modo stick-a-ratón: caudal en cuentas/s según `speed`, dirección, ausencia de deriva vertical y `suppress_axis` (el eje no llega al gamepad virtual). **Mueve el cursor real.** |
| `sigint_cleanup_test.py` | Que SIGINT (Ctrl+C) termine el puente, destruya los dispositivos virtuales y libere el `grab()` (un segundo puente debe poder capturar el mando). |

## Uso

```bash
cargo build                     # o: cargo build --release
python tools/hw_bridge_test.py
python tools/stick_to_mouse_test.py
python tools/sigint_cleanup_test.py
```

Requisitos: `python-evdev` (`sudo pacman -S python-evdev`) y el usuario en el
grupo `input`. Por defecto usan `target/debug/gamesir-rs`; se puede cambiar con
la variable de entorno `GAMESIR_BIN`:

```bash
GAMESIR_BIN=target/release/gamesir-rs python tools/hw_bridge_test.py
```

Salen con código 0 si todo pasa y 1 si falla alguna comprobación.

El nodo físico se resuelve solo a través de
`/dev/input/by-id/usb-*GameSir-Tegenaria*-event-joystick`, así que no importa
que el número de `eventNNN` cambie al reconectar.
