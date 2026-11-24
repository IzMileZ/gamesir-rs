//! Interfaz gráfica (eframe/egui), bandeja y el hilo de trabajo que habla con
//! el mando.
//!
//! Tres actores: un **hilo de entrada** que abre el mando, lo lee y traduce
//! (espera con epoll en vez de sondear); un **controlador** en el hilo
//! principal que funde los eventos del hilo en un modelo compartido, atiende
//! la bandeja y gestiona el ciclo de vida de la ventana; y la **ventana**
//! egui, que se crea y se destruye sin matar el proceso (en Wayland no se
//! puede desmapear una ventana: ocultarla a la bandeja es cerrarla y volver a
//! abrirla después). Los comandos viajan por un canal `mpsc` despertado con un
//! `eventfd` para que el epoll del hilo de entrada los vea.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use evdev::{AbsoluteAxisCode, EventType, InputEvent, KeyCode};

use crate::input::{self, Bridge, Fetch, Outcome, PadInfo, PhysicalPad, Stats};
use crate::profile::{Action, Bind, Profile, Stick};
use crate::tray::{self, TrayCmd, TrayView};

const VERDE: egui::Color32 = egui::Color32::from_rgb(46, 160, 67);
const AMBAR: egui::Color32 = egui::Color32::from_rgb(210, 153, 34);
const ROJO: egui::Color32 = egui::Color32::from_rgb(199, 62, 62);
const GRIS_CHIP: egui::Color32 = egui::Color32::from_gray(48);

/// Cada cuánto se vuelve a buscar el mando cuando no hay ninguno abierto.
const DETECT_INTERVAL: Duration = Duration::from_millis(500);
/// Espera antes de reiniciar el puente por un cambio que exige recrear los
/// dispositivos virtuales (arrastrar un slider genera decenas de cambios).
const RESTART_DEBOUNCE: Duration = Duration::from_millis(400);
const STATS_INTERVAL: Duration = Duration::from_millis(500);
/// Espera máxima del epoll con el stick-a-ratón emitiendo: el propio `tick()`
/// del remapper agrupa el movimiento en pasos de 8 ms.
const MOUSE_WAIT: Duration = Duration::from_millis(4);
const EVENT_LOG_LEN: usize = 24;

/// Botones que se pueden elegir como destino en el gamepad virtual.
const PAD_TARGETS: [KeyCode; 15] = [
    KeyCode::BTN_SOUTH,
    KeyCode::BTN_EAST,
    KeyCode::BTN_NORTH,
    KeyCode::BTN_WEST,
    KeyCode::BTN_C,
    KeyCode::BTN_Z,
    KeyCode::BTN_TL,
    KeyCode::BTN_TR,
    KeyCode::BTN_TL2,
    KeyCode::BTN_TR2,
    KeyCode::BTN_SELECT,
    KeyCode::BTN_START,
    KeyCode::BTN_MODE,
    KeyCode::BTN_THUMBL,
    KeyCode::BTN_THUMBR,
];

const MOUSE_TARGETS: [KeyCode; 5] = [
    KeyCode::BTN_LEFT,
    KeyCode::BTN_RIGHT,
    KeyCode::BTN_MIDDLE,
    KeyCode::BTN_SIDE,
    KeyCode::BTN_EXTRA,
];

const KEY_TARGETS: [KeyCode; 71] = [
    KeyCode::KEY_A,
    KeyCode::KEY_B,
    KeyCode::KEY_C,
    KeyCode::KEY_D,
    KeyCode::KEY_E,
    KeyCode::KEY_F,
    KeyCode::KEY_G,
    KeyCode::KEY_H,
    KeyCode::KEY_I,
    KeyCode::KEY_J,
    KeyCode::KEY_K,
    KeyCode::KEY_L,
    KeyCode::KEY_M,
    KeyCode::KEY_N,
    KeyCode::KEY_O,
    KeyCode::KEY_P,
    KeyCode::KEY_Q,
    KeyCode::KEY_R,
    KeyCode::KEY_S,
    KeyCode::KEY_T,
    KeyCode::KEY_U,
    KeyCode::KEY_V,
    KeyCode::KEY_W,
    KeyCode::KEY_X,
    KeyCode::KEY_Y,
    KeyCode::KEY_Z,
    KeyCode::KEY_1,
    KeyCode::KEY_2,
    KeyCode::KEY_3,
    KeyCode::KEY_4,
    KeyCode::KEY_5,
    KeyCode::KEY_6,
    KeyCode::KEY_7,
    KeyCode::KEY_8,
    KeyCode::KEY_9,
    KeyCode::KEY_0,
    KeyCode::KEY_F1,
    KeyCode::KEY_F2,
    KeyCode::KEY_F3,
    KeyCode::KEY_F4,
    KeyCode::KEY_F5,
    KeyCode::KEY_F6,
    KeyCode::KEY_F7,
    KeyCode::KEY_F8,
    KeyCode::KEY_F9,
    KeyCode::KEY_F10,
    KeyCode::KEY_F11,
    KeyCode::KEY_F12,
    KeyCode::KEY_SPACE,
    KeyCode::KEY_ENTER,
    KeyCode::KEY_ESC,
    KeyCode::KEY_TAB,
    KeyCode::KEY_BACKSPACE,
    KeyCode::KEY_DELETE,
    KeyCode::KEY_INSERT,
    KeyCode::KEY_HOME,
    KeyCode::KEY_END,
    KeyCode::KEY_PAGEUP,
    KeyCode::KEY_PAGEDOWN,
    KeyCode::KEY_UP,
    KeyCode::KEY_DOWN,
    KeyCode::KEY_LEFT,
    KeyCode::KEY_RIGHT,
    KeyCode::KEY_LEFTSHIFT,
    KeyCode::KEY_LEFTCTRL,
    KeyCode::KEY_LEFTALT,
    KeyCode::KEY_RIGHTSHIFT,
    KeyCode::KEY_RIGHTCTRL,
    KeyCode::KEY_RIGHTALT,
    KeyCode::KEY_MINUS,
    KeyCode::KEY_EQUAL,
];

/// Cerrojo tolerante a pánicos: un hilo que muera dejando el cerrojo tomado
/// no debe arrastrar al resto (la ventana y el controlador comparten modelo).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Hilo de trabajo
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkerMode {
    /// Sin mando abierto.
    #[default]
    Idle,
    /// Leyendo el físico sin capturarlo: alimenta el monitor en vivo.
    Monitor,
    /// Puente activo: `grab()` del físico y dispositivos virtuales emitiendo.
    Bridge,
    /// El puente estaba pedido pero el mando se desconectó: en cuanto vuelva
    /// a aparecer se reactiva solo.
    Reconnect,
}

impl WorkerMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "inactivo",
            Self::Monitor => "monitorizando (sin capturar)",
            Self::Bridge => "puente activo (mando capturado)",
            Self::Reconnect => "esperando reconexión (puente pedido)",
        }
    }
}

pub enum Command {
    StartBridge,
    StopBridge,
    UpdateProfile(Profile),
    Shutdown,
}

