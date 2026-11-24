//! Motor de entrada: detección del mando, `grab()` exclusivo, dispositivos
//! virtuales uinput (gamepad + teclado + ratón) y traducción de eventos.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, Device, EventType, InputEvent, InputId,
    KeyCode, RelativeAxisCode, SynchronizationCode, UinputAbsSetup,
};

use crate::profile::{Action, Profile, Stick, StickConfig, TriggerConfig};

pub const VENDOR_GAMESIR: u16 = 0x3537;
pub const PRODUCT_TEGENARIA_LITE: u16 = 0x1093;
/// El Tegenaria Lite expone tres nodos con el mismo nombre (gamepad, teclado y
/// ratón); estos textos sirven para reconocer la familia.
pub const NAME_HINTS: [&str; 2] = ["tegenaria", "gamesir"];

pub const PAD_NAME: &str = "Tegenaria Remapped (pad)";
pub const KEYBOARD_NAME: &str = "Tegenaria Remapped (kbd)";
pub const MOUSE_NAME: &str = "Tegenaria Remapped (mouse)";
pub const VIRTUAL_VENDOR: u16 = VENDOR_GAMESIR;
pub const VIRTUAL_PRODUCT: u16 = 0x1094;

/// `ENODEV`: el kernel lo devuelve al leer un dispositivo USB desconectado.
const ENODEV: i32 = 19;
/// Intervalo de sondeo en modo no bloqueante (sin dependencias de epoll/poll).
const POLL_INTERVAL: Duration = Duration::from_millis(1);
/// Cadencia del modo stick-a-ratón (~125 Hz).
const MOUSE_TICK: Duration = Duration::from_millis(8);

const BASE_PAD_KEYS: [KeyCode; 11] = [
    KeyCode::BTN_SOUTH,
    KeyCode::BTN_EAST,
    KeyCode::BTN_NORTH,
    KeyCode::BTN_WEST,
    KeyCode::BTN_TL,
    KeyCode::BTN_TR,
    KeyCode::BTN_SELECT,
    KeyCode::BTN_START,
    KeyCode::BTN_MODE,
    KeyCode::BTN_THUMBL,
    KeyCode::BTN_THUMBR,
];

/// Traducción del layout DInput del Tegenaria Lite a un gamepad virtual
/// Xbox-like. Los binds del perfil tienen prioridad sobre esta tabla.
///
/// En los botones no hay nada que traducir: el físico ya usa la convención
/// `BTN_SOUTH`/`BTN_EAST`/... (lo que `evdev` de Python llamaba `BTN_A`/`BTN_B`
/// son alias del mismo código), así que basta con dejarlos pasar.
fn passthrough_key(code: KeyCode) -> Option<Action> {
    // BTN_TL2/BTN_TR2 son la versión digital de los gatillos; en el gamepad
    // virtual viajan solo como eje analógico (ABS_Z / ABS_RZ).
    if code == KeyCode::BTN_TL2 || code == KeyCode::BTN_TR2 {
        return None;
    }
    (0x130..=0x13f)
        .contains(&code.0)
        .then_some(Action::Pad(code))
}

/// Papel de cada eje físico del mando.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AxisRole {
    LeftX,
    LeftY,
    RightX,
    RightY,
    LeftTrigger,
    RightTrigger,
    HatX,
    HatY,
}

fn axis_role(code: AbsoluteAxisCode) -> Option<AxisRole> {
    Some(match code {
        AbsoluteAxisCode::ABS_X => AxisRole::LeftX,
        AbsoluteAxisCode::ABS_Y => AxisRole::LeftY,
        // DInput: el stick derecho viaja en Z/RZ, los gatillos en BRAKE/GAS.
        AbsoluteAxisCode::ABS_Z => AxisRole::RightX,
        AbsoluteAxisCode::ABS_RZ => AxisRole::RightY,
        AbsoluteAxisCode::ABS_BRAKE => AxisRole::LeftTrigger,
        AbsoluteAxisCode::ABS_GAS => AxisRole::RightTrigger,
        AbsoluteAxisCode::ABS_HAT0X => AxisRole::HatX,
        AbsoluteAxisCode::ABS_HAT0Y => AxisRole::HatY,
        _ => return None,
    })
}

fn virtual_axis(role: AxisRole) -> Option<AbsoluteAxisCode> {
    Some(match role {
        AxisRole::LeftX => AbsoluteAxisCode::ABS_X,
        AxisRole::LeftY => AbsoluteAxisCode::ABS_Y,
        AxisRole::RightX => AbsoluteAxisCode::ABS_RX,
        AxisRole::RightY => AbsoluteAxisCode::ABS_RY,
        AxisRole::LeftTrigger => AbsoluteAxisCode::ABS_Z,
        AxisRole::RightTrigger => AbsoluteAxisCode::ABS_RZ,
        AxisRole::HatX => AbsoluteAxisCode::ABS_HAT0X,
        AxisRole::HatY => AbsoluteAxisCode::ABS_HAT0Y,
    })
}

