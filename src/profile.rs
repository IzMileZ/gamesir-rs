//! Perfiles de remapeo: tipos, carga/serialización TOML y saneado de valores.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io;
use std::path::Path;

use evdev::KeyCode;
use serde::{Deserialize, Serialize};

/// Perfil de prueba embebido en el binario (también lo vuelca
/// `--write-default-profile`).
pub const DEFAULT_PROFILE_TOML: &str = include_str!("../profiles/default.toml");

/// Destino de un botón físico remapeado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Botón del gamepad virtual.
    Pad(KeyCode),
    /// Tecla del teclado virtual.
    Key(KeyCode),
    /// Botón de ratón virtual (`BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`).
    Mouse(KeyCode),
    /// Descartar el evento.
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bind {
    /// Código de botón del mando físico.
    pub from: KeyCode,
    pub to: Action,
}

/// Zona muerta y sensibilidad de un stick Hall Effect.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StickConfig {
    /// Zona muerta radial, 0.0 (ninguna) .. 0.95.
    pub deadzone: f32,
    /// Ganancia aplicada tras la zona muerta, 0.0 .. 4.0.
    pub sensitivity: f32,
}

impl Default for StickConfig {
    fn default() -> Self {
        Self {
            deadzone: 0.08,
            sensitivity: 1.0,
        }
    }
}

impl StickConfig {
    pub fn sanitized(mut self) -> Self {
        self.deadzone = self.deadzone.clamp(0.0, 0.95);
        self.sensitivity = self.sensitivity.clamp(0.0, 4.0);
        if !self.deadzone.is_finite() {
            self.deadzone = 0.0;
        }
        if !self.sensitivity.is_finite() {
            self.sensitivity = 1.0;
        }
        self
    }
}

/// Umbral rápido (hair trigger) de los gatillos L2/R2.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TriggerConfig {
    /// Con `true`, el recorrido útil empieza en `threshold` y llega a tope antes.
    pub hair_trigger: bool,
    /// Umbral de pulsación digital, 0.01 .. 1.0.
    pub threshold: f32,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            hair_trigger: false,
            threshold: 0.15,
        }
    }
}

impl TriggerConfig {
    pub fn sanitized(mut self) -> Self {
        self.threshold = self.threshold.clamp(0.01, 1.0);
        if !self.threshold.is_finite() {
            self.threshold = 0.15;
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stick {
    Left,
    Right,
}

/// Convierte la deflexión de un stick en movimiento del ratón virtual.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StickToMouse {
    pub enabled: bool,
    pub stick: Stick,
    /// Botón físico que hay que mantener pulsado; sin él, el modo está siempre activo.
    pub hold: Option<KeyCode>,
    /// Cuentas relativas por segundo con el stick a tope.
    pub speed: f32,
    /// Si es `true`, el stick deja de enviarse al gamepad virtual mientras mueve el ratón.
    pub suppress_axis: bool,
}

impl Default for StickToMouse {
    fn default() -> Self {
        Self {
            enabled: false,
            stick: Stick::Right,
            hold: None,
            speed: 900.0,
            suppress_axis: true,
        }
    }
}

impl StickToMouse {
    pub fn sanitized(mut self) -> Self {
        self.speed = self.speed.clamp(0.0, 20_000.0);
        if !self.speed.is_finite() {
            self.speed = 900.0;
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    /// En TOML cada entrada es una tabla `[[bind]]`.
    #[serde(rename = "bind")]
    pub binds: Vec<Bind>,
    pub left_stick: StickConfig,
    pub right_stick: StickConfig,
    pub triggers: TriggerConfig,
    pub stick_to_mouse: StickToMouse,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: String::from("sin nombre"),
            binds: Vec::new(),
            left_stick: StickConfig::default(),
            right_stick: StickConfig::default(),
            triggers: TriggerConfig::default(),
            stick_to_mouse: StickToMouse::default(),
        }
    }
}

#[derive(Debug)]
pub enum ProfileError {
    Io(io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "error de E/S: {e}"),
            Self::Parse(e) => write!(f, "TOML inválido: {e}"),
            Self::Serialize(e) => write!(f, "no se pudo serializar: {e}"),
        }
    }
}

impl Error for ProfileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::Serialize(e) => Some(e),
        }
    }
}

impl From<io::Error> for ProfileError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<toml::de::Error> for ProfileError {
    fn from(e: toml::de::Error) -> Self {
        Self::Parse(e)
    }
}

impl From<toml::ser::Error> for ProfileError {
    fn from(e: toml::ser::Error) -> Self {
        Self::Serialize(e)
    }
}