pub enum Event {
    Mode(WorkerMode),
    Pad(Option<PadInfo>),
    Nodes(String),
    Log(String),
    Error(String),
    Button(u16, i32),
    Axis(u16, i32),
    Stats(Stats),
}

/// Estado del hilo. Los dos casos con dispositivo van en `Box` para que las
/// variantes pesen lo mismo (clippy::large_enum_variant) y moverlas sea barato.
#[derive(Default)]
enum State {
    #[default]
    Idle,
    Monitor(Box<PhysicalPad>),
    Bridge(Box<Bridge>),
}

/// Contador `eventfd` que despierta al epoll del hilo de entrada cuando
/// alguien envía un comando. Lo comparten todos los `CmdSender` y el hilo.
struct WakeFd(RawFd);

impl WakeFd {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(fd))
    }

    fn ping(&self) {
        let uno: u64 = 1;
        unsafe {
            libc::write(self.0, &uno as *const u64 as *const libc::c_void, 8);
        }
    }

    fn drain(&self) {
        let mut v: u64 = 0;
        while unsafe { libc::read(self.0, &mut v as *mut u64 as *mut libc::c_void, 8) } == 8 {}
    }
}

impl Drop for WakeFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

#[derive(Clone)]
pub struct CmdSender {
    tx: Sender<Command>,
    wake: Arc<WakeFd>,
}

impl CmdSender {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
        self.wake.ping();
    }
}

/// Espera simultánea del nodo evdev y del `eventfd` de comandos.
struct Poller {
    epfd: RawFd,
    pad: Option<RawFd>,
}

impl Poller {
    fn new(wake: RawFd) -> io::Result<Self> {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let poller = Self { epfd, pad: None };
        poller.ctl(libc::EPOLL_CTL_ADD, wake)?;
        Ok(poller)
    }