// ---------------------------------------------------------------------------
// Matemática de ejes (pura, cubre deadzone / sensibilidad / hair trigger)
// ---------------------------------------------------------------------------

/// Normaliza un eje centrado (sticks, hat) a `[-1.0, 1.0]`.
pub fn normalize_abs(value: i32, info: Option<AbsInfo>) -> f32 {
    match info {
        Some(ai) if ai.maximum() > ai.minimum() => {
            let min = ai.minimum() as f32;
            let max = ai.maximum() as f32;
            let mid = (max + min) / 2.0;
            ((value as f32 - mid) / ((max - min) / 2.0)).clamp(-1.0, 1.0)
        }
        // Rango degenerado: el eje no informa de nada útil.
        Some(_) => 0.0,
        // Sin absinfo se asume el rango de 8 bits del Tegenaria Lite.
        None => ((value as f32 - 127.5) / 127.5).clamp(-1.0, 1.0),
    }
}

/// Normaliza un gatillo a `[0.0, 1.0]`.
pub fn normalize_trigger(value: i32, info: Option<AbsInfo>) -> f32 {
    match info {
        Some(ai) if ai.maximum() > ai.minimum() => {
            let min = ai.minimum() as f32;
            let max = ai.maximum() as f32;
            ((value as f32 - min) / (max - min)).clamp(0.0, 1.0)
        }
        Some(_) => 0.0,
        None => (value as f32 / 255.0).clamp(0.0, 1.0),
    }
}

/// Aplica zona muerta radial y sensibilidad conservando la dirección.
pub fn shape_stick(x: f32, y: f32, cfg: StickConfig) -> (f32, f32) {
    let mag = (x * x + y * y).sqrt();
    if !mag.is_finite() || mag <= cfg.deadzone {
        return (0.0, 0.0);
    }
    let scaled = ((mag - cfg.deadzone) / (1.0 - cfg.deadzone)) * cfg.sensitivity;
    let factor = scaled.min(1.0) / mag;
    (x * factor, y * factor)
}

/// Con `hair_trigger`, el recorrido útil empieza en `threshold`; el valor
/// devuelto sigue en `[0.0, 1.0]`.
pub fn shape_trigger(value: f32, cfg: TriggerConfig) -> f32 {
    if !cfg.hair_trigger {
        return value.clamp(0.0, 1.0);
    }
    ((value - cfg.threshold) / (1.0 - cfg.threshold)).clamp(0.0, 1.0)
}

/// Escala a la resolución de 16 bits del gamepad virtual (simétrico: ±32767).
pub fn to_stick_i32(value: f32) -> i32 {
    (value.clamp(-1.0, 1.0) * 32767.0).round() as i32
}

/// Escala al rango 0..255 de los gatillos virtuales.
pub fn to_trigger_i32(value: f32) -> i32 {
    (value.clamp(0.0, 1.0) * 255.0).round() as i32
}

// ---------------------------------------------------------------------------
// Detección
// ---------------------------------------------------------------------------

/// Mando físico detectado, listo para abrir el puente.
pub struct PadCandidate {
    pub path: PathBuf,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub buttons: usize,
    pub axes: Vec<(AbsoluteAxisCode, AbsInfo)>,
    device: Device,
}

impl PadCandidate {
    /// Puntuación para elegir entre varios candidatos: VID/PID exacto primero,
    /// luego coincidencia de nombre y por último capacidades completas.
    fn score(&self) -> u8 {
        let mut s = 0;
        if self.vendor == VENDOR_GAMESIR && self.product == PRODUCT_TEGENARIA_LITE {
            s += 4;
        }
        let name = self.name.to_lowercase();
        if NAME_HINTS.iter().any(|hint| name.contains(hint)) {
            s += 2;
        }
        if self.buttons > 0 && !self.axes.is_empty() {
            s += 1;
        }
        s
    }
}

fn has_pad_caps(dev: &Device) -> bool {
    let pad_keys = dev
        .supported_keys()
        .is_some_and(|keys| keys.iter().any(|k| (0x120..=0x13f).contains(&k.0)));
    let sticks = dev.supported_absolute_axes().is_some_and(|axes| {
        axes.contains(AbsoluteAxisCode::ABS_X) && axes.contains(AbsoluteAxisCode::ABS_Y)
    });
    pad_keys && sticks
}

