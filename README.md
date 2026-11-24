# gamesir-rs

Gestor/remapeador en Rust para el mando cableado **GameSir Tegenaria Lite**
(VID:PID `3537:1093`) en CachyOS/Arch Linux.

**Estado: fase 2A — GUI nativa con `eframe`/`egui`.** Quedan pendientes el modo
segundo plano (bandeja o `--daemon`) y el sondeo con epoll (fase 2B).

## Qué hace hoy

- **Interfaz gráfica nativa** (`eframe`/`egui`, sin dependencias de GTK/Qt):
  barra superior con estado de conexión, selector de perfil y botón de
  Activar/Detener puente; pestaña de monitor en vivo; pestaña de remapeo y
  ajustes. El puente corre en su propio hilo y nunca bloquea el pintado.
- **Detección automática** del Tegenaria Lite entre todos los nodos de
  `/dev/input`. El mando expone tres nodos con el mismo nombre (gamepad,
  teclado y ratón); se elige el que tiene teclas de gamepad + ejes `ABS_X`/
  `ABS_Y`, y se puntúa por VID/PID y nombre. Si se desconecta y se vuelve a
  conectar, el monitor lo retoma solo.
- **Monitor en vivo** de botones (con su código real, para identificar `BTN_C`
  y `BTN_Z`), sticks, gatillos y cruceta, más un registro de pulsaciones.
- **Editor visual de binds** (botón físico → mando/teclado/ratón/ignorar) y
  deslizadores de zona muerta, sensibilidad, umbral de hair trigger y
  stick-a-ratón. Los ajustes numéricos se aplican en caliente; los cambios que
  exigen declarar dispositivos virtuales nuevos reinician el puente solos.
- **Vista en vivo** de botones y ejes tal cual llegan del hardware (`--watch`).
- **Puente de entrada**: `grab()` exclusivo del mando físico (los juegos dejan
  de verlo) + dispositivos virtuales uinput.
- **Remapeo** de botones a botones de gamepad, teclas de teclado o clics de
  ratón, definido en un perfil TOML.
- **Sticks Hall Effect**: zona muerta radial y sensibilidad por stick.
- **Gatillos L2/R2**: hair trigger (umbral rápido) con reescalado del recorrido.
- **Stick-a-ratón**: convierte la deflexión de un stick en movimiento `REL_X`/
  `REL_Y`, con velocidad configurable, botón de retención opcional y supresión
  del eje en el gamepad virtual.
- **Perfiles TOML** con validación estricta: claves desconocidas o nombres de
  botón inexistentes dan error en vez de ignorarse en silencio.

Los dispositivos virtuales se crean según lo que pida el perfil: el gamepad
siempre, el teclado solo si hay binds a teclas y el ratón solo si hay binds a
clics o stick-a-ratón activo. El gamepad virtual declara además los códigos de
botón que usen los binds (por ejemplo `BTN_C`).

## Requisitos y permisos

- Rust (probado con 1.100.0 en CachyOS). Dependencias: `evdev` (con la feature
  `serde`), `serde` + `derive`, `toml` y `eframe` (`egui`).
- **No hace falta root** si tu usuario pertenece al grupo `input` y
  `/dev/uinput` tiene la ACL `uaccess` de systemd-logind (caso habitual en un
  escritorio Arch):

  ```bash
  groups | tr ' ' '\n' | grep -x input   # tu usuario en el grupo input
  ls -l /dev/uinput                      # crw-rw----+ root input
  ```

- En un equipo donde eso no se cumpla, hay una regla udev opcional en
  [`udev/99-gamesir.rules`](udev/99-gamesir.rules):

  ```bash
  sudo cp udev/99-gamesir.rules /etc/udev/rules.d/
  sudo udevadm control --reload-rules && sudo udevadm trigger
  ```

## Uso

```bash
cargo run --release                          # abre la interfaz gráfica
cargo run --release -- --bridge              # puente en la terminal, sin GUI
cargo run --release -- --list                # gamepads detectados y capacidades
cargo run --release -- --watch               # vista en vivo de botones y ejes
cargo run --release -- --profile mi.toml     # perfil concreto (también en la GUI)
cargo run --release -- --write-default-profile ~/.config/gamesir-rs/prueba.toml
```