    fn ctl(&self, op: i32, fd: RawFd) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: fd as u64,
        };
        if unsafe { libc::epoll_ctl(self.epfd, op, fd, &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_pad(&mut self, fd: Option<RawFd>) {
        if self.pad == fd {
            return;
        }
        if let Some(old) = self.pad {
            let _ = self.ctl(libc::EPOLL_CTL_DEL, old);
        }
        if let Some(new) = fd
            && let Err(e) = self.ctl(libc::EPOLL_CTL_ADD, new)
        {
            eprintln!("[!] epoll: {e}");
        }
        self.pad = fd;
    }

    fn wait(&self, timeout: Duration) {
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 2];
        unsafe {
            libc::epoll_wait(self.epfd, evs.as_mut_ptr(), evs.len() as i32, ms);
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        unsafe { libc::close(self.epfd) };
    }
}

struct Worker {
    cmds: Receiver<Command>,
    wake: Arc<WakeFd>,
    poller: Poller,
    out: Sender<Event>,
    state: State,
    profile: Profile,
    signature: (Vec<(u16, Action)>, bool),
    pending: Option<(Profile, Instant)>,
    buf: Vec<InputEvent>,
    last_detect: Instant,
    last_stats: Instant,
    stats: Stats,
    /// El usuario quiere el puente activo, tenga o no mando delante ahora
    /// mismo: es lo que permite reactivarlo al reconectar el cable.
    bridge_intent: bool,
}

impl Worker {
    fn new(cmds: Receiver<Command>, wake: Arc<WakeFd>, out: Sender<Event>) -> io::Result<Self> {
        let poller = Poller::new(wake.0)?;
        Ok(Self {
            cmds,
            wake,
            poller,
            out,
            state: State::Idle,
            profile: Profile::embedded_default(),
            signature: Profile::default().caps_signature(),
            pending: None,
            buf: Vec::with_capacity(64),
            last_detect: Instant::now(),
            last_stats: Instant::now(),
            stats: Stats::default(),
            bridge_intent: false,
        })
    }

    fn run(&mut self) {
        self.start_monitor();
        while self.step() {}
        // Al destruirse `state` se cierran los descriptores: el kernel libera el
        // grab() y destruye los dispositivos virtuales.
    }

    /// Una iteración del bucle. Devuelve `false` para terminar el hilo.
    fn step(&mut self) -> bool {
        if !self.handle_commands() {
            return false;
        }
        self.poller.wait(self.wait_timeout());
        self.wake.drain();
        if !self.handle_commands() {
            return false;
        }

        let mut state = std::mem::take(&mut self.state);
        let mut buf = std::mem::take(&mut self.buf);
        let mut disconnected = false;
        let mut fatal: Option<String> = None;
        let mut want_monitor = false;
        let mut want_bridge = false;

        match &mut state {
            State::Idle => {
                if self.last_detect.elapsed() >= DETECT_INTERVAL {
                    self.last_detect = Instant::now();
                    if input::detect_pad_info().is_some() {
                        if self.bridge_intent {
                            want_bridge = true;
                        } else {
                            want_monitor = true;
                        }
                    }
                }
            }
            State::Monitor(pad) => match pad.fetch(&mut buf) {
                Ok(Fetch::Events) => {
                    for ev in &buf {
                        self.report(ev);
                    }
                }
                Ok(Fetch::Idle) => {}
                Ok(Fetch::Disconnected) => disconnected = true,
                Err(e) => fatal = Some(format!("lectura del mando: {e}")),
            },
            State::Bridge(bridge) => {
                match bridge.fetch(&mut buf) {
                    Ok(Fetch::Events) => {
                        for ev in &buf {
                            self.report(ev);
                            self.stats.events += 1;
                            match bridge.handle(ev) {
                                Ok(Outcome::Pad) => self.stats.to_pad += 1,
                                Ok(Outcome::Keyboard) => self.stats.to_keyboard += 1,
                                Ok(Outcome::Mouse) => self.stats.to_mouse += 1,
                                Ok(Outcome::Dropped) => self.stats.dropped += 1,
                                Ok(Outcome::Sync | Outcome::Ignored) => {}
                                Err(e) => fatal = Some(format!("emisión virtual: {e}")),
                            }
                        }
                    }
                    Ok(Fetch::Idle) => {}
                    Ok(Fetch::Disconnected) => disconnected = true,
                    Err(e) => fatal = Some(format!("lectura del mando: {e}")),
                }
                match bridge.tick() {
                    Ok(true) => self.stats.to_mouse += 1,
                    Ok(false) => {}
                    Err(e) => fatal = Some(format!("stick-a-ratón: {e}")),
                }
                if self.last_stats.elapsed() >= STATS_INTERVAL {
                    self.last_stats = Instant::now();
                    self.emit(Event::Stats(self.stats));
                }
            }
        }

        self.state = state;
        self.buf = buf;

        if let Some(msg) = fatal {
            self.emit(Event::Error(msg));
            disconnected = true;
        }
        if disconnected {
            self.on_disconnected();
        } else if want_bridge {
            self.start_bridge();
        } else if want_monitor {
            self.start_monitor();
        }
        self.apply_pending();
        self.sync_poller();
        true
    }

    /// Registra en el epoll el descriptor del mando abierto, si lo hay.
    fn sync_poller(&mut self) {
        let fd = match &self.state {
            State::Idle => None,
            State::Monitor(pad) => Some(pad.fd()),
            State::Bridge(bridge) => Some(bridge.physical.fd()),
        };
        self.poller.set_pad(fd);
    }

    /// Hasta cuánto puede dormir el epoll sin perder puntualidad.
    fn wait_timeout(&self) -> Duration {
        let mut espera = match &self.state {
            State::Idle => DETECT_INTERVAL.saturating_sub(self.last_detect.elapsed()),
            State::Monitor(_) => STATS_INTERVAL,
            State::Bridge(bridge) => {
                let stats = STATS_INTERVAL.saturating_sub(self.last_stats.elapsed());
                if bridge.mouse_active() {
                    stats.min(MOUSE_WAIT)
                } else {
                    stats
                }
            }
        };
        if let Some((_, desde)) = self.pending {
            espera = espera.min(RESTART_DEBOUNCE.saturating_sub(desde.elapsed()));
        }
        espera
    }

    fn handle_commands(&mut self) -> bool {
        loop {
            match self.cmds.try_recv() {
                Ok(Command::Shutdown) => return false,
                Ok(Command::StartBridge) => {
                    self.bridge_intent = true;
                    self.start_bridge();
                }
                Ok(Command::StopBridge) => {
                    self.bridge_intent = false;
                    self.stop_bridge();
                }
                Ok(Command::UpdateProfile(profile)) => self.update_profile(profile),
                Err(TryRecvError::Empty) => return true,
                // El controlador ha desaparecido: no tiene sentido seguir.
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn emit(&self, ev: Event) {
        let _ = self.out.send(ev);
    }

    /// Reenvía lo que llega del mando físico (antes de traducir).
    fn report(&self, ev: &InputEvent) {
        let type_ = ev.event_type();
        if type_ == EventType::KEY {
            self.emit(Event::Button(ev.code(), ev.value()));
        } else if type_ == EventType::ABSOLUTE {
            self.emit(Event::Axis(ev.code(), ev.value()));
        }
    }

    fn start_monitor(&mut self) {
        self.state = State::Idle;
        let Some(pad) = input::pick_pad(input::find_pads()) else {
            self.emit(Event::Pad(None));
            self.emit(Event::Mode(WorkerMode::Idle));
            self.emit(Event::Nodes(String::new()));
            return;
        };
        let info = pad.info();
        match PhysicalPad::new(pad) {
            Ok(physical) => {
                self.state = State::Monitor(Box::new(physical));
                self.emit(Event::Pad(Some(info)));
                self.emit(Event::Mode(WorkerMode::Monitor));
                self.emit(Event::Nodes(String::new()));
                self.emit(Event::Log("mando detectado, monitor en vivo activo".into()));
            }
            Err(e) => {
                self.emit(Event::Error(format!("no se pudo abrir el mando: {e}")));
                self.emit(Event::Mode(WorkerMode::Idle));
            }
        }
    }

    fn start_bridge(&mut self) {
        self.state = State::Idle;
        self.pending = None;
        let Some(pad) = input::pick_pad(input::find_pads()) else {
            self.emit(Event::Error(
                "no se detectó ningún gamepad: conecta el mando e inténtalo otra vez".into(),
            ));
            self.emit(Event::Pad(None));
            self.emit(Event::Nodes(String::new()));
            self.emit(Event::Mode(self.intent_mode()));
            return;
        };
        let info = pad.info();
        let profile = self.profile.clone();
        match Bridge::start(pad, profile.clone(), true) {
            Ok(bridge) => {
                self.signature = profile.caps_signature();
                self.stats = Stats::default();
                self.last_stats = Instant::now();
                let nodes = bridge.outputs.nodes.to_string();
                self.state = State::Bridge(Box::new(bridge));
                self.emit(Event::Pad(Some(info)));
                self.emit(Event::Nodes(nodes));
                self.emit(Event::Mode(WorkerMode::Bridge));
                self.emit(Event::Log(
                    "puente activo: físico capturado y dispositivos virtuales creados".into(),
                ));
            }
            Err(e) => {
                self.emit(Event::Error(format!("no se pudo iniciar el puente: {e}")));
                if self.bridge_intent {
                    // Se reintenta solo en cuanto el sondeo vuelva a ver el mando.
                    self.last_detect = Instant::now();
                    self.emit(Event::Mode(WorkerMode::Reconnect));
                } else {
                    self.start_monitor();
                }
            }
        }
    }

    fn stop_bridge(&mut self) {
        if matches!(self.state, State::Bridge(_)) {
            self.emit(Event::Stats(self.stats));
            self.emit(Event::Log("puente detenido, mando liberado".into()));
        }
        self.pending = None;
        self.start_monitor();
    }

    fn intent_mode(&self) -> WorkerMode {
        if self.bridge_intent {
            WorkerMode::Reconnect
        } else {
            WorkerMode::Idle
        }
    }

    fn on_disconnected(&mut self) {
        self.state = State::Idle;
        self.last_detect = Instant::now();
        self.emit(Event::Pad(None));
        self.emit(Event::Nodes(String::new()));
        if self.bridge_intent {
            self.emit(Event::Mode(WorkerMode::Reconnect));
            self.emit(Event::Log(
                "el mando se desconectó; el puente se reactivará al reconectarlo".into(),
            ));
        } else {
            self.emit(Event::Mode(WorkerMode::Idle));
            self.emit(Event::Log("el mando físico se desconectó".into()));
        }
    }

    fn update_profile(&mut self, profile: Profile) {
        let exige_recrear = match self.state {
            State::Bridge(_) => profile.caps_signature() != self.signature,
            _ => false,
        };
        if exige_recrear {
            // Se aplaza: mientras se arrastra un selector llegan muchos cambios
            // y recrear los dispositivos virtuales en cada uno sería carísimo.
            self.pending = Some((profile, Instant::now()));
            return;
        }
        self.pending = None;
        if let State::Bridge(bridge) = &mut self.state {
            bridge.set_tuning(profile.clone());
        }
        self.profile = profile;
    }

    fn apply_pending(&mut self) {
        let listo = self
            .pending
            .as_ref()
            .is_some_and(|(_, desde)| desde.elapsed() >= RESTART_DEBOUNCE);
        if !listo {
            return;
        }
        let (profile, _) = self.pending.take().expect("comprobado justo antes");
        self.profile = profile;
        self.emit(Event::Log(
            "reiniciando el puente para declarar los dispositivos virtuales nuevos".into(),
        ));
        self.start_bridge();
    }
}

// ---------------------------------------------------------------------------
// Modelo compartido y aplicación egui
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Monitor,
    Mapping,
}

/// Por qué se cerró la última ventana: para ocultarla a la bandeja o para
/// terminar el proceso de verdad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Hide,
    Quit,
}

/// Tipos de destino de un bind, tal y como se ofrecen en la interfaz.
#[derive(PartialEq, Clone, Copy)]
enum BindKind {
    Auto,
    Pad,
    Key,
    Mouse,
    Ignore,
}

impl BindKind {
    const ALL: [Self; 5] = [Self::Auto, Self::Pad, Self::Key, Self::Mouse, Self::Ignore];

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automático",
            Self::Pad => "Botón de mando",
            Self::Key => "Tecla",
            Self::Mouse => "Clic de ratón",
            Self::Ignore => "Ignorar",
        }
    }

    fn of(action: Option<Action>) -> Self {
        match action {
            None => Self::Auto,
            Some(Action::Pad(_)) => Self::Pad,
            Some(Action::Key(_)) => Self::Key,
            Some(Action::Mouse(_)) => Self::Mouse,
            Some(Action::Ignore) => Self::Ignore,
        }
    }

    /// Acción por defecto al cambiar de tipo, antes de elegir destino concreto.
    fn default_action(self) -> Option<Action> {
        match self {
            Self::Auto => None,
            Self::Pad => Some(Action::Pad(KeyCode::BTN_SOUTH)),
            Self::Key => Some(Action::Key(KeyCode::KEY_SPACE)),
            Self::Mouse => Some(Action::Mouse(KeyCode::BTN_LEFT)),
            Self::Ignore => Some(Action::Ignore),
        }
    }
}

