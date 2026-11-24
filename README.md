# gamesir-rs

Gestor/remapeador en Rust para el mando cableado **GameSir Tegenaria Lite**
(VID:PID `3537:1093`) en CachyOS/Arch Linux.

**Estado: fase 2B — listo para uso diario en segundo plano.** Bandeja del
sistema (StatusNotifierItem por D-Bus, nativo en Wayland), modo `--daemon`,
reconexión automática del puente al reconectar el cable y sondeo con epoll.

## Qué hace hoy

- **Interfaz gráfica nativa** (`eframe`/`egui`, sin dependencias de GTK/Qt):
  barra superior con estado de conexión, selector de perfil y botón de
  Activar/Detener puente; pestaña de monitor en vivo; pestaña de remapeo y
  ajustes. El puente corre en su propio hilo y nunca bloquea el pintado.
- **Bandeja del sistema** por D-Bus (`ksni`, protocolo StatusNotifierItem: el
  nativo de Wayland/Hyprland/Waybar, sin X11): mostrar/ocultar la ventana,
  activar/detener el puente, submenú de perfiles con checkmark y «Salir».
  El ícono es un mando dibujado a mano que se pone verde con el puente activo.
- **Modo `--daemon`**: arranca en segundo plano con el puente activo y sin
  ventana; la bandeja puede abrir la ventana más tarde. Ctrl+C (SIGINT) lo
  termina y el kernel libera el `grab()` y los dispositivos virtuales.
- **Ocultar al cerrar**: con el puente activo, la X de la ventana la oculta a
  la bandeja en vez de matar el proceso (opción en la barra superior); el
  «Salir» de la bandeja cierra de verdad. En Wayland una ventana no se puede
  desmapear, así que ocultar es cerrar sin salir y mostrar es reabrir: el
  estado (perfil, monitor, puente) sobrevive porque vive fuera de la ventana.
- **Reconexión automática**: si desconectas el cable con el puente pedido, el
  hilo pasa a «esperando reconexión» y reactiva el puente solo al volver el
  mando, con el perfil que estuviera activo.