| Opción | Efecto |
| --- | --- |
| `-b`, `--bridge` | puente en la terminal en vez de abrir la GUI |
| `-d`, `--device RUTA` | usar un nodo `/dev/input/eventXX` concreto |
| `-t`, `--duration SEG` | cerrar el puente tras SEG segundos |
| `-n`, `--no-grab` | no capturar el físico en exclusivo (para probar en paralelo) |
| `-v`, `--verbose` | imprimir cada evento y su destino |
| `-h`, `--help` | ayuda |

Ctrl+C termina el puente: el kernel cierra los descriptores al morir el
proceso, así que el `grab()` y los dispositivos virtuales se liberan solos
(comprobado). Al cerrar la ventana de la GUI ocurre lo mismo: el `Drop` de la
aplicación apaga el hilo de trabajo y este suelta el mando.

> Ojo si lo lanzas en segundo plano desde un script no interactivo
> (`gamesir-rs &`): bash marca esos procesos con SIGINT/SIGQUIT ignorados, así
> que Ctrl+C no los parará. Usa `--duration` o `kill -TERM`.

## Interfaz gráfica

- **Barra superior**: punto de estado con el nombre y la ruta del nodo
  detectado (o «no detectado»), VID:PID y modo actual; selector de perfil con
  los TOML de `~/.config/gamesir-rs/profiles/` más el embebido; botones
  Guardar / Guardar como… / Nuevo; y el botón grande **▶ Activar puente** /
  **■ Detener puente** (deshabilitado si no hay mando).
- **Pestaña «Monitor en vivo»**: rejilla de chips con todos los botones que
  declara el mando —se encienden en verde al pulsarlos, con su código real—,
  barras de los dos sticks y de los gatillos (valor normalizado y valor crudo),
  cruceta y registro de las últimas pulsaciones.
- **Pestaña «Remapeo y ajustes»**: una fila por botón físico con selector de
  acción (Automático / Botón de mando / Tecla / Clic de ratón / Ignorar),
  selector de destino y botón para quitar el bind; debajo, los deslizadores de
  zona muerta y sensibilidad por stick, el hair trigger con su umbral y el
  bloque de stick-a-ratón (stick, velocidad, supresión del eje y botón de
  retención).
- **Barra inferior**: nodos virtuales creados, contadores de eventos y el
  último aviso o error.

Los perfiles que guarda la GUI van a `~/.config/gamesir-rs/profiles/`
(`$XDG_CONFIG_HOME` si está definido). Los cambios de sliders y checkboxes se
aplican al puente en caliente; si un cambio exige declarar códigos o
dispositivos virtuales nuevos (editar binds o activar stick-a-ratón), el hilo
reinicia el puente tras 400 ms de calma para no recrear dispositivos en cada
frame de un arrastre.

## Formato del perfil

Los botones y ejes se escriben con su nombre de `<linux/input-event-codes.h>`
(`BTN_SOUTH`, `KEY_SPACE`, `BTN_LEFT`, ...). Las secciones que falten toman sus
valores por defecto.

```toml
name = "Mi perfil"

[[bind]]
from = "BTN_NORTH"        # botón físico (lo que envía el mando)
to = { key = "KEY_SPACE" } # destino: { pad = ... }, { key = ... }, { mouse = ... } o "ignore"

[left_stick]
deadzone = 0.08           # zona muerta radial, 0.0 .. 0.95
sensitivity = 1.0         # ganancia, 0.0 .. 4.0

[right_stick]
deadzone = 0.10
sensitivity = 1.0

[triggers]
hair_trigger = true       # el recorrido útil empieza en `threshold`
threshold = 0.12          # umbral de pulsación, 0.01 .. 1.0

[stick_to_mouse]
enabled = false
stick = "right"           # "left" o "right"
hold = "BTN_SELECT"       # botón que hay que mantener; quítalo para modo siempre activo
speed = 900.0             # cuentas por segundo con el stick a tope
suppress_axis = true      # no enviar ese stick al gamepad virtual mientras mueve el ratón
```

El perfil embebido de prueba está en [`profiles/default.toml`](profiles/default.toml)
y se vuelca con `--write-default-profile`.

Nota sobre los nombres: `evdev` de Python llamaba `BTN_A`/`BTN_B` a los códigos
`0x130`/`0x131`, que en el kernel (y en la crate `evdev` de Rust) son
`BTN_SOUTH`/`BTN_EAST`. Es el mismo botón con distinto alias, por eso los
botones no necesitan traducción.

## Qué traduce el puente

El mando físico habla DInput con ejes de 8 bits; el gamepad virtual se expone
como un mando Xbox-like con sticks de 16 bits.

