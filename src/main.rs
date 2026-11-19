// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! regrow — utilidad libre de mantenimiento de sectores crudos (HDD/SSD/NVMe)
//! que se ejecuta desde el SO anfitrión (macOS/Linux/Windows), sin arranque
//! dedicado.
//!
//! Subcomandos:
//! - `scan` — inspección de solo lectura (no destructiva).
//! - `repair` — reescritura dirigida a sectores problemáticos.
//! - `regenerate` — reescribe todo un rango, incluso sectores sanos.
//! - `prescan` — barrido bulk rápido que reporta zonas problemáticas.
//! - `speedtest` — lectura bulk cronometrada y zonas más lentas.
//! - `smart` — estado SMART vía smartctl.
//! - `menu` — menú interactivo (misma ejecución que el CLI).
//!
//! Default no destructivo: los sectores ilegibles NO se reescriben con ceros
//! salvo `--allow-data-loss` explícito. `--backup DIR` guarda el contenido de
//! cada sector problemático antes de reescribirlo (si el respaldo falla, no
//! se escribe). Toda pasada (también el escaneo) deja un checkpoint
//! `diskstats/DISKn.state` automático: `--resume` lo retoma y el menú lo ofrece
//! al elegir una unidad que tenga una pasada interrumpida.

mod backup;
mod config;
mod engine;
mod log;
mod mapfile;
mod menu;
mod rescue;
mod smart;
mod source;
mod speed;
mod state;
mod surfmap;
mod tui;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime};

use backup::{FileBackup, SectorBackup};
use config::{format_disk_line, update_disklist};
use engine::{
    band_flag, prescan, run, Cell, EngineOptions, EngineStats, ProblemKind, RunBuilder, RunHooks,
    RunMode, GRID_CELLS,
};
use log::{format_asctime, format_hms, merge_logs, rotate_log_chain, write_log, DiskLog, Flag, Record};
use smart::{print_status, query as smart_query, TemperatureGuard};
use source::{FileImage, MediaKind, RawDisk, SectorSource};
use speed::{run_speedtest, SPEED_CHUNK};
use state::{load_pending, resume_note, resume_plan, state_path, Checkpoint};

/// Opciones compartidas por el CLI y el menú interactivo.
#[derive(Clone, Debug)]
pub struct CommonOpts {
    pub image: Option<PathBuf>,
    pub raw: Option<u32>, // unidad crudo nativa (/dev/rdiskN, /dev/sdX, PhysicalDriveN)
    pub dev: Option<PathBuf>, // ruta explícita de dispositivo crudo
    pub start: u32,
    pub end: Option<u32>, // exclusivo; None = total
    pub id: u32, // identificador de registros (DISKn.LOG, DISKn.bak.raw, DISKn.state)
    pub id_explicit: bool, // true si --id se dio explícito 
    pub log_dir: PathBuf,
    pub no_log: bool,
    pub merge: bool,
    pub delay_ms: u64,
    /// Umbrales de bandas de latencia en ms . `None` (opción
    /// `--latency off`) desactiva la clasificación: comportamiento sin clasificación.
    pub bands: Option<[u64; 4]>,
    pub allow_data_loss: bool,
    pub backup_dir: Option<PathBuf>,
    pub prescan: bool, // usar prescan bulk dentro de scan/repair
    pub resume: bool,  // retomar desde DISKn.state
    pub max_temp: Option<u64>, // umbral °C para el monitor SMART
    pub ssd_wear_ok: bool,     // desbloquea regenerate en SSD 
    // Rescate (rescue): la ruta posicional pasa a ser la IMAGEN DESTINO.
    pub block: usize,                // sectores por bloque de la fase directa
    pub retries: u8,                 // tentativas por sector en el rascado
    pub reverse: bool,               // pasada inversa (activa por defecto)
    pub map_path: Option<PathBuf>,   // mapfile propio (def: <imagen-destino>.map)
    pub export_map: Option<PathBuf>, // exportar además el mapa en texto
    pub import_map: Option<PathBuf>, // continuar desde un mapfile de texto ajeno
    // Mapa de superficie (surfmap):
    pub sectors: Option<u64>, // dominio; si falta se toma del checkpoint
    pub cells: usize,         // resolución del mapa (def 1024)
    pub out_path: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "uso: regrow <scan|repair|refresh|regenerate|prescan|speedtest> <imagen> [opciones]\n\
         \x20 regrow <…> --raw N [opciones]\n\
         \x20 regrow <…> --dev PATH [opciones]\n\
         \x20 regrow smart --raw N | --dev PATH\n\
         \x20 regrow selftest <status|short|long|conveyance|abort> --raw N | --dev PATH\n\
         \x20 regrow rescue <imagen-destino> --raw N | --dev PATH [opciones]\n\
         \x20 regrow surfmap --id N [--log-dir D] [--sectors N] [--cells N] [--out F]\n\
         \x20 regrow menu [imagen|--raw N|--dev PATH] [opciones]\n\
         \x20 --start N          primer sector (def 0)\n\
         \x20 --end N            último sector exclusivo (def: total)\n\
         \x20 --id N             identificador de registros DISK n (def: N de --raw, si no 1)\n\
         \x20 --log-dir D        directorio de registros (def diskstats)\n\
         \x20 --merge            fusionar con el DISKn.LOG existente\n\
         \x20 --no-log           no escribir registros\n\
         \x20 --delay-ms N       umbral de retardo (banda T4) en ms (def 200)\n\
         \x20 --latency a,b,c,d  bandas de latencia en ms, estrictamente crecientes\n\
         \x20                    (def 5,20,50,200); 'off' desactiva la clasificación\n\
         \x20                    (los retardos solo cuentan, sin flag en el log)\n\
         \x20 --backup DIR       respaldar en DIR/DISKn.bak.raw el contenido de los\n\
         \x20                    sectores problemáticos antes de reescribirlos\n\
         \x20 --allow-data-loss  escribir ceros sobre ilegibles para forzar su\n\
         \x20                    reasignación por el firmware (destructivo; por defecto NO)\n\
         \x20 --prescan          barrido bulk previo dentro de scan/repair\n\
         \x20 --resume           retomar la pasada interrumpida desde DISKn.state\n\
         \x20                    (en el menú se ofrece solo si la unidad tiene una)\n\
         \x20 --max-temp C       pausar la pasada si el disco supera C °C (usa smartctl)\n\
         \x20 --ssd-wear-ok      permitir regenerate sobre medio sólido (SSD/NVMe):\n\
         \x20                    reescribir páginas sanas desgasta la NAND sin beneficio\n\
         \x20 --block N          (rescue) sectores por bloque de la fase directa (def 128 = 64 KiB)\n\
         \x20 --retry N          (rescue) tentativas por sector fallido en el rascado (def 3)\n\
         \x20 --no-reverse       (rescue) desactivar la pasada inversa final\n\
         \x20 --map F            (rescue) mapfile propio (def: <imagen-destino>.map)\n\
         \x20 --export-map F     (rescue) exportar además el mapa en texto compatible\n\
         \x20 --import-map F     (rescue) continuar desde un mapfile de texto ajeno\n\
         \x20                    (rescue: la ruta posicional es la IMAGEN DESTINO sparse;\n\
         \x20                    el origen es SIEMPRE --raw/--dev; --resume retoma no linealmente)\n\
         \x20 --sectors N        (surfmap) dominio del mapa; si falta, se toma del DISKn.state\n\
         \x20 --cells N          (surfmap) resolución 16..8192 (def 1024)\n\
         \x20 --raw N            unidad crudo nativa (root/admin requerido)\n\
         \x20 --dev PATH         ruta explícita de dispositivo crudo\n\
         \x20 --version          versión y licencia\n\
         \x20 En menú, --start/--end fijan el rango sin preguntar; si no, se pregunta en cada pasada.\n\
         \x20 ReGrow © 2026 TuckerDS — software libre bajo licencia GPL-3.0-or-later,\n\
         \x20 sin garantía de uso (ver LICENSE)."
    );
    std::process::exit(2)
}