impl Profile {
    /// Perfil de prueba embebido, ya saneado.
    pub fn embedded_default() -> Self {
        Self::parse(DEFAULT_PROFILE_TOML).expect("el perfil embebido debe ser válido")
    }

    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        Ok(toml::from_str::<Self>(text)?.sanitized())
    }

    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    pub fn save(&self, path: &Path) -> Result<(), ProfileError> {
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }

    pub fn to_toml(&self) -> Result<String, ProfileError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Aplica límites a todos los valores numéricos del perfil.
    pub fn sanitized(mut self) -> Self {
        self.left_stick = self.left_stick.sanitized();
        self.right_stick = self.right_stick.sanitized();
        self.triggers = self.triggers.sanitized();
        self.stick_to_mouse = self.stick_to_mouse.sanitized();
        self
    }

    /// Tabla de búsqueda de binds por código de botón físico. Si un código se
    /// repite, gana la última entrada del perfil.
    pub fn bind_map(&self) -> HashMap<u16, Action> {
        self.binds.iter().map(|b| (b.from.0, b.to)).collect()
    }

    /// Firma de lo que obliga a **recrear** los dispositivos virtuales: qué
    /// códigos hay que declarar y si hace falta el ratón virtual. Si no cambia,
    /// el perfil puede aplicarse en caliente con
    /// [`Remapper::set_tuning`](crate::input::Remapper::set_tuning).
    pub fn caps_signature(&self) -> (Vec<(u16, Action)>, bool) {
        let mut binds: Vec<(u16, Action)> = self.bind_map().into_iter().collect();
        binds.sort_by_key(|(code, _)| *code);
        (binds, self.stick_to_mouse.enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evdev::KeyCode;

    #[test]
    fn embedded_default_parses() {
        let p = Profile::embedded_default();
        assert_eq!(p.name, "Prueba Tegenaria");
        assert_eq!(p.binds.len(), 5);

        let map = p.bind_map();
        assert_eq!(map[&KeyCode::BTN_SOUTH.0], Action::Pad(KeyCode::BTN_SOUTH));
        assert_eq!(map[&KeyCode::BTN_C.0], Action::Pad(KeyCode::BTN_C));
        assert_eq!(map[&KeyCode::BTN_NORTH.0], Action::Key(KeyCode::KEY_SPACE));
        assert_eq!(map[&KeyCode::BTN_WEST.0], Action::Mouse(KeyCode::BTN_LEFT));
        assert_eq!(map[&KeyCode::BTN_Z.0], Action::Ignore);

        assert!(p.triggers.hair_trigger);
        assert!((p.triggers.threshold - 0.12).abs() < 1e-6);
        assert!(!p.stick_to_mouse.enabled);
        assert_eq!(p.stick_to_mouse.stick, Stick::Right);
        assert_eq!(p.stick_to_mouse.hold, Some(KeyCode::BTN_SELECT));
    }

    #[test]
    fn missing_sections_fall_back_to_defaults() {
        let p = Profile::parse("name = \"vacío\"\n").unwrap();
        assert!(p.binds.is_empty());
        assert_eq!(p.left_stick.deadzone, StickConfig::default().deadzone);
        assert!(!p.triggers.hair_trigger);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let p = Profile::parse(
            "[left_stick]\ndeadzone = 5.0\nsensitivity = -1.0\n\n\
             [triggers]\nthreshold = 99.0\n",
        )
        .unwrap();
        assert_eq!(p.left_stick.deadzone, 0.95);
        assert_eq!(p.left_stick.sensitivity, 0.0);
        assert_eq!(p.triggers.threshold, 1.0);
    }

    #[test]
    fn misspelled_keys_are_rejected() {
        let err = Profile::parse("[left_stick]\ndeadzon = 0.1\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("TOML inválido"), "mensaje inesperado: {err}");
    }

    #[test]
    fn unknown_button_name_is_rejected() {
        let err = Profile::parse("[[bind]]\nfrom = \"BTN_INEXISTENTE\"\nto = \"ignore\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("TOML inválido"), "mensaje inesperado: {err}");
    }

    #[test]
    fn roundtrips_through_toml() {
        let p = Profile::embedded_default();
        let again = Profile::parse(&p.to_toml().unwrap()).unwrap();
        assert_eq!(again.bind_map(), p.bind_map());
        assert_eq!(again.name, p.name);
        assert_eq!(again.triggers.threshold, p.triggers.threshold);
    }

    #[test]
    fn caps_signature_only_changes_with_the_virtual_devices() {
        let a = Profile::parse("[[bind]]\nfrom = \"BTN_C\"\nto = { key = \"KEY_A\" }\n").unwrap();
        let b = Profile::parse("[[bind]]\nfrom = \"BTN_C\"\nto = { pad = \"BTN_C\" }\n").unwrap();
        assert_ne!(
            a.caps_signature().0,
            b.caps_signature().0,
            "cambiar el destino de un bind obliga a recrear los virtuales"
        );

        // Ajustes puramente numéricos: aplicables en caliente.
        let c = Profile::parse("[left_stick]\ndeadzone = 0.3\n").unwrap();
        let d = Profile::parse("[left_stick]\ndeadzone = 0.1\n").unwrap();
        assert_eq!(c.caps_signature(), d.caps_signature());

        // Activar stick-a-ratón hace falta el ratón virtual.
        let e = Profile::parse("[stick_to_mouse]\nenabled = true\n").unwrap();
        assert_ne!(c.caps_signature(), e.caps_signature());
    }
}