/// Todo lo que sobrevive a un cierre de ventana: lo pintan las ventanas que
/// se vayan abriendo y lo lee el controlador para la bandeja.
pub struct GuiModel {
    tab: Tab,
    mode: WorkerMode,
    pad: Option<PadInfo>,
    nodes: String,
    buttons: BTreeMap<u16, i32>,
    axes: BTreeMap<u16, i32>,
    event_log: VecDeque<String>,
    log: String,
    error: Option<String>,
    stats: Option<Stats>,
    profile: Profile,
    /// Último perfil enviado al hilo, para detectar cambios sin ir widget a widget.
    sent: Profile,
    profile_path: Option<PathBuf>,
    available: Vec<PathBuf>,
    file_name: String,
    /// Con el puente activo, la X oculta la ventana en la bandeja en vez de
    /// matar el proceso.
    hide_on_close: bool,
    tray_ok: bool,
    /// Hay una ventana egui abierta ahora mismo (lo fija el supervisor).
    window_open: bool,
    /// Pedidos pendientes del controlador a la ventana abierta.
    hide_now: bool,
    quit_now: bool,
    close_reason: Option<CloseReason>,
}

impl GuiModel {
    fn new(profile: Profile, path: Option<PathBuf>) -> Self {
        let file_name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("mi_perfil.toml"));
        Self {
            tab: Tab::Monitor,
            mode: WorkerMode::Idle,
            pad: None,
            nodes: String::new(),
            buttons: BTreeMap::new(),
            axes: BTreeMap::new(),
            event_log: VecDeque::new(),
            log: "arrancando…".into(),
            error: None,
            stats: None,
            sent: profile.clone(),
            profile,
            profile_path: path,
            available: list_profiles(&profiles_dir()),
            file_name,
            hide_on_close: true,
            tray_ok: false,
            window_open: false,
            hide_now: false,
            quit_now: false,
            close_reason: None,
        }
    }

    fn push_event(&mut self, line: String) {
        self.event_log.push_front(line);
        while self.event_log.len() > EVENT_LOG_LEN {
            self.event_log.pop_back();
        }
    }

    fn push_profile(&mut self, cmds: &CmdSender) {
        self.sent = self.profile.clone();
        cmds.send(Command::UpdateProfile(self.profile.clone()));
    }

    fn paint(&mut self, ui: &mut egui::Ui, cmds: &CmdSender) {
        self.top_bar(ui, cmds);
        self.tab_bar(ui);
        self.status_bar(ui);

        let tab = self.tab;
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match tab {
                    Tab::Monitor => self.monitor_tab(ui),
                    Tab::Mapping => self.mapping_tab(ui),
                });
        });

        if self.profile != self.sent {
            self.push_profile(cmds);
        }
    }

    // -- barra superior -----------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui, cmds: &CmdSender) {
        egui::Panel::top("barra_superior").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let (color, texto) = match (&self.pad, self.mode) {
                    (Some(pad), WorkerMode::Bridge) => {
                        (VERDE, format!("{} — puente activo", pad.short()))
                    }
                    (Some(pad), WorkerMode::Monitor) => {
                        (VERDE, format!("{} — conectado", pad.short()))
                    }
                    (Some(pad), WorkerMode::Idle) => {
                        (AMBAR, format!("{} — conectado, en espera", pad.short()))
                    }
                    (_, WorkerMode::Reconnect) => (
                        AMBAR,
                        match &self.pad {
                            Some(pad) => format!("{} — reconectando…", pad.short()),
                            None => "mando desconectado: el puente se reactivará al reconectar"
                                .to_string(),
                        },
                    ),
                    (None, _) => (ROJO, "GameSir Tegenaria Lite no detectado".to_string()),
                };
                ui.colored_label(color, egui::RichText::new("●").size(15.0));
                ui.label(egui::RichText::new(texto).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(self.mode.label()).weak());
                    if let Some(pad) = &self.pad {
                        ui.label(
                            egui::RichText::new(format!("{:04x}:{:04x}", pad.vendor, pad.product))
                                .monospace()
                                .weak(),
                        );
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                self.profile_controls(ui);
                ui.separator();
                self.bridge_button(ui, cmds);
                ui.separator();
                ui.add_enabled(
                    self.tray_ok,
                    egui::Checkbox::new(&mut self.hide_on_close, "Ocultar al cerrar"),
                )
                .on_disabled_hover_text(
                    "Sin bandeja del sistema disponible, cerrar la ventana termina el proceso",
                )
                .on_hover_text(
                    "Con el puente activo, cerrar la ventana la oculta en la bandeja \n\
                     en vez de terminar el proceso (el «Salir» de la bandeja sí cierra)",
                );
            });
            ui.add_space(6.0);
        });
    }

    fn bridge_button(&mut self, ui: &mut egui::Ui, cmds: &CmdSender) {
        let pedido = matches!(self.mode, WorkerMode::Bridge | WorkerMode::Reconnect);
        let texto = if pedido {
            "■  Detener puente"
        } else {
            "▶  Activar puente"
        };
        let boton = egui::Button::new(
            egui::RichText::new(texto)
                .strong()
                .size(15.0)
                .color(egui::Color32::WHITE),
        )
        .fill(if pedido { ROJO } else { VERDE })
        .min_size(egui::vec2(210.0, 30.0));
        let respuesta = ui
            .add_enabled(pedido || self.pad.is_some(), boton)
            .on_disabled_hover_text("Conecta el mando para poder activar el puente");
        if respuesta.clicked() {
            if pedido {
                cmds.send(Command::StopBridge);
            } else {
                self.push_profile(cmds);
                cmds.send(Command::StartBridge);
            }
        }
    }

    fn profile_controls(&mut self, ui: &mut egui::Ui) {
        ui.label("Perfil:");
        let seleccion = match &self.profile_path {
            Some(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            None => format!("{} (embebido)", self.profile.name),
        };
        egui::ComboBox::from_id_salt("selector_perfil")
            .selected_text(seleccion)
            .width(190.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(
                        self.profile_path.is_none(),
                        format!("{} (embebido)", self.profile.name),
                    )
                    .clicked()
                {
                    self.load_embedded();
                }
                for path in self.available.clone() {
                    let nombre = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let activa = self.profile_path.as_ref() == Some(&path);
                    if ui.selectable_label(activa, nombre).clicked() {
                        self.load_profile(&path);
                    }
                }
            });

        if ui
            .add_enabled(self.profile_path.is_some(), egui::Button::new("Guardar"))
            .on_disabled_hover_text("Carga un perfil o usa «Guardar como» primero")
            .clicked()
            && let Some(path) = self.profile_path.clone()
        {
            self.save_profile(path);
        }
        ui.add(
            egui::TextEdit::singleline(&mut self.file_name)
                .desired_width(130.0)
                .hint_text("nombre.toml"),
        );
        if ui.button("Guardar como…").clicked() {
            let nombre = if self.file_name.ends_with(".toml") {
                self.file_name.clone()
            } else {
                format!("{}.toml", self.file_name.trim())
            };
            self.save_profile(profiles_dir().join(nombre));
        }
        if ui.button("Nuevo").clicked() {
            self.profile = Profile::default();
            self.profile.name = "perfil nuevo".into();
            self.profile_path = None;
            self.log = "perfil nuevo en blanco; guárdalo con «Guardar como…»".into();
        }
    }

    // -- pestañas -----------------------------------------------------------

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("pestanyas").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(self.tab == Tab::Monitor, "  Monitor en vivo  ")
                    .clicked()
                {
                    self.tab = Tab::Monitor;
                }
                if ui
                    .selectable_label(self.tab == Tab::Mapping, "  Remapeo y ajustes  ")
                    .clicked()
                {
                    self.tab = Tab::Mapping;
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("barra_estado").show(ui, |ui| {
            let mut descartar = false;
            if let Some(err) = self.error.as_deref() {
                ui.horizontal(|ui| {
                    ui.colored_label(ROJO, egui::RichText::new(format!("⚠ {err}")).strong());
                    descartar = ui.small_button("✕").clicked();
                });
            }
            if descartar {
                self.error = None;
            }
            ui.horizontal(|ui| {
                if !self.nodes.is_empty() {
                    ui.label(egui::RichText::new(&self.nodes).monospace().small());
                    ui.separator();
                }
                if let Some(stats) = self.stats {
                    ui.label(
                        egui::RichText::new(format!(
                            "eventos={} pad={} kbd={} ratón={} descartados={}",
                            stats.events,
                            stats.to_pad,
                            stats.to_keyboard,
                            stats.to_mouse,
                            stats.dropped
                        ))
                        .monospace()
                        .small(),
                    );
                    ui.separator();
                }
                ui.label(egui::RichText::new(&self.log).small().weak());
            });
        });
    }

    // -- pestaña 1: monitor -------------------------------------------------

    fn monitor_tab(&mut self, ui: &mut egui::Ui) {
        let Some(pad) = self.pad.clone() else {
            ui.add_space(24.0);
            ui.label(
                "Sin mando detectado. Conecta el GameSir Tegenaria Lite y espera un segundo; \
                 si tiene selector de modo (XInput / DInput / Switch), prueba otro modo.",
            );
            return;
        };

        ui.heading("Botones");
        ui.label(
            egui::RichText::new(
                "Pulsa cualquier botón del mando, incluidos los traseros: se enciende aquí \
                 con su código real (así se identifican BTN_C y BTN_Z).",
            )
            .weak(),
        );
        ui.add_space(6.0);
        let botones = pad.keys.clone();
        let estado = self.buttons.clone();
        egui::Grid::new("rejilla_botones")
            .num_columns(4)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                for grupo in botones.chunks(4) {
                    for code in grupo {
                        button_chip(ui, *code, estado.get(&code.0).copied().unwrap_or(0));
                    }
                    for _ in grupo.len()..4 {
                        ui.label("");
                    }
                    ui.end_row();
                }
            });

        ui.add_space(10.0);
        ui.heading("Sticks y gatillos");
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Izquierdo (ABS_X / ABS_Y)").strong());
                let (x, y) =
                    self.stick_values(&pad, AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y);
                bipolar_bar(ui, "X", x);
                bipolar_bar(ui, "Y", y);
                ui.label(
                    egui::RichText::new(format!(
                        "crudo: {} / {}",
                        raw(&self.axes, AbsoluteAxisCode::ABS_X),
                        raw(&self.axes, AbsoluteAxisCode::ABS_Y)
                    ))
                    .monospace()
                    .small()
                    .weak(),
                );
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Derecho (ABS_Z / ABS_RZ)").strong());
                let (x, y) =
                    self.stick_values(&pad, AbsoluteAxisCode::ABS_Z, AbsoluteAxisCode::ABS_RZ);
                bipolar_bar(ui, "X", x);
                bipolar_bar(ui, "Y", y);
                ui.label(
                    egui::RichText::new(format!(
                        "crudo: {} / {}",
                        raw(&self.axes, AbsoluteAxisCode::ABS_Z),
                        raw(&self.axes, AbsoluteAxisCode::ABS_RZ)
                    ))
                    .monospace()
                    .small()
                    .weak(),
                );
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Gatillos").strong());
                let izq = self
                    .trigger_value(&pad, AbsoluteAxisCode::ABS_BRAKE)
                    .unwrap_or(0.0);
                let der = self
                    .trigger_value(&pad, AbsoluteAxisCode::ABS_GAS)
                    .unwrap_or(0.0);
                unipolar_bar(ui, "L2", izq);
                unipolar_bar(ui, "R2", der);
                ui.label(
                    egui::RichText::new(format!(
                        "crudo: {} / {}",
                        raw(&self.axes, AbsoluteAxisCode::ABS_BRAKE),
                        raw(&self.axes, AbsoluteAxisCode::ABS_GAS)
                    ))
                    .monospace()
                    .small()
                    .weak(),
                );
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Cruceta").strong());
                ui.label(
                    egui::RichText::new(format!(
                        "HAT0X = {}   HAT0Y = {}",
                        raw(&self.axes, AbsoluteAxisCode::ABS_HAT0X),
                        raw(&self.axes, AbsoluteAxisCode::ABS_HAT0Y)
                    ))
                    .monospace(),
                );
            });
        });

        ui.add_space(10.0);
        ui.heading("Últimos botones");
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                ui.set_min_height(96.0);
                if self.event_log.is_empty() {
                    ui.label(egui::RichText::new("todavía no se ha pulsado nada").weak());
                }
                for line in self.event_log.iter() {
                    ui.label(egui::RichText::new(line).monospace().small());
                }
            });
    }

    fn stick_values(&self, pad: &PadInfo, x: AbsoluteAxisCode, y: AbsoluteAxisCode) -> (f32, f32) {
        (
            self.axis_norm(pad, x).unwrap_or(0.0),
            self.axis_norm(pad, y).unwrap_or(0.0),
        )
    }

    fn axis_norm(&self, pad: &PadInfo, code: AbsoluteAxisCode) -> Option<f32> {
        let value = *self.axes.get(&code.0)?;
        let info = pad.axis(code).map(|a| a.absinfo());
        Some(input::normalize_abs(value, info))
    }

    fn trigger_value(&self, pad: &PadInfo, code: AbsoluteAxisCode) -> Option<f32> {
        let value = *self.axes.get(&code.0)?;
        let info = pad.axis(code).map(|a| a.absinfo());
        Some(input::normalize_trigger(value, info))
    }

    // -- pestaña 2: remapeo y ajustes ---------------------------------------

    fn mapping_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Remapeo de botones");
        ui.label(
            egui::RichText::new(
                "«Automático» deja el comportamiento por defecto del puente: reenvío al \
                 gamepad virtual (BTN_TL2/BTN_TR2 se descartan porque viajan como eje).",
            )
            .weak(),
        );
        ui.add_space(6.0);

        let codigos = self.bindable_codes();
        egui::Grid::new("rejilla_binds")
            .num_columns(4)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Botón físico").strong());
                ui.label(egui::RichText::new("Acción").strong());
                ui.label(egui::RichText::new("Destino").strong());
                ui.label("");
                ui.end_row();
                for code in codigos {
                    self.bind_row(ui, code);
                }
            });

        ui.add_space(12.0);
        ui.heading("Sticks (zona muerta y sensibilidad)");
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Izquierdo").strong());
                ui.add(
                    egui::Slider::new(&mut self.profile.left_stick.deadzone, 0.0..=0.95)
                        .text("zona muerta"),
                );
                ui.add(
                    egui::Slider::new(&mut self.profile.left_stick.sensitivity, 0.1..=3.0)
                        .text("sensibilidad"),
                );
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Derecho").strong());
                ui.add(
                    egui::Slider::new(&mut self.profile.right_stick.deadzone, 0.0..=0.95)
                        .text("zona muerta"),
                );
                ui.add(
                    egui::Slider::new(&mut self.profile.right_stick.sensitivity, 0.1..=3.0)
                        .text("sensibilidad"),
                );
            });
        });

        ui.add_space(12.0);
        ui.heading("Gatillos L2 / R2");
        ui.checkbox(
            &mut self.profile.triggers.hair_trigger,
            "Hair trigger (umbral rápido)",
        );
        let hair = self.profile.triggers.hair_trigger;
        ui.add_enabled_ui(hair, |ui| {
            ui.add(
                egui::Slider::new(&mut self.profile.triggers.threshold, 0.01..=1.0).text("umbral"),
            );
        });

        ui.add_space(12.0);
        ui.heading("Stick a ratón");
        ui.checkbox(&mut self.profile.stick_to_mouse.enabled, "Activado");
        let activado = self.profile.stick_to_mouse.enabled;
        ui.add_enabled_ui(activado, |ui| {
            ui.horizontal(|ui| {
                ui.label("Stick:");
                ui.selectable_value(
                    &mut self.profile.stick_to_mouse.stick,
                    Stick::Left,
                    "Izquierdo",
                );
                ui.selectable_value(
                    &mut self.profile.stick_to_mouse.stick,
                    Stick::Right,
                    "Derecho",
                );
            });
            ui.add(
                egui::Slider::new(&mut self.profile.stick_to_mouse.speed, 50.0..=3000.0)
                    .text("velocidad (cuentas/s)"),
            );
            ui.checkbox(
                &mut self.profile.stick_to_mouse.suppress_axis,
                "No enviar ese stick al gamepad virtual mientras mueve el ratón",
            );
            let actual = self.profile.stick_to_mouse.hold;
            ui.horizontal(|ui| {
                ui.label("Botón de retención:");
                let elegido = hold_combo(ui, actual);
                if elegido != actual {
                    self.profile.stick_to_mouse.hold = elegido;
                }
            });
        });
    }

    /// Botones editables: los que declara el mando más los que ya tengan bind.
    fn bindable_codes(&self) -> Vec<KeyCode> {
        let mut codes: Vec<KeyCode> = match &self.pad {
            Some(pad) => pad
                .keys
                .iter()
                .copied()
                .filter(|k| (0x120..=0x13f).contains(&k.0))
                .collect(),
            None => PAD_TARGETS.to_vec(),
        };
        for bind in &self.profile.binds {
            if !codes.contains(&bind.from) {
                codes.push(bind.from);
            }
        }
        codes.sort_by_key(|c| c.0);
        codes.dedup();
        codes
    }

    fn bind_row(&mut self, ui: &mut egui::Ui, code: KeyCode) {
        let actual = self
            .profile
            .binds
            .iter()
            .find(|b| b.from == code)
            .map(|b| b.to);
        let kind = BindKind::of(actual);

        ui.label(egui::RichText::new(format!("{code:?}")).monospace());

        let mut nuevo = kind;
        egui::ComboBox::from_id_salt(("accion", code.0))
            .selected_text(kind.label())
            .width(140.0)
            .show_ui(ui, |ui| {
                for opcion in BindKind::ALL {
                    ui.selectable_value(&mut nuevo, opcion, opcion.label());
                }
            });

        match nuevo {
            BindKind::Auto => {
                ui.label(egui::RichText::new("reenvío por defecto").weak());
            }
            BindKind::Ignore => {
                ui.label(egui::RichText::new("se descarta").weak());
            }
            BindKind::Pad => {
                let destino = match actual {
                    Some(Action::Pad(c)) => c,
                    _ => KeyCode::BTN_SOUTH,
                };
                if let Some(c) = code_combo(ui, ("pad", code.0), &PAD_TARGETS, Some(destino)) {
                    self.set_bind(code, Some(Action::Pad(c)));
                }
            }
            BindKind::Key => {
                let destino = match actual {
                    Some(Action::Key(c)) => Some(c),
                    _ => None,
                };
                if let Some(c) = code_combo(ui, ("key", code.0), &KEY_TARGETS, destino) {
                    self.set_bind(code, Some(Action::Key(c)));
                }
            }
            BindKind::Mouse => {
                let destino = match actual {
                    Some(Action::Mouse(c)) => Some(c),
                    _ => None,
                };
                if let Some(c) = code_combo(ui, ("mouse", code.0), &MOUSE_TARGETS, destino) {
                    self.set_bind(code, Some(Action::Mouse(c)));
                }
            }
        }

        if kind != BindKind::Auto {
            if ui
                .small_button("✕")
                .on_hover_text("Quitar el bind")
                .clicked()
            {
                self.set_bind(code, None);
            }
        } else {
            ui.label("");
        }

        if nuevo != kind {
            self.set_bind(code, nuevo.default_action());
        }
        ui.end_row();
    }

    fn set_bind(&mut self, code: KeyCode, action: Option<Action>) {
        self.profile.binds.retain(|b| b.from != code);
        if let Some(action) = action {
            self.profile.binds.push(Bind {
                from: code,
                to: action,
            });
        }
        self.log = format!(
            "bind de {code:?} actualizado; se aplicará en caliente o al reiniciar el puente"
        );
    }

    // -- perfiles -----------------------------------------------------------

    fn load_embedded(&mut self) {
        self.profile = Profile::embedded_default();
        self.profile_path = None;
        self.log = "perfil embebido de prueba cargado".into();
    }

    fn load_profile(&mut self, path: &Path) {
        match Profile::load(path) {
            Ok(profile) => {
                self.profile = profile;
                self.profile_path = Some(path.to_path_buf());
                self.error = None;
                self.log = format!("perfil cargado: {}", path.display());
            }
            Err(e) => self.error = Some(format!("{}: {e}", path.display())),
        }
    }

    fn save_profile(&mut self, path: PathBuf) {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.error = Some(format!("no se pudo crear {}: {e}", parent.display()));
            return;
        }
        match self.profile.save(&path) {
            Ok(()) => {
                self.profile_path = Some(path.clone());
                self.available = list_profiles(&profiles_dir());
                self.error = None;
                self.log = format!("perfil guardado: {}", path.display());
            }
            Err(e) => self.error = Some(format!("no se pudo guardar {}: {e}", path.display())),
        }
    }
}