fn take_num(args: &[String], i: &mut usize, name: &str) -> u64 {
    *i += 1;
    if *i >= args.len() {
        eprintln!("error: {name} necesita valor");
        std::process::exit(2);
    }
    match args[*i].parse::<u64>() {
        Ok(v) => v,
        Err(_) => {
            eprintln!("error: valor inválido para {name}: {}", args[*i]);
            std::process::exit(2);
        }
    }
}

fn take_path(args: &[String], i: &mut usize, name: &str) -> PathBuf {
    *i += 1;
    if *i >= args.len() {
        eprintln!("error: {name} necesita valor");
        std::process::exit(2);
    }
    PathBuf::from(&args[*i])
}

/// `--latency a,b,c,d`: umbrales de las bandas T1..T4 en ms, estrictamente
/// crecientes; `off` desactiva la clasificación (los retardos solo cuentan).
fn latency_error(v: &str) -> ! {
    eprintln!(
        "error: --latency necesita cuatro umbrales en ms estrictamente crecientes (p. ej. 5,20,50,200) o 'off': {v}"
    );
    std::process::exit(2)
}

fn take_latency(args: &[String], i: &mut usize) -> Option<[u64; 4]> {
    *i += 1;
    if *i >= args.len() {
        latency_error("(falta el valor)");
    }
    let v = args[*i].trim();
    if v.eq_ignore_ascii_case("off") {
        return None;
    }
    let parsed: Option<Vec<u64>> = v
        .split(',')
        .map(|p| p.trim().parse::<u64>())
        .collect::<Result<Vec<u64>, _>>()
        .ok();
    match parsed {
        Some(p) if p.len() == 4 => {
            if p[0] == 0 || p[0] >= p[1] || p[1] >= p[2] || p[2] >= p[3] {
                latency_error(v);
            }
            Some([p[0], p[1], p[2], p[3]])
        }
        _ => latency_error(v),
    }
}

/// Parsea la imagen posicional opcional + flags comunes (CLI y menú).
pub(crate) fn parse_common(args: &[String]) -> CommonOpts {
    let mut o = CommonOpts {
        image: None,
        raw: None,
        dev: None,
        start: 0,
        end: None,
        id: 1,
        id_explicit: false,
        log_dir: PathBuf::from("diskstats"),
        no_log: false,
        merge: false,
        delay_ms: engine::DEFAULT_DELAY_MS,
        bands: Some(engine::DEFAULT_LATENCY_BANDS),
        allow_data_loss: false,
        backup_dir: None,
        prescan: false,
        resume: false,
        max_temp: None,
        ssd_wear_ok: false,
        block: rescue::RESCUE_BLOCK_DEFAULT,
        retries: rescue::RESCUE_RETRIES_DEFAULT,
        reverse: true,
        map_path: None,
        export_map: None,
        import_map: None,
        sectors: None,
        cells: surfmap::SURFMAP_DEFAULT_CELLS,
        out_path: None,
    };
    let mut i = 0;
    let mut delay_given = false;
    if !args.is_empty() && !args[0].starts_with("--") {
        o.image = Some(PathBuf::from(&args[0]));
        i = 1;
    }
    while i < args.len() {
        match args[i].as_str() {
            "--start" => o.start = take_num(args, &mut i, "--start") as u32,
            "--end" => o.end = Some(take_num(args, &mut i, "--end") as u32),
            "--id" => {
                o.id = take_num(args, &mut i, "--id") as u32;
                o.id_explicit = true;
            }
            "--delay-ms" => {
                o.delay_ms = take_num(args, &mut i, "--delay-ms");
                delay_given = true;
            }
            "--latency" => o.bands = take_latency(args, &mut i),
            "--max-temp" => o.max_temp = Some(take_num(args, &mut i, "--max-temp")),
            "--raw" => o.raw = Some(take_num(args, &mut i, "--raw") as u32),
            "--dev" => o.dev = Some(take_path(args, &mut i, "--dev")),
            "--log-dir" => o.log_dir = take_path(args, &mut i, "--log-dir"),
            "--backup" => o.backup_dir = Some(take_path(args, &mut i, "--backup")),
            "--no-log" => o.no_log = true,
            "--merge" => o.merge = true,
            "--allow-data-loss" => o.allow_data_loss = true,
            "--prescan" => o.prescan = true,
            "--resume" => o.resume = true,
            "--ssd-wear-ok" => o.ssd_wear_ok = true,
            "--block" => o.block = take_num(args, &mut i, "--block").clamp(1, 65536) as usize,
            "--retry" => o.retries = take_num(args, &mut i, "--retry").clamp(1, 255) as u8,
            "--no-reverse" => o.reverse = false,
            "--map" => o.map_path = Some(take_path(args, &mut i, "--map")),
            "--export-map" => o.export_map = Some(take_path(args, &mut i, "--export-map")),
            "--import-map" => o.import_map = Some(take_path(args, &mut i, "--import-map")),
            "--sectors" => o.sectors = Some(take_num(args, &mut i, "--sectors")),
            "--cells" => o.cells = take_num(args, &mut i, "--cells").clamp(16, 8192) as usize,
            "--out" => o.out_path = Some(take_path(args, &mut i, "--out")),
            other => {
                eprintln!("error: opción desconocida {other}");
                usage();
            }
        }
        i += 1;
    }
    // Interacción --delay-ms / --latency : la banda superior
    // (T4) es SIEMPRE el umbral de retardo. Con --delay-ms se reemplaza T4;
    // sin él, T4 define el retardo. `--latency off` deja el umbral único sin bandas.
    if let Some(b) = o.bands {
        if delay_given {
            if o.delay_ms <= b[2] {
                eprintln!(
                    "error: --delay-ms {} debe superar la banda T3 ({} ms); sube el retardo, ajusta las bandas con --latency o desactiva la clasificación con --latency off",
                    o.delay_ms, b[2]
                );
                std::process::exit(2);
            }
            o.bands = Some([b[0], b[1], b[2], o.delay_ms]);
        } else {
            o.delay_ms = b[3];
        }
    }
    // por defecto el id de registros sigue al número de unidad
    // crudo (`--raw N` ⇒ DISK N), salvo `--id` explícito. Antes el orden de
    // los argumentos importaba y el menú no lo aplicaba (informe de campo:
    // rdisk2 escribía DISK1.LOG).
    if o.raw.is_some() && !o.id_explicit {
        o.id = o.raw.unwrap();
    }
    o
}

/// Nota de permisos solo cuando el error es de permiso (no para fallos de
/// ioctl/medio, que ya traen su propia causa).
fn perm_note(e: &std::io::Error) -> &'static str {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        " (se requiere root/administrador)"
    } else {
        ""
    }
}

/// Abre el destino según las opciones. Devuelve (etiqueta, ruta_raw opcional,
/// fuente). `ruta_raw` solo existe para destinos de dispositivo (necesaria
/// para smartctl).
pub(crate) fn open_target(
    o: &CommonOpts,
    write: bool,
) -> Result<(String, Option<PathBuf>, Box<dyn SectorSource>), (String, ExitCode)> {
    if let Some(n) = o.raw {
        let name = RawDisk::native_name(n);
        return match RawDisk::open(Path::new(&name), write) {
            Ok(s) => Ok((name.clone(), Some(PathBuf::from(&name)), Box::new(s))),
            Err(e) => Err((
                format!("error abriendo {name}: {e}{}", perm_note(&e)),
                ExitCode::from(1),
            )),
        };
    }
    if let Some(p) = &o.dev {
        return match RawDisk::open(p, write) {
            Ok(s) => Ok((p.display().to_string(), Some(p.clone()), Box::new(s))),
            Err(e) => Err((
                format!(
                    "error abriendo {}: {e}{}",
                    p.display(),
                    perm_note(&e)
                ),
                ExitCode::from(1),
            )),
        };
    }
    if let Some(p) = &o.image {
        let opened = if write {
            FileImage::open_rw(p)
        } else {
            FileImage::open_ro(p)
        };
        return match opened {
            Ok(s) => Ok((p.display().to_string(), None, Box::new(s))),
            Err(e) => Err((format!("error abriendo {}: {e}", p.display()), ExitCode::from(2))),
        };
    }
    Err(("error: falta la imagen o --raw/--dev".into(), ExitCode::from(2)))
}