| Físico (8 bits) | Virtual | Comentario |
| --- | --- | --- |
| `ABS_X` / `ABS_Y` (0..255) | `ABS_X` / `ABS_Y` (±32767) | stick izquierdo + deadzone/sensibilidad |
| `ABS_Z` / `ABS_RZ` (0..255) | `ABS_RX` / `ABS_RY` (±32767) | stick derecho + deadzone/sensibilidad |
| `ABS_BRAKE` (0..255) | `ABS_Z` (0..255) | gatillo izquierdo + hair trigger |
| `ABS_GAS` (0..255) | `ABS_RZ` (0..255) | gatillo derecho + hair trigger |
| `ABS_HAT0X` / `ABS_HAT0Y` | iguales | cruceta, sin cambios |
| `BTN_SOUTH`..`BTN_THUMBR` | iguales | los binds del perfil tienen prioridad |
| `BTN_TL2` / `BTN_TR2` | — | duplicado digital de los gatillos: se descarta |

## Estructura

```
src/main.rs      CLI: parseo de argumentos y elección de modo (GUI/list/watch/puente)
src/gui.rs       interfaz eframe/egui y el hilo de trabajo que habla con el mando
src/input.rs     detección, grab(), dispositivos virtuales uinput, remapeo y bucle
src/profile.rs   tipos del perfil, carga/guardado TOML y saneado de valores
profiles/default.toml   perfil de prueba (embebido en el binario con include_str!)
udev/99-gamesir.rules   regla opcional para equipos sin grupo input/uaccess
tools/           bancos de pruebas con el hardware real (Python + python-evdev)
```

## Verificación

- `cargo test` → 15 tests unitarios de la matemática de ejes (normalización,
  zona muerta radial, saturación de sensibilidad, hair trigger, escalado), del
  parseo de perfiles (defaults, recorte de valores, rechazo de claves y
  nombres inválidos, ida y vuelta TOML) y de la firma de capacidades que
  decide si un perfil puede aplicarse en caliente.
- `cargo clippy --all-targets` sin avisos, `cargo fmt` aplicado.
- **Prueba de extremo a extremo con el mando real** (inyectando eventos en el
  nodo físico, que al estar capturado en exclusivo solo ve el puente): binds a
  gamepad/teclado/ratón/ignore, passthrough, descartes de `BTN_TL2`/`BTN_Z`,
  `ABS_X 255 → 32767`, `ABS_X 128 → 0`, `ABS_Z 0 → ABS_RX -32767`,
  `ABS_GAS 255/50/20 → ABS_RZ 255/22/0` (hair trigger), hat, y limpieza al
  salir. 15/15 comprobaciones correctas.
- **Stick-a-ratón**: 897 cuentas en 1.00 s con `speed = 900` (esperadas ~900),
  -273 en 0.30 s hacia la izquierda, `REL_Y = 0` y ningún `ABS_RX` en el gamepad
  virtual con `suppress_axis = true`.
- **SIGINT**: retorno -2, nodos virtuales destruidos y el siguiente puente
  vuelve a capturar el físico sin `EBUSY`.
- **GUI sobre el escritorio real** (Hyprland/Wayland, con capturas de pantalla
  y clics simulados con `ydotool`): la ventana abre y pinta las dos pestañas;
  con eventos inyectados en el nodo físico se encienden los chips de
  `BTN_SOUTH`/`BTN_C`, se deflectan las barras de stick y gatillo y se rellena
  el registro de pulsaciones; el botón **Activar puente** crea los tres
  dispositivos virtuales y cambia el estado a «mando capturado»; al cerrar la
  ventana el proceso termina y no queda ningún dispositivo virtual.

Los tres bancos de pruebas de hardware viven en [`tools/`](tools/README.md); se
repiten con `python tools/<script>.py` (necesitan `python-evdev` y el mando
conectado).

Pendiente de identificar en el hardware: `BTN_C` y `BTN_Z` están declarados por
el mando pero no se ha confirmado qué botones físicos son (probablemente los
traseros).

## Siguiente fase (2B)

- Modo segundo plano: bandeja del sistema (`tray-icon`) y/o flag `--daemon`.
- Reactivar el puente automáticamente al reconectar el mando (el monitor ya se
  recupera solo; el puente no).
- Sondeo con epoll en vez del bucle actual de lectura no bloqueante + espera
  de 1 ms.
