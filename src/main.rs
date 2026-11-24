//! gamesir-rs: interfaz gráfica (por defecto) y CLI de diagnóstico/puente.

mod gui;
mod input;
mod profile;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use input::{Bridge, PadCandidate, PhysicalPad, RunOptions, Stats};
use profile::Profile;

const USAGE: &str = "\
gamesir-rs — puente de entrada para el GameSir Tegenaria Lite

Uso:
  gamesir-rs                       abre la interfaz gráfica
  gamesir-rs --bridge              puente en la terminal, sin GUI
  gamesir-rs --list                lista los gamepads detectados y sale
  gamesir-rs --watch               vista en vivo de botones y ejes del físico
  gamesir-rs --profile PERFIL.toml usa un perfil concreto (también en la GUI)
  gamesir-rs --write-default-profile RUTA
                                   vuelca el perfil embebido en TOML

Opciones:
  -d, --device RUTA   nodo /dev/input/eventXX en vez de autodetectar
  -t, --duration SEG  cierra el puente tras SEG segundos (0 = hasta Ctrl+C)
  -n, --no-grab       no tomar el mando en exclusivo (para probar en paralelo)
  -v, --verbose       imprimir cada evento traducido
  -h, --help          esta ayuda

Sin --duration, Ctrl+C termina el proceso y el kernel libera el grab() y los
dispositivos virtuales.
";

#[derive(Default)]
struct Args {
    list: bool,
    watch: bool,
    bridge: bool,
    help: bool,
    verbose: bool,
    no_grab: bool,
    device: Option<PathBuf>,
    profile_path: Option<PathBuf>,
    write_default_profile: Option<PathBuf>,
    duration: Option<Duration>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} necesita un valor"));
        match arg.as_str() {
            "-h" | "--help" => args.help = true,
            "--list" => args.list = true,
            "--watch" => args.watch = true,
            "-b" | "--bridge" => args.bridge = true,
            "-v" | "--verbose" => args.verbose = true,
            "-n" | "--no-grab" => args.no_grab = true,
            "-d" | "--device" => args.device = Some(PathBuf::from(value("--device")?)),
            "-p" | "--profile" => args.profile_path = Some(PathBuf::from(value("--profile")?)),
            "--write-default-profile" => {
                args.write_default_profile = Some(PathBuf::from(value("--write-default-profile")?))
            }
            "-t" | "--duration" => {
                let raw = value("--duration")?;
                let secs: f64 = raw
                    .parse()
                    .map_err(|e| format!("--duration no es un número: {e}"))?;
                if secs < 0.0 {
                    return Err("--duration no puede ser negativo".into());
                }
                args.duration = Some(Duration::from_secs_f64(secs));
            }
            other => return Err(format!("argumento desconocido: {other}\n\n{USAGE}")),
        }
    }
    Ok(args)
}

/// Resuelve qué mando físico usar: el indicado con `--device` o el mejor
/// candidato detectado.
fn resolve_pad(args: &Args) -> Result<PadCandidate, String> {
    if let Some(path) = &args.device {
        return input::open_pad(path)
            .map_err(|e| format!("no se pudo abrir {}: {e}", path.display()));
    }
    let pads = input::find_pads();
    input::pick_pad(pads).ok_or_else(|| {
        "no se detectó ningún gamepad. ¿Está conectado el Tegenaria Lite? \
         Si tiene selector de modo (XInput / DInput / Switch), prueba otro modo."
            .to_string()
    })
}

fn print_stats(stats: &Stats) {
    println!(
        "[ok] Sesión de {:.2} s | eventos={} pad={} kbd={} mouse={} descartados={}",
        stats.elapsed.as_secs_f64(),
        stats.events,
        stats.to_pad,
        stats.to_keyboard,
        stats.to_mouse,
        stats.dropped
    );
    if stats.disconnected {
        println!("[!] El mando físico se desconectó.");
    }
}

/// Modo pedido en la línea de comandos. Sin flags se abre la GUI; `--bridge`
/// fuerza el puente en la terminal (útil para los bancos de pruebas de tools/).
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Gui,
    List,
    Watch,
    Bridge,
}

fn mode_of(args: &Args) -> Mode {
    match (args.list, args.watch, args.bridge) {
        (true, ..) => Mode::List,
        (_, true, _) => Mode::Watch,
        (_, _, true) => Mode::Bridge,
        _ => Mode::Gui,
    }
}

fn run(args: Args) -> Result<(), String> {
    if let Some(path) = &args.write_default_profile {
        std::fs::write(path, profile::DEFAULT_PROFILE_TOML)
            .map_err(|e| format!("no se pudo escribir {}: {e}", path.display()))?;
        println!("[ok] Perfil por defecto escrito en {}", path.display());
        return Ok(());
    }

    let profile = match &args.profile_path {
        Some(path) => Profile::load(path).map_err(|e| format!("{}: {e}", path.display()))?,
        None => Profile::embedded_default(),
    };

    let mode = mode_of(&args);

    if mode == Mode::Gui {
        return gui::run(profile, args.profile_path.clone());
    }

    if mode == Mode::List {
        let pads = input::find_pads();
        if pads.is_empty() {
            println!("No se detectó ningún gamepad.");
            return Ok(());
        }
        println!("Gamepads detectados: {}\n", pads.len());
        for pad in &pads {
            print!("{}", input::describe(pad));
            println!();
        }
        return Ok(());
    }

    let pad = resolve_pad(&args)?;

    if mode == Mode::Watch {
        let mut physical = PhysicalPad::new(pad).map_err(|e| format!("mando físico: {e}"))?;
        let stats =
            input::watch(&mut physical, args.duration).map_err(|e| format!("lectura: {e}"))?;
        println!("\n[ok] Lectura detenida. Eventos leídos: {}", stats.events);
        return Ok(());
    }

    println!(
        "[..] Perfil  : '{}' ({} binds)",
        profile.name,
        profile.binds.len()
    );
    println!(
        "[..] Físico  : {} ({}) {:04x}:{:04x}",
        pad.name,
        pad.path.display(),
        pad.vendor,
        pad.product
    );

    let mut bridge =
        Bridge::start(pad, profile, !args.no_grab).map_err(|e| format!("puente: {e}"))?;
    println!("[ok] Virtual : {}", bridge.outputs.nodes);
    println!(
        "[ok] Grab    : {}",
        if bridge.physical.is_grabbed() {
            "exclusivo (los juegos solo ven el mando virtual)"
        } else {
            "desactivado (--no-grab)"
        }
    );
    println!("[..] Ctrl+C para salir\n");

    let opts = RunOptions {
        duration: args.duration,
        verbose: args.verbose,
    };
    let stats = bridge.run(opts).map_err(|e| format!("puente: {e}"))?;
    print_stats(&stats);
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if args.help {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[!] {e}");
            ExitCode::FAILURE
        }
    }
}