/// Rango efectivo [start, end) validado contra el tamaño real.
fn resolve_range(src: &dyn SectorSource, o: &CommonOpts) -> Result<(u32, u32), ExitCode> {
    let total = src.sector_count();
    let total32 = total.min(u32::MAX as u64) as u32;
    let end = o.end.unwrap_or(total32).min(total32);
    if o.start >= end {
        eprintln!("error: rango vacío (start={} end={end}, total={total})", o.start);
        return Err(ExitCode::from(1));
    }
    Ok((o.start, end))
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args[0].as_str() {
        "--version" | "-V" | "version" => {
            println!("{}", version_line());
            ExitCode::SUCCESS
        }
        "menu" => menu::run(&parse_common(&args[1..])),
        "smart" => cli_smart(&args[1..]),
        "selftest" => cli_selftest(&args[1..]),
        "rescue" => cli_rescue(&parse_common(&args[1..])),
        "surfmap" => cli_surfmap(&parse_common(&args[1..])),
        "prescan" => cli_prescan(&parse_common(&args[1..])),
        "speedtest" => cli_speedtest(&parse_common(&args[1..])),
        "scan" => cli_pass(&parse_common(&args[1..]), RunMode::Scan),
        "repair" => cli_pass(&parse_common(&args[1..]), RunMode::Repair),
        "refresh" => cli_pass(&parse_common(&args[1..]), RunMode::Refresh),
        "regenerate" => cli_pass(&parse_common(&args[1..]), RunMode::Regenerate),
        _ => usage(),
    }
}