pub struct GuiApp {
    model: Arc<Mutex<GuiModel>>,
    cmds: CmdSender,
}

impl GuiApp {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        model: Arc<Mutex<GuiModel>>,
        cmds: CmdSender,
    ) -> Self {
        Self { model, cmds }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut m = lock(&self.model);
        m.paint(ui, &self.cmds);

        let ctx = ui.ctx().clone();
        let cerrado = ctx.input(|i| i.viewport().close_requested());
        if m.quit_now {
            m.quit_now = false;
            m.close_reason = Some(CloseReason::Quit);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if m.hide_now {
            m.hide_now = false;
            m.close_reason = Some(CloseReason::Hide);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cerrado {
            // En Wayland una ventana no se puede desmapear (el `set_visible`
            // de winit no hace nada): ocultar a la bandeja es cerrar la
            // ventana sin matar el proceso, y «Mostrar» la vuelve a abrir.
            m.close_reason = if m.tray_ok
                && m.hide_on_close
                && matches!(m.mode, WorkerMode::Bridge | WorkerMode::Reconnect)
            {
                Some(CloseReason::Hide)
            } else {
                Some(CloseReason::Quit)
            };
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

// ---------------------------------------------------------------------------
// Controlador: funde eventos, atiende la bandeja y gestiona la ventana
// ---------------------------------------------------------------------------

/// Órdenes del controlador al supervisor de ventana (hilo principal).
pub enum WindowCmd {
    Show,
    Quit,
}

struct Controller {
    model: Arc<Mutex<GuiModel>>,
    cmds: CmdSender,
    ev_rx: Receiver<Event>,
    stop: Arc<AtomicBool>,
    tray: Option<ksni::blocking::Handle<tray::GamesirTray>>,
    tray_view: Arc<Mutex<TrayView>>,
    tray_rx: Receiver<TrayCmd>,
    win_tx: Sender<WindowCmd>,
    snap: Option<TrayView>,
    quit: bool,
}

impl Controller {
    fn bucle(&mut self) {
        while !self.quit && !self.stop.load(Ordering::Relaxed) {
            if let Ok(ev) = self.ev_rx.recv_timeout(Duration::from_millis(100)) {
                self.merge(ev);
            }
            while let Ok(ev) = self.ev_rx.try_recv() {
                self.merge(ev);
            }
            while let Ok(cmd) = self.tray_rx.try_recv() {
                self.tray_cmd(cmd);
            }
            if !self.quit {
                self.push_tray();
            }
        }
        if let Some(tray) = &self.tray {
            tray.shutdown().wait();
        }
    }

    /// Funde un evento del hilo de entrada en el modelo compartido.
    fn merge(&mut self, ev: Event) {
        let mut m = lock(&self.model);
        match ev {
            Event::Mode(mode) => m.mode = mode,
            Event::Pad(pad) => {
                m.pad = pad;
                m.buttons.clear();
                m.axes.clear();
            }
            Event::Nodes(nodes) => m.nodes = nodes,
            Event::Log(msg) => m.log = msg,
            Event::Error(msg) => {
                m.log = msg.clone();
                m.error = Some(msg);
            }
            Event::Button(code, value) => {
                m.buttons.insert(code, value);
                let nombre = format!("{:?}", KeyCode::new(code));
                let estado = match value {
                    0 => "soltado",
                    1 => "PULSADO",
                    _ => "repetición",
                };
                m.push_event(format!("{nombre:<12} {estado}"));
            }
            Event::Axis(code, value) => {
                m.axes.insert(code, value);
            }
            Event::Stats(stats) => m.stats = Some(stats),
        }
    }

    fn tray_cmd(&mut self, cmd: TrayCmd) {
        match cmd {
            TrayCmd::ToggleWindow => {
                let abierta = lock(&self.model).window_open;
                if abierta {
                    lock(&self.model).hide_now = true;
                } else {
                    let _ = self.win_tx.send(WindowCmd::Show);
                }
            }
            TrayCmd::SetBridge(true) => {
                let perfil = lock(&self.model).profile.clone();
                self.cmds.send(Command::UpdateProfile(perfil));
                self.cmds.send(Command::StartBridge);
            }
            TrayCmd::SetBridge(false) => self.cmds.send(Command::StopBridge),
            TrayCmd::LoadProfile(path) => self.load_profile(path),
            TrayCmd::Quit => {
                lock(&self.model).quit_now = true;
                let _ = self.win_tx.send(WindowCmd::Quit);
                self.quit = true;
            }
        }
    }

    fn load_profile(&mut self, path: Option<PathBuf>) {
        let mut m = lock(&self.model);
        match &path {
            None => {
                m.profile = Profile::embedded_default();
                m.profile_path = None;
                m.log = "perfil embebido de prueba cargado".into();
            }
            Some(ruta) => match Profile::load(ruta) {
                Ok(perfil) => {
                    m.profile = perfil;
                    m.profile_path = Some(ruta.clone());
                    m.error = None;
                    m.log = format!("perfil cargado: {}", ruta.display());
                }
                Err(e) => {
                    m.error = Some(format!("{}: {e}", ruta.display()));
                    return;
                }
            },
        }
        m.available = list_profiles(&profiles_dir());
        m.sent = m.profile.clone();
        self.cmds.send(Command::UpdateProfile(m.profile.clone()));
    }

    /// Vuelca el estado relevante a la bandeja y emite las señales de cambio
    /// solo si algo difiere del último volcado.
    fn push_tray(&mut self) {
        let vista = {
            let m = lock(&self.model);
            TrayView {
                mode: m.mode,
                pad: m.pad.as_ref().map(|p| p.short()),
                window_open: m.window_open,
                profiles: perfiles_menu(&m),
                active: m.profile_path.clone(),
            }
        };
        if self.snap.as_ref() == Some(&vista) {
            return;
        }
        *lock(&self.tray_view) = vista.clone();
        if let Some(tray) = &self.tray {
            tray.update(|_| {});
        }
        self.snap = Some(vista);
    }
}

fn perfiles_menu(m: &GuiModel) -> Vec<(String, Option<PathBuf>)> {
    let mut lista = vec![(embedded_label(), None)];
    for path in &m.available {
        let nombre = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        lista.push((nombre, Some(path.clone())));
    }
    lista
}

fn embedded_label() -> String {
    static NOMBRE: OnceLock<String> = OnceLock::new();
    NOMBRE
        .get_or_init(|| format!("{} (embebido)", Profile::embedded_default().name))
        .clone()
}

/// Arranca el hilo de entrada, la bandeja y el controlador, y supervisa la
/// ventana desde el hilo principal. Bloquea hasta que se pide salir.
pub fn run(profile: Profile, path: Option<PathBuf>, daemon: bool) -> Result<(), String> {
    let (cmd_tx, cmd_rx) = channel();
    let wake = Arc::new(WakeFd::new().map_err(|e| format!("eventfd: {e}"))?);
    let cmds = CmdSender {
        tx: cmd_tx,
        wake: wake.clone(),
    };
    let (ev_tx, ev_rx) = channel();
    let model = Arc::new(Mutex::new(GuiModel::new(profile.clone(), path)));
    // El hilo arranca con el perfil inicial antes de su primer bucle.
    cmds.send(Command::UpdateProfile(profile));

    let worker = thread::Builder::new()
        .name("gamesir-input".into())
        .spawn(move || match Worker::new(cmd_rx, wake, ev_tx) {
            Ok(mut worker) => worker.run(),
            Err(e) => eprintln!("[!] hilo de entrada: {e}"),
        })
        .expect("no se pudo crear el hilo de entrada");

    let (tray_tx, tray_rx) = channel();
    let tray_view = Arc::new(Mutex::new(TrayView::default()));
    let tray = match tray::iniciar(tray_view.clone(), tray_tx) {
        Ok(handle) => Some(handle),
        Err(e) => {
            eprintln!("[!] {e}; seguirá funcionando sin bandeja");
            None
        }
    };
    lock(&model).tray_ok = tray.is_some();

    let (win_tx, win_rx) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let mut ctl = Controller {
        model: model.clone(),
        cmds: cmds.clone(),
        ev_rx,
        stop: stop.clone(),
        tray,
        tray_view,
        tray_rx,
        win_tx,
        snap: None,
        quit: false,
    };
    let controller = thread::Builder::new()
        .name("gamesir-control".into())
        .spawn(move || ctl.bucle())
        .expect("no se pudo crear el hilo controlador");

    if daemon {
        // Segundo plano: puente activo desde el principio y sin ventana.
        cmds.send(Command::StartBridge);
    }

    // Supervisor de ventana: winit solo permite el bucle de eventos en el
    // hilo principal, y en Wayland una ventana no se puede desmapear, así
    // que ocultar/mostrar es cerrar y volver a abrir (`run_native` otra vez).
    let mut abierta = !daemon;
    loop {
        if abierta {
            lock(&model).window_open = true;
            let options = eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([1000.0, 700.0])
                    .with_min_inner_size([820.0, 560.0]),
                ..Default::default()
            };
            let app_model = model.clone();
            let app_cmds = cmds.clone();
            let _ = eframe::run_native(
                "gamesir-rs — Tegenaria Lite",
                options,
                Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, app_model, app_cmds)))),
            );
            let mut m = lock(&model);
            m.window_open = false;
            if m.close_reason.take() != Some(CloseReason::Hide) {
                break;
            }
        }
        // Oculta (o arrancada en --daemon): espera la próxima orden.
        match win_rx.recv() {
            Ok(WindowCmd::Show) => {
                let mut m = lock(&model);
                m.close_reason = None;
                m.hide_now = false;
                m.quit_now = false;
            }
            Ok(WindowCmd::Quit) | Err(_) => break,
        }
        abierta = true;
    }

    stop.store(true, Ordering::Relaxed);
    let _ = controller.join();
    cmds.send(Command::Shutdown);
    let _ = worker.join();
    Ok(())
}