/// Lista los gamepads conectados. Abre cada nodo de `/dev/input`; los que no
/// pueda abrir (permisos) se ignoran en silencio, como hace `evdev::enumerate`.
pub fn find_pads() -> Vec<PadCandidate> {
    let mut pads = Vec::new();
    for (path, device) in evdev::enumerate() {
        if !has_pad_caps(&device) {
            continue;
        }
        let id = device.input_id();
        let name = device.name().unwrap_or("desconocido").to_string();
        let buttons = device
            .supported_keys()
            .map(|keys| keys.iter().count())
            .unwrap_or(0);
        let axes = device
            .get_absinfo()
            .map(|it| it.collect())
            .unwrap_or_default();
        pads.push(PadCandidate {
            path,
            name,
            vendor: id.vendor(),
            product: id.product(),
            buttons,
            axes,
            device,
        });
    }
    pads
}

/// Mejor candidato entre los detectados.
pub fn pick_pad(pads: Vec<PadCandidate>) -> Option<PadCandidate> {
    pads.into_iter().max_by_key(|p| p.score())
}

/// Abre directamente un nodo `/dev/input/eventXX`.
pub fn open_pad(path: &Path) -> io::Result<PadCandidate> {
    let device = Device::open(path)?;
    let id = device.input_id();
    let name = device.name().unwrap_or("desconocido").to_string();
    let buttons = device
        .supported_keys()
        .map(|keys| keys.iter().count())
        .unwrap_or(0);
    let axes = device.get_absinfo()?.collect();
    Ok(PadCandidate {
        path: path.to_path_buf(),
        name,
        vendor: id.vendor(),
        product: id.product(),
        buttons,
        axes,
        device,
    })
}

