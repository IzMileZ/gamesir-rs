//! Bandeja del sistema por D-Bus (StatusNotifierItem, `ksni`).
//!
//! Es el protocolo nativo de bandeja en Wayland: Waybar (y cualquier otro
//! host StatusNotifier) muestra el ícono y el menú sin depender de X11.
//! El servicio vive en su propio hilo; los clics del menú llegan como
//! [`TrayCmd`] al controlador y el estado se comparte con [`TrayView`].

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, StandardItem, SubMenu};
use ksni::{Category, Icon, Status, ToolTip};

use crate::gui::{WorkerMode, lock};

/// Lo que la bandeja muestra del estado global. Lo escribe el controlador y
/// lo lee el hilo del servicio D-Bus; comparar snapshots evita emitir
/// señales de cambio cuando no cambió nada.
#[derive(Clone, PartialEq, Default)]
pub struct TrayView {
    pub mode: WorkerMode,
    /// Nombre corto del mando, o `None` si no hay ninguno conectado.
    pub pad: Option<String>,
    pub window_open: bool,
    /// Perfiles ofrecidos en el submenú: etiqueta y ruta (`None` = embebido).
    pub profiles: Vec<(String, Option<PathBuf>)>,
    /// Ruta del perfil activo; `None` = el embebido.
    pub active: Option<PathBuf>,
}

impl TrayView {
    fn bridge_wanted(&self) -> bool {
        matches!(self.mode, WorkerMode::Bridge | WorkerMode::Reconnect)
    }
}

/// Clics del menú (y del ícono) hacia el controlador.
pub enum TrayCmd {
    /// Mostrar la ventana si está oculta, ocultarla si está abierta.
    ToggleWindow,
    /// Activar (`true`) o detener (`false`) el puente.
    SetBridge(bool),
    /// Cargar un perfil: `None` = el embebido.
    LoadProfile(Option<PathBuf>),
    /// Cerrar de verdad: ventana, hilo de entrada y dispositivos virtuales.
    Quit,
}

pub struct GamesirTray {
    view: Arc<Mutex<TrayView>>,
    tx: Sender<TrayCmd>,
}

impl GamesirTray {
    fn send(&self, cmd: TrayCmd) {
        let _ = self.tx.send(cmd);
    }

    fn view(&self) -> TrayView {
        lock(&self.view).clone()
    }
}

impl ksni::Tray for GamesirTray {
    fn id(&self) -> String {
        "gamesir-rs".into()
    }

    fn title(&self) -> String {
        "gamesir-rs — Tegenaria Lite".into()
    }

    fn category(&self) -> Category {
        Category::Hardware
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![icono(lock(&self.view).mode == WorkerMode::Bridge)]
    }

    fn tool_tip(&self) -> ToolTip {
        let v = self.view();
        ToolTip {
            title: "gamesir-rs — Tegenaria Lite".into(),
            description: format!(
                "{} · {}",
                v.pad.clone().unwrap_or_else(|| "sin mando".into()),
                v.mode.label()
            ),
            ..Default::default()
        }
    }

    /// Clic izquierdo sobre el ícono: mostrar/ocultar la ventana.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCmd::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let v = self.view();
        let mut menu = vec![MenuItem::Standard(StandardItem {
            label: if v.window_open {
                "Ocultar ventana".into()
            } else {
                "Mostrar ventana".into()
            },
            activate: Box::new(|t: &mut Self| t.send(TrayCmd::ToggleWindow)),
            ..Default::default()
        })];
        menu.push(MenuItem::Standard(StandardItem {
            label: if v.bridge_wanted() {
                "Detener puente del mando".into()
            } else {
                "Activar puente del mando".into()
            },
            enabled: v.pad.is_some() || v.bridge_wanted(),
            activate: Box::new(|t: &mut Self| {
                let on = t.view().bridge_wanted();
                t.send(TrayCmd::SetBridge(!on));
            }),
            ..Default::default()
        }));
        menu.push(MenuItem::SubMenu(SubMenu {
            label: "Perfiles".into(),
            submenu: v
                .profiles
                .iter()
                .map(|(label, path)| {
                    let ruta = path.clone();
                    MenuItem::Checkmark(CheckmarkItem {
                        label: label.clone(),
                        checked: v.active.as_ref() == path.as_ref(),
                        activate: Box::new(move |t: &mut Self| {
                            t.send(TrayCmd::LoadProfile(ruta.clone()));
                        }),
                        ..Default::default()
                    })
                })
                .collect(),
            ..Default::default()
        }));
        menu.push(MenuItem::Separator);
        menu.push(MenuItem::Standard(StandardItem {
            label: "Salir".into(),
            activate: Box::new(|t: &mut Self| t.send(TrayCmd::Quit)),
            ..Default::default()
        }));
        menu
    }
}

/// Registra el ícono en el bus de sesión. Devuelve el handle para actualizar
/// el estado y emitir las señales de cambio; el servicio corre en su hilo.
pub fn iniciar(
    view: Arc<Mutex<TrayView>>,
    tx: Sender<TrayCmd>,
) -> Result<ksni::blocking::Handle<GamesirTray>, String> {
    GamesirTray { view, tx }
        .spawn()
        .map_err(|e| format!("bandeja: {e}"))
}

/// Ícono de gamepad 22×22 dibujado a mano, en ARGB32 (orden de red).
/// Verde con el puente activo, gris en reposo.
fn icono(activo: bool) -> Icon {
    const W: i32 = 22;
    const H: i32 = 22;
    let cuerpo = if activo {
        (46, 160, 67)
    } else {
        (122, 126, 122)
    };
    let detalle = (22, 26, 22);
    let (x0, x1, y0, y1, r) = (2, 19, 5, 16, 3);

    let mut data = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let dx = (x0 + r - x).max(x - (x1 - r)).max(0);
            let dy = (y0 + r - y).max(y - (y1 - r)).max(0);
            let en_cuerpo =
                (x0..=x1).contains(&x) && (y0..=y1).contains(&y) && dx * dx + dy * dy <= r * r;
            let en_cruz = ((6..=8).contains(&x) && (9..=12).contains(&y))
                || ((5..=9).contains(&x) && (10..=11).contains(&y));
            let en_boton =
                (x - 14).pow(2) + (y - 9).pow(2) <= 1 || (x - 16).pow(2) + (y - 12).pow(2) <= 1;
            let (rgb, alpha) = if en_cuerpo && (en_cruz || en_boton) {
                (detalle, 255)
            } else if en_cuerpo {
                (cuerpo, 255)
            } else {
                ((0, 0, 0), 0)
            };
            data.extend_from_slice(&[alpha, rgb.0, rgb.1, rgb.2]);
        }
    }
    Icon {
        width: W,
        height: H,
        data,
    }
}