// ---------------------------------------------------------------------------
// Widgets y utilidades
// ---------------------------------------------------------------------------

fn button_chip(ui: &mut egui::Ui, code: KeyCode, value: i32) {
    let pulsado = value != 0;
    let fondo = if pulsado { VERDE } else { GRIS_CHIP };
    let texto = if pulsado {
        egui::Color32::WHITE
    } else {
        egui::Color32::from_gray(170)
    };
    egui::Frame::default()
        .fill(fondo)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::same(5))
        .show(ui, |ui| {
            ui.set_min_width(104.0);
            ui.label(
                egui::RichText::new(format!("{code:?}"))
                    .monospace()
                    .color(texto),
            );
            if value == 2 {
                ui.label(egui::RichText::new("repetición").small().weak());
            }
        });
}

fn bipolar_bar(ui: &mut egui::Ui, etiqueta: &str, value: f32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(etiqueta).monospace().weak());
        ui.add(
            egui::ProgressBar::new(((value + 1.0) / 2.0).clamp(0.0, 1.0))
                .desired_width(150.0)
                .text(format!("{value:+.3}")),
        );
    });
}

fn unipolar_bar(ui: &mut egui::Ui, etiqueta: &str, value: f32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(etiqueta).monospace().weak());
        ui.add(
            egui::ProgressBar::new(value.clamp(0.0, 1.0))
                .desired_width(150.0)
                .fill(AMBAR)
                .text(format!("{value:.3}")),
        );
    });
}