/// Descripción multilínea de un mando para `--list`.
pub fn describe(pad: &PadCandidate) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  Ruta    : {}\n  Nombre  : {}\n  VID:PID : {:04x}:{:04x}\n  Botones : {}\n  Ejes    :\n",
        pad.path.display(),
        pad.name,
        pad.vendor,
        pad.product,
        pad.buttons
    ));
    for (code, ai) in &pad.axes {
        out.push_str(&format!(
            "      {code:<12?} min={:<7} max={:<7} fuzz={:<4} flat={}\n",
            ai.minimum(),
            ai.maximum(),
            ai.fuzz(),
            ai.flat()
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Mando físico
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Fetch {
    /// No había eventos pendientes.
    Idle,
    /// `out` contiene un lote de eventos.
    Events,
    /// El mando se desconectó.
    Disconnected,
}

pub struct PhysicalPad {
    device: Device,
    absinfo: HashMap<u16, AbsInfo>,
    pub path: PathBuf,
    pub name: String,
    grabbed: bool,
}

impl PhysicalPad {
    pub fn new(candidate: PadCandidate) -> io::Result<Self> {
        let PadCandidate {
            path,
            name,
            axes,
            device,
            ..
        } = candidate;
        let absinfo = axes.into_iter().map(|(c, ai)| (c.0, ai)).collect();
        let pad = Self {
            device,
            absinfo,
            path,
            name,
            grabbed: false,
        };
        // Sin dependencias de poll/epoll: se lee en no bloqueante y se duerme
        // 1 ms cuando no hay eventos.
        pad.device.set_nonblocking(true)?;
        Ok(pad)
    }

    /// Uso exclusivo del nodo: los juegos dejan de ver el mando original.
    pub fn grab(&mut self) -> io::Result<()> {
        self.device.grab()?;
        self.grabbed = true;
        Ok(())
    }

    pub fn ungrab(&mut self) {
        if self.grabbed {
            if let Err(e) = self.device.ungrab() {
                eprintln!("[!] no se pudo liberar el mando: {e}");
            }
            self.grabbed = false;
        }
    }

    pub fn is_grabbed(&self) -> bool {
        self.grabbed
    }

    /// Rellena `out` con los eventos disponibles (lo vacía antes).
    pub fn fetch(&mut self, out: &mut Vec<InputEvent>) -> io::Result<Fetch> {
        out.clear();
        match self.device.fetch_events() {
            Ok(events) => {
                out.extend(events);
                Ok(if out.is_empty() {
                    Fetch::Idle
                } else {
                    Fetch::Events
                })
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(Fetch::Idle),
            Err(e) if e.raw_os_error() == Some(ENODEV) => Ok(Fetch::Disconnected),
            Err(e) => Err(e),
        }
    }
}

impl Drop for PhysicalPad {
    fn drop(&mut self) {
        self.ungrab();
    }
}

// ---------------------------------------------------------------------------
// Dispositivos virtuales uinput
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Nodes {
    pub pad: Option<PathBuf>,
    pub keyboard: Option<PathBuf>,
    pub mouse: Option<PathBuf>,
}

impl std::fmt::Display for Nodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pad: ")?;
        match &self.pad {
            Some(p) => write!(f, "{}", p.display())?,
            None => write!(f, "-")?,
        }
        write!(f, "  kbd: ")?;
        match &self.keyboard {
            Some(p) => write!(f, "{}", p.display())?,
            None => write!(f, "(no creado)")?,
        }
        write!(f, "  mouse: ")?;
        match &self.mouse {
            Some(p) => write!(f, "{}", p.display())?,
            None => write!(f, "(no creado)")?,
        }
        Ok(())
    }
}

fn stick_setup(code: AbsoluteAxisCode) -> UinputAbsSetup {
    UinputAbsSetup::new(code, AbsInfo::new(0, -32768, 32767, 16, 128, 0))
}

fn trigger_setup(code: AbsoluteAxisCode) -> UinputAbsSetup {
    UinputAbsSetup::new(code, AbsInfo::new(0, 0, 255, 0, 0, 0))
}

fn hat_setup(code: AbsoluteAxisCode) -> UinputAbsSetup {
    UinputAbsSetup::new(code, AbsInfo::new(0, -1, 1, 0, 0, 0))
}

fn build_virtual(
    name: &'static str,
    product: u16,
    keys: &AttributeSet<KeyCode>,
    abs: &[UinputAbsSetup],
    rel: &AttributeSet<RelativeAxisCode>,
) -> io::Result<(VirtualDevice, Option<PathBuf>)> {
    let mut builder = VirtualDevice::builder()?
        .name(name)
        .input_id(InputId::new(
            BusType::BUS_USB,
            VIRTUAL_VENDOR,
            product,
            0x0100,
        ))
        .with_keys(keys)?;
    for setup in abs {
        builder = builder.with_absolute_axis(setup)?;
    }
    if rel.iter().next().is_some() {
        builder = builder.with_relative_axes(rel)?;
    }
    let mut device = builder.build()?;
    let node = device.enumerate_dev_nodes_blocking()?.next().transpose()?;
    Ok((device, node))
}

#[derive(Default)]
struct Pending {
    pad: bool,
    keyboard: bool,
    mouse: bool,
}

/// Gamepad virtual (siempre) más teclado y ratón virtuales, creados solo si el
/// perfil los necesita.
pub struct VirtualOutputs {
    pad: VirtualDevice,
    keyboard: Option<VirtualDevice>,
    mouse: Option<VirtualDevice>,
    pending: Pending,
    pub nodes: Nodes,
}

impl VirtualOutputs {
    pub fn new(profile: &Profile) -> io::Result<Self> {
        let mut pad_keys = AttributeSet::<KeyCode>::new();
        for key in BASE_PAD_KEYS {
            pad_keys.insert(key);
        }
        let mut kb_keys = AttributeSet::<KeyCode>::new();
        let mut mouse_keys = AttributeSet::<KeyCode>::new();
        for bind in &profile.binds {
            match bind.to {
                Action::Pad(code) => pad_keys.insert(code),
                Action::Key(code) => kb_keys.insert(code),
                Action::Mouse(code) => mouse_keys.insert(code),
                Action::Ignore => {}
            }
        }

        let abs = [
            stick_setup(AbsoluteAxisCode::ABS_X),
            stick_setup(AbsoluteAxisCode::ABS_Y),
            stick_setup(AbsoluteAxisCode::ABS_RX),
            stick_setup(AbsoluteAxisCode::ABS_RY),
            trigger_setup(AbsoluteAxisCode::ABS_Z),
            trigger_setup(AbsoluteAxisCode::ABS_RZ),
            hat_setup(AbsoluteAxisCode::ABS_HAT0X),
            hat_setup(AbsoluteAxisCode::ABS_HAT0Y),
        ];

        let (pad, pad_node) = build_virtual(
            PAD_NAME,
            VIRTUAL_PRODUCT,
            &pad_keys,
            &abs,
            &Default::default(),
        )?;

        let (keyboard, keyboard_node) = if kb_keys.iter().next().is_some() {
            let (dev, node) =
                build_virtual(KEYBOARD_NAME, 0x1095, &kb_keys, &[], &Default::default())?;
            (Some(dev), node)
        } else {
            (None, None)
        };

        let needs_mouse = profile.stick_to_mouse.enabled
            || profile
                .binds
                .iter()
                .any(|b| matches!(b.to, Action::Mouse(_)));
        let (mouse, mouse_node) = if needs_mouse {
            let mut rel = AttributeSet::<RelativeAxisCode>::new();
            rel.insert(RelativeAxisCode::REL_X);
            rel.insert(RelativeAxisCode::REL_Y);
            let (dev, node) = build_virtual(MOUSE_NAME, 0x1096, &mouse_keys, &[], &rel)?;
            (Some(dev), node)
        } else {
            (None, None)
        };

        Ok(Self {
            pad,
            keyboard,
            mouse,
            pending: Pending::default(),
            nodes: Nodes {
                pad: pad_node,
                keyboard: keyboard_node,
                mouse: mouse_node,
            },
        })
    }

    fn emit_pad(&mut self, ev: InputEvent) -> io::Result<()> {
        self.pending.pad = true;
        self.pad.emit(&[ev])
    }

    pub fn pad_key(&mut self, code: KeyCode, value: i32) -> io::Result<()> {
        self.emit_pad(InputEvent::new(EventType::KEY.0, code.0, value))
    }

    pub fn pad_abs(&mut self, code: AbsoluteAxisCode, value: i32) -> io::Result<()> {
        self.emit_pad(InputEvent::new(EventType::ABSOLUTE.0, code.0, value))
    }

    /// Devuelve `false` si el teclado virtual no existe.
    pub fn key(&mut self, code: KeyCode, value: i32) -> io::Result<bool> {
        let Some(kb) = &mut self.keyboard else {
            return Ok(false);
        };
        self.pending.keyboard = true;
        kb.emit(&[InputEvent::new(EventType::KEY.0, code.0, value)])?;
        Ok(true)
    }

    /// Devuelve `false` si el ratón virtual no existe.
    pub fn mouse_key(&mut self, code: KeyCode, value: i32) -> io::Result<bool> {
        let Some(mouse) = &mut self.mouse else {
            return Ok(false);
        };
        self.pending.mouse = true;
        mouse.emit(&[InputEvent::new(EventType::KEY.0, code.0, value)])?;
        Ok(true)
    }

    /// Devuelve `false` si el ratón virtual no existe.
    pub fn mouse_rel(&mut self, code: RelativeAxisCode, value: i32) -> io::Result<bool> {
        let Some(mouse) = &mut self.mouse else {
            return Ok(false);
        };
        self.pending.mouse = true;
        mouse.emit(&[InputEvent::new(EventType::RELATIVE.0, code.0, value)])?;
        Ok(true)
    }

    /// Cierra el marco de eventos en cada salida con eventos pendientes.
    pub fn sync(&mut self) -> io::Result<()> {
        let syn = || {
            InputEvent::new(
                EventType::SYNCHRONIZATION.0,
                SynchronizationCode::SYN_REPORT.0,
                0,
            )
        };
        if self.pending.pad {
            self.pad.emit(&[syn()])?;
            self.pending.pad = false;
        }
        if self.pending.keyboard {
            if let Some(kb) = &mut self.keyboard {
                kb.emit(&[syn()])?;
            }
            self.pending.keyboard = false;
        }
        if self.pending.mouse {
            if let Some(mouse) = &mut self.mouse {
                mouse.emit(&[syn()])?;
            }
            self.pending.mouse = false;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Traducción de eventos
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pad,
    Keyboard,
    Mouse,
    Dropped,
    Sync,
    Ignored,
}

pub struct Remapper {
    profile: Profile,
    binds: HashMap<u16, Action>,
    absinfo: HashMap<u16, AbsInfo>,
    /// Último valor crudo normalizado de cada stick, en `[-1.0, 1.0]`.
    left: (f32, f32),
    right: (f32, f32),
    /// Mismo valor tras aplicar zona muerta y sensibilidad.
    left_shaped: (f32, f32),
    right_shaped: (f32, f32),
    hold_active: bool,
    mouse_frac: (f64, f64),
    mouse_tick: Instant,
}

impl Remapper {
    pub fn new(profile: Profile, absinfo: HashMap<u16, AbsInfo>) -> Self {
        let binds = profile.bind_map();
        Self {
            profile,
            binds,
            absinfo,
            left: (0.0, 0.0),
            right: (0.0, 0.0),
            left_shaped: (0.0, 0.0),
            right_shaped: (0.0, 0.0),
            hold_active: false,
            mouse_frac: (0.0, 0.0),
            mouse_tick: Instant::now(),
        }
    }

    fn mouse_mode_active(&self) -> bool {
        let cfg = &self.profile.stick_to_mouse;
        cfg.enabled && (cfg.hold.is_none() || self.hold_active)
    }

    fn absinfo(&self, code: AbsoluteAxisCode) -> Option<AbsInfo> {
        self.absinfo.get(&code.0).copied()
    }

    pub fn handle(&mut self, ev: &InputEvent, out: &mut VirtualOutputs) -> io::Result<Outcome> {
        let type_ = ev.event_type();
        if type_ == EventType::SYNCHRONIZATION {
            out.sync()?;
            return Ok(Outcome::Sync);
        }
        if type_ == EventType::KEY {
            return self.handle_key(KeyCode::new(ev.code()), ev.value(), out);
        }
        if type_ == EventType::ABSOLUTE {
            return self.handle_abs(AbsoluteAxisCode(ev.code()), ev.value(), out);
        }
        Ok(Outcome::Ignored)
    }

    fn handle_key(
        &mut self,
        code: KeyCode,
        value: i32,
        out: &mut VirtualOutputs,
    ) -> io::Result<Outcome> {
        if Some(code) == self.profile.stick_to_mouse.hold {
            self.hold_active = value != 0;
        }
        let action = self
            .binds
            .get(&code.0)
            .copied()
            .or_else(|| passthrough_key(code));
        match action {
            Some(Action::Pad(target)) => {
                out.pad_key(target, value)?;
                Ok(Outcome::Pad)
            }
            Some(Action::Key(target)) => {
                if out.key(target, value)? {
                    Ok(Outcome::Keyboard)
                } else {
                    Ok(Outcome::Dropped)
                }
            }
            Some(Action::Mouse(target)) => {
                if out.mouse_key(target, value)? {
                    Ok(Outcome::Mouse)
                } else {
                    Ok(Outcome::Dropped)
                }
            }
            Some(Action::Ignore) | None => Ok(Outcome::Dropped),
        }
    }

    fn handle_abs(
        &mut self,
        code: AbsoluteAxisCode,
        value: i32,
        out: &mut VirtualOutputs,
    ) -> io::Result<Outcome> {
        let Some(role) = axis_role(code) else {
            return Ok(Outcome::Ignored);
        };
        let info = self.absinfo(code);
        match role {
            AxisRole::LeftX | AxisRole::LeftY => {
                let norm = normalize_abs(value, info);
                if role == AxisRole::LeftX {
                    self.left.0 = norm;
                } else {
                    self.left.1 = norm;
                }
                self.emit_stick(Stick::Left, out)
            }
            AxisRole::RightX | AxisRole::RightY => {
                let norm = normalize_abs(value, info);
                if role == AxisRole::RightX {
                    self.right.0 = norm;
                } else {
                    self.right.1 = norm;
                }
                self.emit_stick(Stick::Right, out)
            }
            AxisRole::LeftTrigger | AxisRole::RightTrigger => {
                let shaped = shape_trigger(normalize_trigger(value, info), self.profile.triggers);
                let target = virtual_axis(role).expect("los gatillos tienen eje virtual");
                out.pad_abs(target, to_trigger_i32(shaped))?;
                Ok(Outcome::Pad)
            }
            AxisRole::HatX | AxisRole::HatY => {
                let target = virtual_axis(role).expect("el hat tiene eje virtual");
                out.pad_abs(target, value)?;
                Ok(Outcome::Pad)
            }
        }
    }

    fn emit_stick(&mut self, stick: Stick, out: &mut VirtualOutputs) -> io::Result<Outcome> {
        let cfg = match stick {
            Stick::Left => self.profile.left_stick,
            Stick::Right => self.profile.right_stick,
        };
        let raw = match stick {
            Stick::Left => self.left,
            Stick::Right => self.right,
        };
        let shaped = shape_stick(raw.0, raw.1, cfg);
        match stick {
            Stick::Left => self.left_shaped = shaped,
            Stick::Right => self.right_shaped = shaped,
        }
        if self.mouse_suppresses(stick) {
            return Ok(Outcome::Dropped);
        }
        let (axis_x, axis_y) = match stick {
            Stick::Left => (AxisRole::LeftX, AxisRole::LeftY),
            Stick::Right => (AxisRole::RightX, AxisRole::RightY),
        };
        out.pad_abs(
            virtual_axis(axis_x).expect("eje virtual conocido"),
            to_stick_i32(shaped.0),
        )?;
        out.pad_abs(
            virtual_axis(axis_y).expect("eje virtual conocido"),
            to_stick_i32(shaped.1),
        )?;
        Ok(Outcome::Pad)
    }

    fn mouse_suppresses(&self, stick: Stick) -> bool {
        let cfg = &self.profile.stick_to_mouse;
        cfg.suppress_axis && self.mouse_mode_active() && cfg.stick == stick
    }

    /// Mueve el ratón virtual según el stick elegido. Se llama en cada vuelta
    /// del bucle, no por evento.
    pub fn tick(&mut self, out: &mut VirtualOutputs) -> io::Result<bool> {
        let cfg = self.profile.stick_to_mouse;
        let now = Instant::now();
        let dt = now.duration_since(self.mouse_tick).as_secs_f64();
        if !self.mouse_mode_active() {
            self.mouse_tick = now;
            self.mouse_frac = (0.0, 0.0);
            return Ok(false);
        }
        if dt < MOUSE_TICK.as_secs_f64() {
            return Ok(false);
        }
        self.mouse_tick = now;

        let (x, y) = match cfg.stick {
            Stick::Left => self.left_shaped,
            Stick::Right => self.right_shaped,
        };
        let dx = x as f64 * cfg.speed as f64 * dt + self.mouse_frac.0;
        let dy = y as f64 * cfg.speed as f64 * dt + self.mouse_frac.1;
        let (ix, iy) = (dx.trunc() as i32, dy.trunc() as i32);
        self.mouse_frac = (dx - ix as f64, dy - iy as f64);
        if ix == 0 && iy == 0 {
            return Ok(false);
        }
        let mut emitted = false;
        if ix != 0 {
            emitted |= out.mouse_rel(RelativeAxisCode::REL_X, ix)?;
        }
        if iy != 0 {
            emitted |= out.mouse_rel(RelativeAxisCode::REL_Y, iy)?;
        }
        if emitted {
            out.sync()?;
        }
        Ok(emitted)
    }
}

// ---------------------------------------------------------------------------
// Puente
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub events: u64,
    pub to_pad: u64,
    pub to_keyboard: u64,
    pub to_mouse: u64,
    pub dropped: u64,
    pub disconnected: bool,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// `None` = hasta Ctrl+C.
    pub duration: Option<Duration>,
    pub verbose: bool,
}

pub struct Bridge {
    pub physical: PhysicalPad,
    pub outputs: VirtualOutputs,
    remapper: Remapper,
}

impl Bridge {
    /// Abre el mando físico, crea los dispositivos virtuales y, salvo que se
    /// indique lo contrario, hace `grab()` del físico.
    pub fn start(pad: PadCandidate, profile: Profile, grab: bool) -> io::Result<Self> {
        let mut physical = PhysicalPad::new(pad)?;
        let outputs = VirtualOutputs::new(&profile)?;
        if grab {
            physical.grab()?;
        }
        let remapper = Remapper::new(profile, physical.absinfo.clone());
        Ok(Self {
            physical,
            outputs,
            remapper,
        })
    }

    pub fn run(&mut self, opts: RunOptions) -> io::Result<Stats> {
        let mut stats = Stats::default();
        let mut buf = Vec::with_capacity(64);
        let started = Instant::now();
        let deadline = opts.duration.map(|d| started + d);

        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            match self.physical.fetch(&mut buf)? {
                Fetch::Disconnected => {
                    stats.disconnected = true;
                    break;
                }
                Fetch::Idle => std::thread::sleep(POLL_INTERVAL),
                Fetch::Events => {
                    for ev in &buf {
                        stats.events += 1;
                        let outcome = self.remapper.handle(ev, &mut self.outputs)?;
                        match outcome {
                            Outcome::Pad => stats.to_pad += 1,
                            Outcome::Keyboard => stats.to_keyboard += 1,
                            Outcome::Mouse => stats.to_mouse += 1,
                            Outcome::Dropped => stats.dropped += 1,
                            Outcome::Sync | Outcome::Ignored => {}
                        }
                        if opts.verbose && !matches!(outcome, Outcome::Sync) {
                            println!("  {ev:?} -> {outcome:?}");
                        }
                    }
                }
            }
            if self.remapper.tick(&mut self.outputs)? {
                stats.to_mouse += 1;
            }
        }

        stats.elapsed = started.elapsed();
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// Vista en vivo
// ---------------------------------------------------------------------------

/// Imprime botones y ejes tal y como llegan del mando físico.
pub fn watch(pad: &mut PhysicalPad, duration: Option<Duration>) -> io::Result<Stats> {
    let mut stats = Stats::default();
    let mut buf = Vec::with_capacity(64);
    let started = Instant::now();
    let deadline = duration.map(|d| started + d);
    println!(
        "\nLeyendo '{}' ({}) — Ctrl+C para salir\n",
        pad.name,
        pad.path.display()
    );

    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        match pad.fetch(&mut buf)? {
            Fetch::Disconnected => {
                stats.disconnected = true;
                println!("\n[!] El mando físico se desconectó.");
                break;
            }
            Fetch::Idle => std::thread::sleep(POLL_INTERVAL),
            Fetch::Events => {
                for ev in &buf {
                    if ev.event_type() == EventType::SYNCHRONIZATION {
                        continue;
                    }
                    stats.events += 1;
                    println!("{ev:?}");
                }
            }
        }
    }

    stats.elapsed = started.elapsed();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stick_cfg(deadzone: f32, sensitivity: f32) -> StickConfig {
        StickConfig {
            deadzone,
            sensitivity,
        }
    }

    #[test]
    fn normalize_8bit_stick() {
        let info = AbsInfo::new(0, 0, 255, 0, 0, 0);
        assert!((normalize_abs(128, Some(info))).abs() < 0.01);
        assert!((normalize_abs(255, Some(info)) - 1.0).abs() < 1e-6);
        assert!((normalize_abs(0, Some(info)) + 1.0).abs() < 1e-6);
        // Sin absinfo se asume 0..255.
        assert!((normalize_abs(255, None) - 1.0).abs() < 1e-6);
        // Rango degenerado -> 0 en vez de NaN.
        let flat = AbsInfo::new(0, 5, 5, 0, 0, 0);
        assert_eq!(normalize_abs(5, Some(flat)), 0.0);
    }

    #[test]
    fn normalize_trigger_is_unipolar() {
        let info = AbsInfo::new(0, 0, 255, 0, 0, 0);
        assert_eq!(normalize_trigger(0, Some(info)), 0.0);
        assert!((normalize_trigger(255, Some(info)) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn deadzone_kills_drift_and_preserves_direction() {
        let cfg = stick_cfg(0.10, 1.0);
        assert_eq!(shape_stick(0.05, -0.05, cfg), (0.0, 0.0));

        let (x, y) = shape_stick(1.0, 0.0, cfg);
        assert!((x - 1.0).abs() < 1e-6);
        assert_eq!(y, 0.0);

        let (x, y) = shape_stick(0.3, 0.4, cfg);
        let mag = (x * x + y * y).sqrt();
        // 0.5 crudo -> (0.5 - 0.1) / 0.9 = 0.4444
        assert!((mag - 0.4444).abs() < 1e-3, "mag = {mag}");
        assert!((x / y - 0.3 / 0.4).abs() < 1e-6);
    }

    #[test]
    fn sensitivity_saturates_without_leaving_the_axis_range() {
        let cfg = stick_cfg(0.0, 4.0);
        let (x, y) = shape_stick(0.3, 0.4, cfg);
        let mag = (x * x + y * y).sqrt();
        assert!((mag - 1.0).abs() < 1e-6, "mag = {mag}");
        // La dirección se conserva aunque sature.
        assert!((x / y - 0.3 / 0.4).abs() < 1e-6);
    }

    #[test]
    fn hair_trigger_shifts_the_usable_range() {
        let plain = TriggerConfig {
            hair_trigger: false,
            threshold: 0.5,
        };
        assert!((shape_trigger(0.5, plain) - 0.5).abs() < 1e-6);

        let hair = TriggerConfig {
            hair_trigger: true,
            threshold: 0.5,
        };
        assert_eq!(shape_trigger(0.4, hair), 0.0);
        assert!((shape_trigger(0.5, hair)).abs() < 1e-6);
        assert!((shape_trigger(0.75, hair) - 0.5).abs() < 1e-6);
        assert_eq!(shape_trigger(1.0, hair), 1.0);
    }

    #[test]
    fn output_scaling_matches_virtual_ranges() {
        assert_eq!(to_stick_i32(1.0), 32767);
        assert_eq!(to_stick_i32(-1.0), -32767);
        assert_eq!(to_stick_i32(0.0), 0);
        assert_eq!(to_stick_i32(2.0), 32767);
        assert_eq!(to_trigger_i32(1.0), 255);
        assert_eq!(to_trigger_i32(-1.0), 0);
    }

    #[test]
    fn gamepad_buttons_pass_through_unchanged() {
        assert_eq!(
            passthrough_key(KeyCode::BTN_SOUTH),
            Some(Action::Pad(KeyCode::BTN_SOUTH))
        );
        assert_eq!(
            passthrough_key(KeyCode::BTN_THUMBR),
            Some(Action::Pad(KeyCode::BTN_THUMBR))
        );
        // Los botones traseros BTN_C/BTN_Z también pasan: el gamepad virtual
        // los declara solo si algún bind los usa.
        assert_eq!(
            passthrough_key(KeyCode::BTN_C),
            Some(Action::Pad(KeyCode::BTN_C))
        );
        // Gatillos digitales: en el virtual son solo eje analógico.
        assert_eq!(passthrough_key(KeyCode::BTN_TL2), None);
        assert_eq!(passthrough_key(KeyCode::BTN_TR2), None);
        // Fuera del rango gamepad (teclado, ratón) no se reenvía nada.
        assert_eq!(passthrough_key(KeyCode::KEY_A), None);
    }

    #[test]
    fn axis_roles_map_dinput_to_xbox() {
        assert_eq!(
            virtual_axis(axis_role(AbsoluteAxisCode::ABS_Z).unwrap()),
            Some(AbsoluteAxisCode::ABS_RX)
        );
        assert_eq!(
            virtual_axis(axis_role(AbsoluteAxisCode::ABS_GAS).unwrap()),
            Some(AbsoluteAxisCode::ABS_RZ)
        );
        assert_eq!(axis_role(AbsoluteAxisCode::ABS_THROTTLE), None);
    }
}