fn cli_smart(args: &[String]) -> ExitCode {
    let o = parse_common(args);
    let (_label, dev, _src) = match open_target(&o, false) {
        Ok(v) => v,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    let Some(dev) = dev else {
        eprintln!("error: smart requiere un dispositivo (--raw/--dev), no una imagen");
        return ExitCode::from(2);
    };
    match smart_query(&dev) {
        Ok(info) => print_status(&info, &dev.display().to_string()),
        Err(e) => eprintln!("error consultando SMART de {}: {e}", dev.display()),
    }
    ExitCode::SUCCESS
}

fn cli_selftest(args: &[String]) -> ExitCode {
    let (action, rest): (String, Vec<String>) = match args.first().map(|s| s.as_str()) {
        Some(a @ ("status" | "short" | "long" | "conveyance" | "abort")) => (a.to_string(), args[1..].to_vec()),
        _ => ("status".to_string(), args.to_vec()),
    };
    let o = parse_common(&rest);
    let Some(dev) = o
        .raw
        .map(|n| PathBuf::from(RawDisk::native_name(n)))
        .or_else(|| o.dev.clone())
    else {
        eprintln!("error: selftest requiere un dispositivo (--raw N | --dev PATH), no una imagen");
        return ExitCode::from(2);
    };
    if !smart::smart_available() {
        eprintln!("error: selftest requiere `smartctl` (paquete smartmontools)");
        return ExitCode::from(1);
    }
    match action.as_str() {
        "status" => match smart::selftest_log(&dev) {
            Ok(entries) => smart::print_selftests(&entries, &dev.display().to_string()),
            Err(e) => {
                eprintln!("error consultando el registro de self-tests: {e}{}", perm_note(&e));
                return ExitCode::from(1);
            }
        },
        "abort" => match smart::selftest_abort(&dev) {
            Ok(true) => println!("self-test abortado en {}", dev.display()),
            Ok(false) => {
                println!("smartctl rechazó el abort (¿no hay ninguna prueba en curso?)");
                return ExitCode::from(1);
            }
            Err(e) => {
                eprintln!("error abortando el self-test: {e}{}", perm_note(&e));
                return ExitCode::from(1);
            }
        },
        kind => match smart::selftest_start(&dev, kind) {
            Ok((true, text)) => {
                println!("self-test {kind} iniciado en {}", dev.display());
                for l in text.lines().filter(|l| l.contains("Estimated") || l.contains("Duration")).take(2) {
                    println!("  {l}");
                }
                println!("  la unidad lo ejecuta sola: no la muevas ni la duermas.");
                println!("  progreso: regrow selftest status --dev {}", dev.display());
            }
            Ok((false, text)) => {
                eprintln!("no se pudo iniciar el self-test {kind}:");
                for l in text.lines().take(4) {
                    eprintln!("  {l}");
                }
                return ExitCode::from(1);
            }
            Err(e) => {
                eprintln!("error iniciando el self-test: {e}{}", perm_note(&e));
                return ExitCode::from(1);
            }
        },
    }
    ExitCode::SUCCESS
}

fn cli_rescue(o: &CommonOpts) -> ExitCode {
    if o.raw.is_none() && o.dev.is_none() {
        eprintln!("error: rescue necesita un origen crudo (--raw N | --dev PATH); la ruta posicional es la imagen DESTINO");
        return ExitCode::from(2);
    }
    let Some(dest) = o.image.clone() else {
        eprintln!("error: rescue necesita la imagen destino (ruta posicional antes de las opciones)");
        return ExitCode::from(2);
    };
    let (display, _dev, mut src) = match open_target(o, false) {
        Ok(v) => v,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    let total = src.sector_count();
    let total32 = total.min(u32::MAX as u64) as u32;
    let end = o.end.unwrap_or(total32).min(total32);
    if o.start >= end {
        eprintln!("error: rango vacío (start={} end={end}, total={total})", o.start);
        return ExitCode::from(1);
    }

    let mut img = match rescue::prepare_image(&dest, total) {
        Ok(im) => im,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };

    let map_path = o.map_path.clone().unwrap_or_else(|| {
        let mut s = dest.to_string_lossy().into_owned();
        s.push_str(".map");
        PathBuf::from(s)
    });
    let mut map = if let Some(imp) = &o.import_map {
        match mapfile::MapFile::import_text(imp, total) {
            Ok(m) => {
                let (g, _, _, _) = m.stats();
                println!("mapa ajeno importado desde {}: {} sectores ya buenos NO se releerán", imp.display(), g);
                m
            }
            Err(e) => {
                eprintln!("error importando el mapfile: {e}");
                return ExitCode::from(2);
            }
        }
    } else if map_path.exists() {
        if o.resume {
            match mapfile::MapFile::load(&map_path) {
                Ok(m) => {
                    if m.sector_count != total {
                        eprintln!(
                            "error: el mapfile es de un dominio de {} sectores y el origen tiene {total}",
                            m.sector_count
                        );
                        return ExitCode::from(1);
                    }
                    let (g, _, _, _) = m.stats();
                    println!("reanudando desde {}: {} sectores buenos preservados", map_path.display(), g);
                    m
                }
                Err(e) => {
                    eprintln!("error leyendo el mapfile: {e}");
                    return ExitCode::from(1);
                }
            }
        } else {
            eprintln!(
                "aviso: existe un mapfile en {}; IGNORADO (añade --resume para continuarlo). Al terminar será sobrescrito.",
                map_path.display()
            );
            mapfile::MapFile::new(total)
        }
    } else {
        mapfile::MapFile::new(total)
    };

    println!(
        "{display}: {total} sectores; rescate [{}..{end}) → {} (mapfile {})",
        o.start,
        dest.display(),
        map_path.display()
    );
    let ropts = rescue::RescueOptions {
        start: o.start as u64,
        end: end as u64,
        block: o.block.max(1),
        retries: o.retries.max(1),
        reverse: o.reverse,
        save_every_ops: rescue::RESCUE_SAVE_EVERY_OPS,
    };
    let t0 = Instant::now();
    let mut last_line = Instant::now();
    let rep = match rescue::run_rescue(&mut *src, &mut img, &mut map, &ropts, &map_path, |p: &rescue::Progress| {
        if last_line.elapsed().as_secs() >= 30 {
            last_line = Instant::now();
            println!(
                "── [{}] sector {} · buenos {} pendientes {} · bloques {}/{} ok-err · {}",
                p.phase.label(),
                p.cur,
                p.good,
                p.pending,
                p.blocks_ok,
                p.block_errors,
                format_hms(t0.elapsed().as_secs())
            );
        }
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error durante el rescate: {e}");
            return ExitCode::from(1);
        }
    };
    if let Some(x) = &o.export_map {
        match map.export_text(x) {
            Ok(()) => println!("mapa de texto exportado a {}", x.display()),
            Err(e) => eprintln!("aviso: no se pudo exportar el mapa de texto: {e}"),
        }
    }
    println!("rescate terminado en {}", format_hms(rep.seconds));
    println!(
        "  buenos: {}  fallidos (se reintentan con --resume): {}  rascados: {}  sin mapear: {}",
        rep.good, rep.failed, rep.scraped, rep.untried
    );
    if rep.failed + rep.scraped + rep.untried > 0 {
        println!(
            "  quedan pendientes: repite con --resume (origen y destino idénticos; mapa {})",
            map_path.display()
        );
    }
    ExitCode::SUCCESS
}

fn cli_surfmap(o: &CommonOpts) -> ExitCode {
    let total = match o.sectors {
        Some(t) => t,
        None => match load_pending(&o.log_dir, o.id) {
            Ok(Some(cp)) => cp.end as u64,
            Ok(None) => {
                eprintln!(
                    "error: falta --sectors N y no hay checkpoint DISK{}.state del que tomar el rango",
                    o.id
                );
                return ExitCode::from(2);
            }
            Err(e) => {
                eprintln!("error leyendo el checkpoint: {e}");
                return ExitCode::from(1);
            }
        },
    };
    match surfmap::write_surfmap(&o.log_dir, o.id, total, o.cells, o.out_path.as_deref()) {
        Ok(p) => {
            println!("mapa de superficie escrito en {} (ábrelo en el navegador)", p.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn cli_prescan(o: &CommonOpts) -> ExitCode {
    let (label, _dev, mut src) = match open_target(o, false) {
        Ok(v) => v,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    let (start, end) = match resolve_range(&*src, o) {
        Ok(v) => v,
        Err(c) => return c,
    };
    println!(
        "{label}: {} sectores (medio: {}), barrido bulk [{start}..{end}) — prescan",
        src.sector_count(),
        src.media().label()
    );

    let mut last_bar = String::new();
    let mut hooks = RunHooks::empty();
    let t_start = Instant::now();
    // Umbrales coherentes con el motor: delay = banda T4 .
    let mut eo = EngineOptions::new(RunMode::Scan, start, end);
    eo.set_bands(o.bands);
    eo.delay = Duration::from_millis(o.delay_ms);
    let pre = prescan(
        &mut *src,
        &eo,
        &mut hooks,
        |cells: &[Cell; GRID_CELLS]| {
            let bar: String = cells.iter().map(|c| c.glyph()).collect();
            if bar != last_bar {
                last_bar = bar.clone();
                let _ = write!(std::io::stdout(), "\r|{bar}|");
                let _ = std::io::stdout().flush();
            }
        },
    );
    let _ = writeln!(std::io::stdout());

    let unreadable = pre.problems.iter().filter(|(_, k)| matches!(k, ProblemKind::Unreadable)).count();
    let slow = pre.problems.len() - unreadable;
    // Contaje por banda  .
    let mut bands = [0u64; 4];
    for (_, k) in &pre.problems {
        if let ProblemKind::Slow(b) = k {
            bands[(*b - 1) as usize] += 1;
        }
    }
    println!("Tiempo transcurrido: {}", format_hms(t_start.elapsed().as_secs()));
    println!(
        "{} bloques bulk limpios, {} bloques requirieron aislamiento",
        pre.bulk_chunks, pre.retried_chunks
    );
    println!("{} sectores ilegibles, {} sectores lentos", unreadable, slow);
    if o.bands.is_some() && slow > 0 {
        println!(
            "bandas: T1 {}  T2 {}  T3 {}  T4 {}",
            bands[0], bands[1], bands[2], bands[3]
        );
    }
    let show = pre.problems.len().min(20);
    for (sector, kind) in pre.problems.iter().take(show) {
        let k = match kind {
            ProblemKind::Unreadable => "ilegible".to_string(),
            ProblemKind::Slow(b) if o.bands.is_some() => format!("lento banda T{b}"),
            ProblemKind::Slow(_) => "lento".to_string(),
        };
        println!("  sector {sector}: {k}");
    }
    if pre.problems.len() > show {
        println!("  … ({} problemáticos más; el log queda detallado)", pre.problems.len() - show);
    }

    if !o.no_log {
        // El prescan registra ilegibles como B y, con bandas activas, los
        // lentos con su flag de banda T/U/V/X (igual que Scan.
        let mut runs = RunBuilder::default();
        for (sector, kind) in &pre.problems {
            match kind {
                ProblemKind::Unreadable => runs.push(*sector, Flag::B),
                ProblemKind::Slow(b) if o.bands.is_some() => runs.push(*sector, band_flag(*b)),
                ProblemKind::Slow(_) => {}
            }
        }
        let mut records = vec![Record { sector: start, flag: Flag::S, run: 1 }];
        records.extend(runs.finish());
        if !pre.aborted {
            records.push(Record { sector: end - 1, flag: Flag::L, run: 1 });
        }
        records.sort_by_key(|r| (r.sector, r.flag.char()));
        let unit = o.id;
        write_chain(o, unit, records, src.sector_count());
    }
    ExitCode::SUCCESS
}

fn cli_speedtest(o: &CommonOpts) -> ExitCode {
    let (label, _dev, mut src) = match open_target(o, false) {
        Ok(v) => v,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    let (start, end) = match resolve_range(&*src, o) {
        Ok(v) => v,
        Err(c) => return c,
    };
    println!(
        "{label}: {} sectores (medio: {}), lectura bulk [{start}..{end}) — speedtest ({} KiB/bloque)",
        src.sector_count(),
        src.media().label(),
        SPEED_CHUNK * 512 / 1024
    );
    let rep = run_speedtest(&mut *src, start, end, |zones| {
        let done: u64 = zones.iter().map(|_| SPEED_CHUNK as u64).sum();
        let _ = write!(
            std::io::stdout(),
            "\rleídos {} sectores",
            done.min((end - start) as u64)
        );
        let _ = std::io::stdout().flush();
    });
    let _ = writeln!(std::io::stdout());
    println!(
        "media: {:.1} MB/s en {} bloques ({} sectores), {} bloques con error",
        rep.avg_mb_per_s(),
        rep.chunk_secs.len(),
        rep.scanned,
        rep.errors
    );
    println!("zonas más lentas:");
    for z in rep.slowest(5) {
        println!("  sector {}: {:.3} s", z.start, z.secs);
    }
    ExitCode::SUCCESS
}

fn cli_pass(o0: &CommonOpts, mode: RunMode) -> ExitCode {
    // Checkpoint: ¿retomar? (un checkpoint solo reanuda su propio
    // modo y su rango debe seguir siendo válido en el disco que se abra).
    let mut o = o0.clone();
    let pending: Option<Checkpoint> = if o.resume {
        let sp = state_path(&o.log_dir, o.id);
        match load_pending(&o.log_dir, o.id) {
            Ok(Some(cp)) => Some(cp),
            Ok(None) => {
                eprintln!("no hay checkpoint en {}; la pasada empieza de cero", sp.display());
                None
            }
            Err(e) => {
                eprintln!("error leyendo checkpoint: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };
    if let Some(cp) = &pending {
        if cp.mode != mode {
            eprintln!(
                "error: el checkpoint pendiente es de una pasada {} ({}); reanúdala con ese modo o borra {}",
                mode_word(cp.mode),
                resume_note(cp),
                state_path(&o.log_dir, o.id).display()
            );
            return ExitCode::from(1);
        }
    }

    let write_mode = matches!(mode, RunMode::Repair | RunMode::Refresh | RunMode::Regenerate);
    let (display, dev_path, mut src) = match open_target(&o, write_mode) {
        Ok(v) => v,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return code;
        }
    };

    let mut prev: Option<Checkpoint> = None;
    if let Some(cp) = pending {
        match resume_plan(&cp, mode, src.sector_count()) {
            Some(plan) => {
                o.start = plan.start;
                o.end = Some(plan.end);
                o.merge = true; // retomar implica preservar el histórico
                println!(
                    "reanudando {}: {} sectores ya completados, continuación en {}..{}",
                    mode_word(mode),
                    plan.done,
                    plan.start,
                    plan.end
                );
                prev = Some(cp);
            }
            None => {
                eprintln!(
                    "error: el checkpoint ya no deja sectores pendientes en este disco ({}); borra {} para empezar de cero",
                    resume_note(&cp),
                    state_path(&o.log_dir, o.id).display()
                );
                return ExitCode::from(1);
            }
        }
    }
    let (start, end) = match resolve_range(&*src, &o) {
        Ok(v) => v,
        Err(c) => return c,
    };

    process_run(&mut *src, &display, dev_path.as_deref(), &o, start, end, mode, prev.as_ref())
}

/// Identidad + versión + licencia en pantalla .
/// Solo aparecen ReGrow, el autor y la licencia.
pub(crate) fn version_line() -> String {
    format!(
        "ReGrow {} — © 2026 TuckerDS · software libre bajo licencia GPL-3.0-or-later (sin garantía de uso; ver LICENSE)",
        env!("CARGO_PKG_VERSION")
    )
}

fn mode_word(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Scan => "ESCANEO",
        RunMode::Repair => "REPARACIÓN",
        RunMode::Refresh => "REFRESCO",
        RunMode::Regenerate => "REGENERACIÓN",
    }
}

/// Veredicto de la puerta de `regenerate` :
/// - HDD: permitido.
/// - SSD confirmado: BLOQUEADO salvo `--ssd-wear-ok` explícito (reescribir
///   páginas sanas consume 1 ciclo P/E de toda la NAND sin beneficio).
/// - SSD con flag: permitido con aviso (coste SMART).
/// - Desconocido: permitido con aviso (no se bloquea por detectores dudosos;
///   en el menú se pide confirmación explícita).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Regate {
    Allow,
    Warn,
    Block,
}

pub(crate) fn gate_regenerate(media: MediaKind, ssd_wear_ok: bool) -> Regate {
    match media {
        MediaKind::Hdd => Regate::Allow,
        MediaKind::Ssd if !ssd_wear_ok => Regate::Block,
        MediaKind::Ssd => Regate::Warn,
        MediaKind::Unknown => Regate::Warn,
    }
}

/// Coste de desgaste actual de un medio sólido (para el aviso del gate G).
fn wear_note(si: &smart::SmartInfo) -> String {
    let f = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "?".into());
    format!(
        "usado={} %  mediaErrors={}  wearLeveling={}",
        f(si.percentage_used),
        f(si.media_errors),
        f(si.wear_leveling)
    )
}

/// Severidad para agregar celdas al comprimir la barra (Bad manda sobre
/// Delay/Rec/Scan/Empty; las bandas de latencia se ordenan T1 < T2 < T3 < T4).
fn cell_rank(c: Cell) -> u8 {
    match c {
        Cell::Bad => 7,
        Cell::Delay => 6,
        Cell::Band3 => 5,
        Cell::Band2 => 4,
        Cell::Band1 => 3,
        Cell::Rec => 2,
        Cell::Scan => 1,
        Cell::Empty => 0,
    }
}

/// Barra comprimida a `bar_w` celdas: cada celda de salida agrega el grupo
/// de celdas de origen que representa y muestra la más severa.
fn fit_bar(cells: &[Cell; GRID_CELLS], bar_w: usize) -> String {
    let n = cells.len();
    if bar_w >= n {
        return cells.iter().map(|c| c.glyph()).collect();
    }
    (0..bar_w)
        .map(|i| {
            let lo = i * n / bar_w;
            let hi = ((i + 1) * n / bar_w).max(lo + 1);
            cells[lo..hi]
                .iter()
                .max_by_key(|c| cell_rank(**c))
                .copied()
                .unwrap_or(Cell::Empty)
                .glyph()
        })
        .collect()
}

/// Filas vivas del panel: barra (50 celdas o menos si la terminal
/// es estrecha) + % + ETA y contadores coloreados (dañados rojo, recuperados
/// cian, retardos ámbar). Compartidas por el pintado inicial y cada tick del
/// hook `ui`. **Toda fila cabe en el ancho de la terminal**: si una línea
/// envuelve, el borrado `ESC[nA` desalinea y el panel cascadearía (bug de
/// campo 2026-11-19); `clip` en `Paint::rows` es la red de seguridad.
fn live_rows(
    mode: RunMode,
    cells: &[Cell; GRID_CELLS],
    stats: &EngineStats,
    span: u64,
    elapsed: u64,
    paint: &tui::Paint,
) -> Vec<String> {
    let pct = stats.scanned.saturating_mul(100) / span.max(1);
    let eta = if stats.scanned > 0 {
        elapsed.saturating_mul(span.saturating_sub(stats.scanned)) / stats.scanned
    } else {
        0
    };
    // "|{bar}| {pct}%  restante HH:MM:SS": el espacio libre tras el prefijo
    // fijo decide el ancho de la barra (mínimo 10 celdas, máximo 50).
    let tail = format!("| {pct}%  restante {}", format_hms(eta));
    let bar_w = paint
        .width()
        .saturating_sub(tail.chars().count() + 1) // + la '|' inicial
        .min(GRID_CELLS)
        .max(10);
    let bar = fit_bar(cells, bar_w);
    let danados = stats.bad + stats.recovered;
    let linea_danados = format!("{danados} sectores dañados detectados");
    let ultima = if mode == RunMode::Scan {
        let bands = stats.slow1 + stats.slow2 + stats.slow3;
        let d = if bands > 0 {
            // Bandas: T1..T3 aparte; `delays` es la banda T4 (retardo).
            format!(
                "{} retardos  T1 {}  T2 {}  T3 {}",
                stats.delays, stats.slow1, stats.slow2, stats.slow3
            )
        } else {
            format!("{} retardos detectados", stats.delays)
        };
        if stats.delays > 0 || bands > 0 {
            paint.yellow(&d)
        } else {
            d
        }
    } else {
        let r = format!("{} sectores dañados recuperados", stats.recovered);
        if stats.recovered > 0 {
            paint.cyan(&r)
        } else {
            r
        }
    };
    vec![
        format!("|{bar}{tail}"),
        format!("{} sectores inspeccionados", stats.scanned),
        if danados > 0 {
            paint.red(&linea_danados)
        } else {
            linea_danados
        },
        ultima,
    ]
}

/// Escribe la cadena de registros completa (rotación + merge + disklist).
fn write_chain(o: &CommonOpts, unit: u32, records: Vec<Record>, sectors: u64) {
    let disk_line = format_disk_line(unit, sectors);
    let created = format!(
        "Created={}",
        format_asctime(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        )
    );
    let footer = DiskLog::make_footer(&disk_line, &[], &created);
    let mut new_log = DiskLog {
        records,
        raw_footer: footer,
    };

    let log_path = o.log_dir.join(format!("DISK{unit}.LOG"));
    if o.merge || o.resume {
        if let Ok(bytes) = fs::read(&log_path) {
            if let Ok(old) = DiskLog::parse(&bytes) {
                new_log = merge_logs(&old, &new_log);
            }
        }
    }
    if let Err(e) = rotate_log_chain(&log_path, 5) {
        eprintln!("error rotando cadena de logs: {e}");
        return;
    }
    if let Err(e) = write_log(&log_path, &new_log) {
        eprintln!("error escribiendo {}: {e}", log_path.display());
        return;
    }
    if let Err(e) = update_disklist(&o.log_dir.join("disklist.cfg"), unit, sectors) {
        eprintln!("error actualizando disklist.cfg: {e}");
        return;
    }
    println!("log escrito: {}", log_path.display());
}

/// Ejecuta el motor y produce resumen + cadena de registros + checkpoint.
/// Compartida por el CLI y el menú: una sola lógica motor+registros.
pub(crate) fn process_run(
    src: &mut dyn SectorSource,
    display: &str,
    dev_path: Option<&Path>,
    o: &CommonOpts,
    start: u32,
    end: u32,
    mode: RunMode,
    prev: Option<&Checkpoint>,
) -> ExitCode {
    let mut opts = EngineOptions::new(mode, start, end);
    opts.delay = Duration::from_millis(o.delay_ms);
    opts.bands = o.bands;
    opts.allow_data_loss = o.allow_data_loss;
    opts.prescan = o.prescan && mode != RunMode::Regenerate;

    // Los mensajes largos se envuelven al ancho REAL de la terminal: el
    // envoltado automático mezclándose con las filas vivas desalinea el
    // panel (bug de campo 2026-11-19, mensaje PASE ABORTADO de ~100 col).
    let paint = tui::paint();
    let live = paint.on;
    // Con terminal, los mensajes se terminan con \r\n: en modo raw (ONLCR
    // apagado por cfmakeraw) un \n pelado NO resetea la columna — escalera
    // (bug de campo 2026-11-19, resumen tras el panel). \r\n es correcto
    // también en modo cooked.
    let say = |s: String| {
        if paint.on {
            let text = if paint.cols > 0 {
                tui::wrap_text(&s, paint.cols)
            } else {
                s
            };
            let _ = write!(std::io::stdout(), "{text}\r\n");
            let _ = std::io::stdout().flush();
        } else {
            println!("{s}");
        }
    };
    // Instantánea SMART ANTES de la pasada : solo dispositivo con
    // smartctl; sirve para los deltas del final y, en SSD, para informar el
    // coste de desgaste antes de escrituras masivas.
    let smart_before = match (dev_path, smart::smart_available()) {
        (Some(p), true) => smart_query(p).ok().filter(|si| si.available),
        _ => None,
    };
    // Tipo de medio: la detección de la fuente, refinada por la `Rotation
    // Rate` de SMART cuando quedó Unknown .
    let media = match src.media() {
        MediaKind::Unknown => match &smart_before {
            Some(si) => smart::media_hint(si),
            None => MediaKind::Unknown,
        },
        m => m,
    };
    let phys = src.phys_block();

    say(format!(
        "{display}: {} sectores (medio: {}), rango [{start}..{end}) — modo {}",
        src.sector_count(),
        media.label(),
        mode_word(mode)
    ));
    if phys != 512 {
        say(format!(
            "bloques físicos de {phys} B (4Kn): acceso por bloque alineado; las escrituras preservan los sectores vecinos (lectura-modificación-escritura)"
        ));
    }

    // Puerta de regenerate : bloqueada en SSD salvo flag.
    if mode == RunMode::Regenerate {
        match gate_regenerate(media, o.ssd_wear_ok) {
            Regate::Block => {
                say(String::from(
                    "Bloqueado: el medio es sólido (SSD/NVMe). Regenerar páginas sanas consume ciclos P/E de toda la NAND (desgaste irreversible, sin beneficio magnético). Si aun así quieres continuar, añade --ssd-wear-ok.",
                ));
                return ExitCode::from(3);
            }
            Regate::Warn => {
                if media == MediaKind::Ssd {
                    match &smart_before {
                        Some(si) => say(format!(
                            "aviso: regenerar sobre SSD desgasta la NAND (--ssd-wear-ok aceptado). Coste actual: {}",
                            wear_note(si)
                        )),
                        None => say(String::from(
                            "aviso: regenerar sobre SSD desgasta la NAND sin beneficio (--ssd-wear-ok aceptado)",
                        )),
                    }
                } else {
                    say(String::from(
                        "aviso: no se pudo confirmar el tipo de medio; si fuera sólido, regenerar desgastaría la NAND sin beneficio",
                    ));
                }
            }
            Regate::Allow => {}
        }
    }

    // Respaldo preventivo.
    let mut backup = match &o.backup_dir {
        Some(d) => match FileBackup::open(d, o.id) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("error abriendo el respaldo en {}: {e}", d.display());
                return ExitCode::from(1);
            }
        },
        None => None,
    };

    // Monitor de temperatura (solo con --max-temp sobre dispositivo real).
    let mut guard = match (o.max_temp, dev_path) {
        (Some(t), Some(p)) => {
            if !smart::smart_available() {
                say(format!(
                    "aviso: --max-temp {t} °C solicitado, pero `smartctl` no está instalado; el control de temperatura queda inactivo"
                ));
            }
            Some(TemperatureGuard::new(p, t))
        }
        (Some(t), None) => {
            println!("aviso: --max-temp ignorado (SMART no disponible para una imagen de archivo; umbral {t} °C)");
            None
        }
        _ => None,
    };

    // Checkpoint automático en TODAS las pasadas : el progreso
    // vale igual en ESCANEO que en las de escritura, y cuesta una escritura de
    // ~250 B cada POLL_SECTORS (8192 sectores ≈ 4 MB leídos).
    let sp = state_path(&o.log_dir, o.id);
    let unit = o.id;

    let t_start = Instant::now();

    // Panel vivo: en tty reescribe 4 filas (barra+%, ETA, contadores
    // coloreados) sondeando ESC con poll(0) en cada tick; sin tty degrada a
    // la barra simple con \r (pipes/tests no ven códigos ANSI). El raw de
    // stdin dura solo la pasada: los menús de línea vuelven en modo cooked.
    // Orden corregido (bug de campo 2026-11-19): la cabecera se imprime ANTES
    // de entrar en raw, y el RawGuard se crea y destruye DENTRO del bloque de
    // la pasada, para que el resumen salga con el terminal ya restaurado
    // (cfmakeraw apaga ONLCR: imprimir con el raw activo escalona las filas).
    if live {
        if mode == RunMode::Regenerate {
            println!("{}", paint.green("Regenerando…"));
        }
        println!("{}", paint.yellow("ESC pausa o aborta el pase"));
        let rows = live_rows(
            mode,
            &[Cell::Empty; GRID_CELLS],
            &EngineStats::default(),
            (end - start) as u64,
            0,
            &paint,
        );
        paint.rows(0, &rows);
    }

    let report;
    {
        // El raw dura exactamente la pasada: el guard se suelta al cerrar este
        // bloque, antes del resumen (bug escalera 2026-11-19).
        let _raw = if live { tui::RawGuard::new() } else { None };
        let mut last_bar = String::new();
        // Sin tty (nohup, pipe, log): cada ~30 s se emite una LÍNEA de estado
        // legible con `tail -f` — sector exacto, %, contadores por banda,
        // tiempo y ETA. En tty no hace falta: el panel vivo ya lo pinta.
        let mut last_status = Instant::now();
        let mut progress = |sector: u32, stats: &EngineStats| {
            let cp = Checkpoint {
                mode,
                start,
                end,
                last_sector: sector,
                stats: prev
                    .map(|p| Checkpoint::merge_stats(p.stats, *stats))
                    .unwrap_or(*stats),
                unit,
            };
            if let Err(e) = cp.save(&sp) {
                eprintln!("aviso: no se pudo guardar el checkpoint: {e}");
            }
            if !live && last_status.elapsed().as_secs() >= 30 {
                last_status = Instant::now();
                let range = (end - start) as u64;
                let done = (sector - start) as u64;
                let el = t_start.elapsed().as_secs().max(1);
                let pct = 100.0 * done as f64 / range.max(1) as f64;
                let eta = if done > 0 {
                    format_hms((range - done) * el / done)
                } else {
                    "?".to_string()
                };
                println!(
                    "── sector {sector} ({pct:.2} %) · T{} U{} V{} X{} · ilegibles {} · {:.1} MB · transcurrido {} · ETA ~{eta}",
                    stats.slow1,
                    stats.slow2,
                    stats.slow3,
                    stats.delays,
                    stats.bad,
                    done as f64 * 512.0 / 1e6,
                    format_hms(el)
                );
            }
        };
        // Hook ui (panel vivo): sondea ESC (poll(0)) y reescribe las filas.
        // Su `false` aborta el pase; la llamada final de `progress` deja el
        // checkpoint en el sector exacto de la pausa (--resume).
        let mut ui = |cells: &[Cell; GRID_CELLS], stats: &EngineStats, _sector: u32| -> bool {
            if let Some(k) = tui::poll_key(0) {
                if k == tui::ESC {
                    return false;
                }
            }
            let rows = live_rows(
                mode,
                cells,
                stats,
                (end - start) as u64,
                t_start.elapsed().as_secs(),
                &paint,
            );
            let n = rows.len();
            paint.rows(n, &rows);
            true
        };
        let hooks = RunHooks {
            backup: backup.as_mut().map(|b| b as &mut dyn SectorBackup),
            monitor: guard.as_mut().map(|g| g as &mut dyn engine::HealthMonitor),
            progress: Some(&mut progress),
            ui: if live { Some(&mut ui) } else { None },
        };
        report = run(src, &opts, hooks, |cells: &[Cell; GRID_CELLS]| {
            if live {
                return; // la barra la pinta el panel vivo
            }
            let bar: String = cells.iter().map(|c| c.glyph()).collect();
            if bar != last_bar {
                last_bar = bar.clone();
                let _ = write!(std::io::stdout(), "\r|{bar}|");
                let _ = std::io::stdout().flush();
            }
        });
        // El RawGuard se suelta aquí: el resumen, la cadena de logs y los
        // hints posteriores se imprimen con el modo cooked restaurado.
    }
    if !live {
        // La barra degradada de pipe no termina en \n: cerrar la fila.
        let _ = writeln!(std::io::stdout());
    }

    // Resumen (los contadores del checkpoint previo se acumulan).
    let totals = match prev {
        Some(p) => Checkpoint::merge_stats(p.stats, report.stats),
        None => report.stats,
    };
    say(format!(
        "Tiempo transcurrido: {}",
        format_hms(t_start.elapsed().as_secs())
    ));
    say(format!("{} sectores inspeccionados", totals.scanned));
    say(format!(
        "{} sectores dañados detectados",
        totals.bad + totals.recovered
    ));
    say(format!("{} sectores dañados recuperados", totals.recovered));
    if mode == RunMode::Scan {
        say(format!("{} retardos detectados", totals.delays));
        if totals.slow1 + totals.slow2 + totals.slow3 > 0 {
            if let Some(b) = o.bands {
                say(format!(
                    "Lentos por banda: T1(>{} ms) {}  T2(>{} ms) {}  T3(>{} ms) {}",
                    b[0], totals.slow1, b[1], totals.slow2, b[2], totals.slow3
                ));
            }
        }
    }
    if totals.healthy_skipped > 0 {
        say(format!(
            "{} sectores sanos salteados (prescan)",
            totals.healthy_skipped
        ));
    }
    if totals.backed_up > 0 {
        say(format!(
            "{} sectores respaldados antes de reescribir",
            totals.backed_up
        ));
    }
    if totals.data_lost > 0 {
        say(format!(
            "aviso: {} sectores ilegibles reescritos con ceros (destructivo, --allow-data-loss)",
            totals.data_lost
        ));
    }
    if let Some(g) = &guard {
        if let Some(si) = &g.last {
            say(format!("SMART al final: {}", si.deltas_note()));
        }
    }
    // Deltas SMART de la pasada : primera instantánea → última.
    if let (Some(p), Some(before)) = (dev_path, &smart_before) {
        match smart_query(p) {
            Ok(after) => {
                if let Some(d) = smart::delta_note(before, &after) {
                    say(format!("SMART durante la pasada: {d}"));
                }
            }
            Err(_) => {}
        }
    }

    if report.aborted {
        // Los registros de una pasada interrumpida NO se pierden: se escribe
        // el log parcial a la cadena (S + runs parciales, SIN L — igual que
        // los registros históricos interrumpidos, que se iban escribiendo
        // incrementalmente). La reanudación con `--merge`/`--resume` fusiona
        // los flags sobre la cadena.
        if !o.no_log {
            write_chain(o, unit, report.records.clone(), src.sector_count());
        }
        say(format!(
            "PASE ABORTADO en sector {}. Guardado checkpoint: {} — reanuda con --resume o eligiendo esta unidad en el menú.",
            report.last_sector,
            sp.display()
        ));
        return ExitCode::from(1);
    }

    if !o.no_log {
        write_chain(o, unit, report.records.clone(), src.sector_count());
    }

    // Pasada completa: el checkpoint ya no hace falta.
    Checkpoint::remove(&sp);

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::{Flag, Record};

    fn opts(dir: &Path, id: u32, merge: bool) -> CommonOpts {
        CommonOpts {
            image: None,
            raw: None,
            dev: None,
            start: 0,
            end: None,
            id,
            log_dir: dir.to_path_buf(),
            id_explicit: false,
            no_log: false,
            merge,
            delay_ms: 500,
            bands: Some([5, 20, 50, 500]),
            allow_data_loss: false,
            backup_dir: None,
            prescan: false,
            resume: false,
            max_temp: None,
            ssd_wear_ok: false,
            block: rescue::RESCUE_BLOCK_DEFAULT,
            retries: rescue::RESCUE_RETRIES_DEFAULT,
            reverse: true,
            map_path: None,
            export_map: None,
            import_map: None,
            sectors: None,
            cells: surfmap::SURFMAP_DEFAULT_CELLS,
            out_path: None,
        }
    }

    /// Puerta de regenerate : SSD bloqueado sin flag; con flag,
    /// aviso; desconocido solo aviso; HDD libre.
    #[test]
    fn gate_regenerate_ssd() {
        assert_eq!(gate_regenerate(MediaKind::Hdd, false), Regate::Allow);
        assert_eq!(gate_regenerate(MediaKind::Hdd, true), Regate::Allow);
        assert_eq!(gate_regenerate(MediaKind::Ssd, false), Regate::Block);
        assert_eq!(gate_regenerate(MediaKind::Ssd, true), Regate::Warn);
        assert_eq!(gate_regenerate(MediaKind::Unknown, false), Regate::Warn);
    }

    /// título, autor y licencia deben verse en pantalla.
    #[test]
    fn version_line_muestra_version_y_licencia() {
        let v = version_line();
        assert!(v.starts_with("ReGrow "), "{v}");
        assert!(v.contains("TuckerDS"), "{v}");
        assert!(v.contains(env!("CARGO_PKG_VERSION")), "{v}");
        assert!(v.contains("GPL-3.0-or-later"), "{v}");
    }

    /// --latency parsea cuatro umbrales crecientes; 'off'
    /// desactiva la clasificación; --delay-ms fija la banda superior (T4).
    #[test]
    fn parse_latencia_bandas() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let o = parse_common(&s(&["scan"]));
        assert_eq!(o.bands, Some([5, 20, 50, 200]));
        assert_eq!(o.delay_ms, 200);
        let o = parse_common(&s(&["scan", "--latency", "10,30,70,300"]));
        assert_eq!(o.bands, Some([10, 30, 70, 300]));
        assert_eq!(o.delay_ms, 300); // T4 es el umbral de retardo
        let o = parse_common(&s(&["scan", "--latency", "off"]));
        assert_eq!(o.bands, None);
        assert_eq!(o.delay_ms, 200);
        let o = parse_common(&s(&["scan", "--delay-ms", "500"]));
        assert_eq!(o.bands, Some([5, 20, 50, 500])); // --delay-ms reemplaza T4
    }

    /// el id de registros sigue al número de unidad crudo
    /// (`--raw 2` ⇒ DISK 2) salvo `--id` explícito, sin importar el orden.
    #[test]
    fn parse_id_por_defecto_sigue_a_raw() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let o = parse_common(&s(&["scan", "--raw", "2"]));
        assert_eq!((o.id, o.id_explicit), (2, false));
        // --id explícito gana aunque vaya antes que --raw.
        let o = parse_common(&s(&["scan", "--id", "1", "--raw", "2"]));
        assert_eq!((o.id, o.id_explicit), (1, true));
        // Imagen sin --raw: id 1 por defecto.
        let o = parse_common(&s(&["img.raw"]));
        assert_eq!((o.id, o.id_explicit), (1, false));
    }

    /// La rama de abort del process_run vuelca los records parciales a la
    /// cadena (S + runs, SIN L). Test directo sobre write_chain simulando
    /// las dos pasadas (interrumpida → reanudada con merge).
    #[test]
    fn log_parcial_en_abort_se_conserva_y_el_merge_lo_recupera() {
        let dir = std::env::temp_dir().join(format!("regrow_partial_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // Pasada 1 interrumpida en el sector 40: S(0) y B(10)x2 ya detectados.
        let o = opts(&dir, 5, false);
        write_chain(
            &o,
            5,
            vec![
                Record {
                    sector: 0,
                    flag: Flag::S,
                    run: 1,
                },
                Record {
                    sector: 10,
                    flag: Flag::B,
                    run: 2,
                },
            ],
            100,
        );
        let t = fs::read_to_string(dir.join("DISK5.LOG")).unwrap();
        assert!(t.contains("0=S1"), "log parcial: {t}");
        assert!(t.contains("10=B2"), "flag conservado: {t}");
        assert!(!t.contains("=L1"), "pasada interrumpida: sin L: {t}");
        assert!(dir.join("disklist.cfg").exists());

        // Pasada 2 reanudada (merge): span 50..100 completo.
        let o2 = CommonOpts {
            merge: true,
            ..o.clone()
        };
        write_chain(
            &o2,
            5,
            vec![
                Record {
                    sector: 50,
                    flag: Flag::S,
                    run: 1,
                },
                Record {
                    sector: 99,
                    flag: Flag::L,
                    run: 1,
                },
            ],
            100,
        );
        let t2 = fs::read_to_string(dir.join("DISK5.LOG")).unwrap();
        assert!(t2.contains("10=B2"), "merge conserva el flag de la pasada cortada: {t2}");
        // el span fusionado es el HISTÓRICO — el reanudar en 50
        // no borra el hecho de que el disco se empezó a escanear en 0.
        assert!(t2.contains("0=S1"), "span histórico preservado: {t2}");
        assert!(!t2.contains("50=S1"), "no se duplica el borde S: {t2}");
        assert!(t2.contains("99=L1"));
        // La rotación en cascada conserva el log parcial previo en .001.
        let t001 = fs::read_to_string(dir.join("DISK5.001")).unwrap();
        assert!(t001.contains("10=B2"));
        fs::remove_dir_all(&dir).ok();
    }

    /// Los números ioctl se DERIVAN de su codificación oficial
    /// (_IOR en BSD / _IO en Linux), no de transcribir constantes a mano.
    #[test]
    fn tiocgwinsz_derivada_no_transcrita() {
        // BSD/Darwin: _IOR('t', 104, struct winsize) → 0x40087468.
        assert_eq!(tui::_ior(b't', 104, 8), 0x4008_7468);
        // Linux: _IO('T', 0x13) → 0x5413.
        assert_eq!(tui::_io(b'T', 0x13), 0x5413);
        // El winsize real hace sizeof() = 8 (4×u16) en macOS y Linux.
        assert_eq!(core::mem::size_of::<std::mem::MaybeUninit<[u16; 4]>>(), 8);
    }

    /// Panel en terminal estrecha: NINGUNA fila puede superar el ancho
    /// (si envuelve, el ESC[nA desalinea y cascadearía — bug de campo).
    #[test]
    fn live_rows_caben_en_terminal_estrecha() {
        let paint = tui::Paint {
            on: false,
            cols: 72,
        };
        let stats = EngineStats {
            scanned: 489833,
            bad: 3,
            recovered: 1,
            delays: 2,
            ..Default::default()
        };
        let rows = live_rows(
            RunMode::Scan,
            &[Cell::Scan; GRID_CELLS],
            &stats,
            15_974_400,
            4500,
            &paint,
        );
        for r in &rows {
            assert!(
                r.chars().count() <= paint.width(),
                "fila de {} cols no cabe en {}: {r}",
                r.chars().count(),
                paint.width()
            );
        }
        // Con 72 cols la barra debe comprimirse por debajo de 50 celdas.
        let bar_len = rows[0].split('|').nth(1).unwrap().chars().count();
        assert!(bar_len < GRID_CELLS, "barra no comprimida: {}", rows[0]);
        // En terminal ancha la barra conserva las 50 celdas.
        let wide = tui::Paint {
            on: false,
            cols: 200,
        };
        let rows_w = live_rows(
            RunMode::Scan,
            &[Cell::Scan; GRID_CELLS],
            &stats,
            15_974_400,
            4500,
            &wide,
        );
        assert!(rows_w[0].starts_with(&format!("|{}", "\u{2588}".repeat(GRID_CELLS))));
    }

    /// `fit_bar` agrega la celda más severa del segmento que representa.
    #[test]
    fn fit_bar_preserva_los_marcadores_severos() {
        let mut cells = [Cell::Empty; GRID_CELLS];
        for c in cells.iter_mut() {
            *c = Cell::Scan;
        }
        // Bad y Delay en segmentos distintos de la barra comprimida (10
        // celdas = 5 celdas de origen por celda).
        cells[20] = Cell::Bad;
        cells[27] = Cell::Delay;
        let bar = fit_bar(&cells, 10);
        assert_eq!(bar.chars().count(), 10);
        assert!(bar.contains('B'), "se perdió el Bad: {bar}");
        assert!(bar.contains('D'), "se perdió el Delay: {bar}");
        // El Bad manda sobre un Scan en su mismo segmento.
        let mut cells2 = [Cell::Scan; GRID_CELLS];
        cells2[23] = Cell::Bad;
        assert!(fit_bar(&cells2, 10).contains('B'));
    }

    #[test]
    fn clip_cuenta_columnas_visibles_no_escapes() {
        let s = tui::clip("\x1b[31m12345678\x1b[0m", 4);
        assert_eq!(s, "\x1b[31m1234\x1b[0m");
        assert_eq!(tui::clip("abcd", 10), "abcd");
    }

    #[test]
    fn wrap_text_reparte_mensajes_largos() {
        let msg = "PASE ABORTADO en sector 489833 — registros parciales \
                   escritos; reanuda con --start 489834 --merge.";
        let w = tui::wrap_text(msg, 72);
        for l in w.lines() {
            assert!(l.chars().count() <= 71, "línea de {}: {l}", l.chars().count());
        }
        assert!(w.replace('\n', " ").contains("--merge."));
    }
}