fn raw(axes: &BTreeMap<u16, i32>, code: AbsoluteAxisCode) -> String {
    match axes.get(&code.0) {
        Some(v) => v.to_string(),
        None => "—".into(),
    }
}

/// Combo de selección de código. `actual = None` muestra «(ninguno)».
fn code_combo(
    ui: &mut egui::Ui,
    id: impl egui::AsIdSalt,
    opciones: &[KeyCode],
    actual: Option<KeyCode>,
) -> Option<KeyCode> {
    let mut elegido = None;
    let texto = match actual {
        Some(code) => format!("{code:?}"),
        None => "(elige…)".into(),
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(texto)
        .width(140.0)
        .show_ui(ui, |ui| {
            for code in opciones {
                if ui
                    .selectable_label(Some(*code) == actual, format!("{code:?}"))
                    .clicked()
                {
                    elegido = Some(*code);
                }
            }
        });
    elegido
}

/// Como `code_combo` pero con una entrada extra para «sin botón».
fn hold_combo(ui: &mut egui::Ui, actual: Option<KeyCode>) -> Option<KeyCode> {
    let mut elegido = None;
    let mut cambiar = false;
    let texto = match actual {
        Some(code) => format!("{code:?}"),
        None => "siempre activo".into(),
    };
    egui::ComboBox::from_id_salt("boton_retencion")
        .selected_text(texto)
        .width(150.0)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(actual.is_none(), "siempre activo")
                .clicked()
            {
                cambiar = true;
            }
            for code in PAD_TARGETS {
                if ui
                    .selectable_label(Some(code) == actual, format!("{code:?}"))
                    .clicked()
                {
                    elegido = Some(code);
                    cambiar = true;
                }
            }
        });
    if cambiar { elegido } else { actual }
}

fn profiles_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("gamesir-rs").join("profiles")
}

fn list_profiles(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort();
    paths
}