- **Espera con epoll** sobre el nodo evdev y un `eventfd` de comandos: sin
  sondeo de 1 ms, el hilo duerme hasta que hay eventos o toca un plazo
  (≈0,1 % de CPU en reposo).
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
  `serde`), `serde` + `derive`, `toml`, `eframe` (`egui`), `ksni` (bandeja) y
  `libc` (epoll/eventfd).
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
cargo run --release -- --daemon              # segundo plano: puente + bandeja, sin ventana
cargo run --release -- --bridge              # puente en la terminal, sin GUI
cargo run --release -- --list                # gamepads detectados y capacidades
cargo run --release -- --watch               # vista en vivo de botones y ejes
cargo run --release -- --profile mi.toml     # perfil concreto (también en la GUI)
cargo run --release -- --write-default-profile ~/.config/gamesir-rs/prueba.toml
```

| Opción | Efecto |
| --- | --- |
| `--daemon`, `--minimized` | segundo plano con el puente activo y sin ventana |
| `-b`, `--bridge` | puente en la terminal en vez de abrir la GUI |
| `-d`, `--device RUTA` | usar un nodo `/dev/input/eventXX` concreto |
| `-t`, `--duration SEG` | cerrar el puente tras SEG segundos |
| `-n`, `--no-grab` | no capturar el físico en exclusivo (para probar en paralelo) |
| `-v`, `--verbose` | imprimir cada evento y su destino |
| `-h`, `--help` | ayuda |

### Instalación en el menú de aplicaciones

```bash
./install.sh              # compila y copia binario + ícono + .desktop en ~/.local
./install.sh --no-build   # solo reinstala lo que ya hay en target/release
```

Copia `target/release/gamesir-rs` a `~/.local/bin/`, el ícono
(`assets/gamesir-rs.svg`, el mismo mando verde de la bandeja) a
`~/.local/share/icons/hicolor/scalable/apps/` y el lanzador
«GameSir Tegenaria Mapper» (`assets/gamesir-rs.desktop`, `Terminal=false`,
categorías `Game;Utility;`) a `~/.local/share/applications/`, refrescando la
base de datos de `.desktop` y el caché de iconos si las herramientas existen.
Con eso aparece en Rofi/Wofi/Fuzzel sin tocar la terminal; no se instala
ningún autostart: se abre a mano cuando se va a usar el mando.

Ctrl+C termina el puente o el daemon: el kernel cierra los descriptores al
morir el proceso, así que el `grab()` y los dispositivos virtuales se liberan
solos (comprobado). Con la ventana abierta y el puente activo, la X la oculta
a la bandeja si la opción «Ocultar al cerrar» está marcada (por defecto sí);
con el puente parado, o sin bandeja, la X termina el proceso como siempre.

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

## Bandeja del sistema

El ícono se registra como **StatusNotifierItem** por el bus D-Bus de sesión
(`ksni`), el protocolo de bandeja nativo de Wayland: lo muestran Waybar, KDE o
cualquier host compatible, sin X11. Menú contextual:

| Ítem | Efecto |
| --- | --- |
| Mostrar / Ocultar ventana | abre o esconde la ventana principal |
| Activar / Detener puente del mando | pide o suelta el puente (deshabilitado sin mando) |
| Perfiles › | submenú con el embebido y los TOML guardados; checkmark en el activo |
| Salir | cierra ventana, hilo de entrada y dispositivos virtuales |

Un clic izquierdo sobre el ícono muestra/oculta la ventana. El ícono (un mando
22×22 dibujado en código, ARGB32) se pone **verde** con el puente activo y gris
en reposo; el tooltip resume el estado («… · puente activo (mando capturado)»,
«sin mando · esperando reconexión (puente pedido)»…).

Detalles de implementación que explican su comportamiento:

- En Wayland el cliente **no puede desmapear** su propia ventana (el
  `set_visible` de winit no hace nada), y winit solo crea el bucle de eventos
  en el hilo principal: por eso el hilo principal supervisa la ventana
  (abrir → cerrar → reabrir) y un hilo aparte controla eventos y bandeja.
  Ocultar tarda un frame en cerrarse y mostrar recrea la ventana al instante;
  el estado no se pierde porque no vive en la ventana.
- Si no hay host de bandeja en la sesión, el programa avisa por stderr y
  sigue funcionando sin ella (la X entonces termina el proceso).
- El menú se puede pulsar desde scripts igual que lo hace Waybar:
  `python tools/tray_menu_test.py list` / `click "Salir"`.

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
src/main.rs      CLI: parseo de argumentos y elección de modo (GUI/daemon/list/watch/puente)
src/gui.rs       ventana eframe/egui, controlador (eventos+bandeja+ventana) e hilo de entrada con epoll
src/tray.rs      bandeja StatusNotifierItem (ksni): ícono, menú y comandos
src/input.rs     detección, grab(), dispositivos virtuales uinput, remapeo y bucle
src/profile.rs   tipos del perfil, carga/guardado TOML y saneado de valores
profiles/default.toml   perfil de prueba (embebido en el binario con include_str!)
assets/          ícono SVG y lanzador .desktop instalados por install.sh
install.sh       copia binario + ícono + .desktop en ~/.local (menú de aplicaciones)
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
- **Fase 2B sobre el escritorio real**: el ícono aparece en el host
  StatusNotifier de Waybar y su menú se pulsa por D-Bus (`tools/tray_menu_test.py`):
  «Activar/Detener puente» crea y destruye los tres virtuales; el ícono cambia
  de gris a verde (comprobado decodificando el `IconPixmap` del bus); con el
  puente activo la X oculta la ventana en <0,5 s sin matar el proceso y
  «Mostrar ventana» la reabre con el perfil y el puente intactos; el submenú
  de perfiles carga un TOML y mueve el checkmark; «Salir» deja proceso,
  virtuales, bandeja y ventana a cero.
- **Reconexión con el cable real**: desconectado el USB con el puente activo,
  el muestreo del bus muestra `virt=0 · esperando reconexión (puente pedido)`
  y, al reconectar, el puente se reactiva solo (`virt=2/3 · puente activo`)
  sin tocar nada.
- **`--daemon`**: arranca sin ventana con el puente activo y bandeja; abre y
  oculta la ventana desde el menú; con SIGINT termina y el kernel libera los
  virtuales. Inyección de 5 eventos contra su puente (epoll): passthrough,
  bind a tecla, bind a ratón, `ignore` y `ABS_X 255 → 32767`, 5/5 correctos.
  CPU en reposo: 10 ms de CPU en 10 s de reloj (0,1 %).

Los bancos de pruebas de hardware y de bandeja viven en [`tools/`](tools/README.md);
se repiten con `python tools/<script>.py` (los de hardware necesitan
`python-evdev` y el mando conectado; el de bandeja, `python-dbus`).

Pendiente de identificar en el hardware: `BTN_C` y `BTN_Z` están declarados por
el mando pero no se ha confirmado qué botones físicos son (probablemente los
traseros).

## Pendientes

- Identificar en el hardware qué botones físicos son `BTN_C` y `BTN_Z`.
- Arranque automático con la sesión (unidad systemd de usuario o entrada de
  autostart de Wayland) que lance `--daemon`.
