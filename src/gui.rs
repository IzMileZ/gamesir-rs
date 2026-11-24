//! Interfaz gráfica (eframe/egui) y el hilo de trabajo que habla con el mando.
//!
//! La GUI nunca toca el hardware: un hilo aparte abre el mando, lo lee y, si el
//! puente está activo, lo traduce a los dispositivos virtuales. Se comunican con
//! canales `std::sync::mpsc` (comandos hacia el hilo, eventos hacia la GUI) y el
//! hilo pide repintados con `egui::Context::request_repaint`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;
use evdev::{AbsoluteAxisCode, EventType, InputEvent, KeyCode};

use crate::input::{self, Bridge, Fetch, Outcome, PadInfo, PhysicalPad, Stats};
use crate::profile::{Action, Bind, Profile, Stick};

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
const POLL_INTERVAL: Duration = Duration::from_millis(1);
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
}

impl WorkerMode {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "inactivo",
            Self::Monitor => "monitorizando (sin capturar)",
            Self::Bridge => "puente activo (mando capturado)",
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

struct Worker {
    ctx: egui::Context,
    cmds: Receiver<Command>,
    out: Sender<Event>,
    state: State,
    profile: Profile,
    signature: (Vec<(u16, Action)>, bool),
    pending: Option<(Profile, Instant)>,
    buf: Vec<InputEvent>,
    last_detect: Instant,
    last_stats: Instant,
    stats: Stats,
}

impl Worker {
    fn new(ctx: egui::Context, cmds: Receiver<Command>, out: Sender<Event>) -> Self {
        Self {
            ctx,
            cmds,
            out,
            state: State::Idle,
            profile: Profile::embedded_default(),
            signature: Profile::default().caps_signature(),
            pending: None,
            buf: Vec::with_capacity(64),
            last_detect: Instant::now(),
            last_stats: Instant::now(),
            stats: Stats::default(),
        }
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

        let mut state = std::mem::take(&mut self.state);
        let mut buf = std::mem::take(&mut self.buf);
        let mut disconnected = false;
        let mut fatal: Option<String> = None;
        let mut want_monitor = false;

        match &mut state {
            State::Idle => {
                if self.last_detect.elapsed() >= DETECT_INTERVAL {
                    self.last_detect = Instant::now();
                    if input::detect_pad_info().is_some() {
                        want_monitor = true;
                    } else {
                        thread::sleep(Duration::from_millis(20));
                    }
                } else {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            State::Monitor(pad) => match pad.fetch(&mut buf) {
                Ok(Fetch::Events) => {
                    for ev in &buf {
                        self.report(ev);
                    }
                    self.ctx.request_repaint();
                }
                Ok(Fetch::Idle) => thread::sleep(POLL_INTERVAL),
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
                        self.ctx.request_repaint();
                    }
                    Ok(Fetch::Idle) => thread::sleep(POLL_INTERVAL),
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
        } else if want_monitor {
            self.start_monitor();
        }
        self.apply_pending();
        true
    }

    fn handle_commands(&mut self) -> bool {
        loop {
            match self.cmds.try_recv() {
                Ok(Command::Shutdown) => return false,
                Ok(Command::StartBridge) => self.start_bridge(),
                Ok(Command::StopBridge) => self.stop_bridge(),
                Ok(Command::UpdateProfile(profile)) => self.update_profile(profile),
                Err(TryRecvError::Empty) => return true,
                // La GUI ha desaparecido: no tiene sentido seguir.
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn emit(&self, ev: Event) {
        let _ = self.out.send(ev);
    }

    /// Reenvía a la GUI lo que llega del mando físico (antes de traducir).
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
            self.emit(Event::Mode(WorkerMode::Idle));
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
                self.start_monitor();
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

    fn on_disconnected(&mut self) {
        self.emit(Event::Log("el mando físico se desconectó".into()));
        self.state = State::Idle;
        self.last_detect = Instant::now();
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
// Aplicación egui
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Monitor,
    Mapping,
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

pub struct GuiApp {
    cmds: Sender<Command>,
    rx: Receiver<Event>,
    worker: Option<JoinHandle<()>>,
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
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>, profile: Profile, path: Option<PathBuf>) -> Self {
        let (cmds_tx, cmds_rx) = channel();
        let (ev_tx, ev_rx) = channel();
        // El hilo arranca con el perfil que se muestra en la ventana, no con el
        // embebido: se envía antes de lanzarlo para que lo vea en su primer bucle.
        let _ = cmds_tx.send(Command::UpdateProfile(profile.clone()));
        let ctx = cc.egui_ctx.clone();
        let worker = thread::Builder::new()
            .name("gamesir-input".into())
            .spawn(move || Worker::new(ctx, cmds_rx, ev_tx).run())
            .expect("no se pudo crear el hilo de entrada");

        let file_name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("mi_perfil.toml"));

        Self {
            cmds: cmds_tx,
            rx: ev_rx,
            worker: Some(worker),
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
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.cmds.send(cmd);
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Mode(mode) => self.mode = mode,
                Event::Pad(pad) => {
                    self.pad = pad;
                    self.buttons.clear();
                    self.axes.clear();
                }
                Event::Nodes(nodes) => self.nodes = nodes,
                Event::Log(msg) => self.log = msg,
                Event::Error(msg) => {
                    self.log = msg.clone();
                    self.error = Some(msg);
                }
                Event::Button(code, value) => {
                    self.buttons.insert(code, value);
                    let nombre = format!("{:?}", KeyCode::new(code));
                    let estado = match value {
                        0 => "soltado",
                        1 => "PULSADO",
                        _ => "repetición",
                    };
                    self.push_event(format!("{nombre:<12} {estado}"));
                }
                Event::Axis(code, value) => {
                    self.axes.insert(code, value);
                }
                Event::Stats(stats) => self.stats = Some(stats),
            }
        }
    }

    fn push_event(&mut self, line: String) {
        self.event_log.push_front(line);
        while self.event_log.len() > EVENT_LOG_LEN {
            self.event_log.pop_back();
        }
    }

    fn push_profile(&mut self) {
        self.sent = self.profile.clone();
        self.send(Command::UpdateProfile(self.profile.clone()));
    }

    // -- barra superior -----------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
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
                self.bridge_button(ui);
            });
            ui.add_space(6.0);
        });
    }

    fn bridge_button(&mut self, ui: &mut egui::Ui) {
        let activo = self.mode == WorkerMode::Bridge;
        let texto = if activo {
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
        .fill(if activo { ROJO } else { VERDE })
        .min_size(egui::vec2(210.0, 30.0));
        let respuesta = ui
            .add_enabled(activo || self.pad.is_some(), boton)
            .on_disabled_hover_text("Conecta el mando para poder activar el puente");
        if respuesta.clicked() {
            if activo {
                self.send(Command::StopBridge);
            } else {
                self.push_profile();
                self.send(Command::StartBridge);
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

impl Drop for GuiApp {
    fn drop(&mut self) {
        self.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain();
        self.top_bar(ui);
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
            self.push_profile();
        }
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
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

/// Abre la ventana principal. Bloquea hasta que se cierra.
pub fn run(profile: Profile, path: Option<PathBuf>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "gamesir-rs — Tegenaria Lite",
        options,
        Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, profile, path)))),
    )
    .map_err(|e| format!("no se pudo abrir la ventana: {e}"))
}
