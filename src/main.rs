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

mod ata;
mod backup;
mod config;
mod engine;
mod fill;
mod fsops;
mod log;
mod mapfile;
mod mapops;
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
use source::{disk_key, mounted_overlap, FileImage, MediaKind, RawDisk, SectorSource};
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
    pub retry_wait: u64,             // segundos de descanso entre reintentos/bloque fallido
    pub reverse: bool,               // pasada inversa (activa por defecto)
    pub map_path: Option<PathBuf>,   // mapfile propio (def: <imagen-destino>.map)
    pub backup_file: Option<PathBuf>, // sidecar de respaldo (def: <imagen-destino>.respaldo)
    pub no_backup: bool,             // desactivar el respaldo (no recomendado)
    pub export_map: Option<PathBuf>, // exportar además el mapa en texto
    pub import_map: Option<PathBuf>, // continuar desde un mapfile de texto ajeno
    pub fill_from: Option<PathBuf>,  // (rescue) relleno: donante = segunda copia del origen
    pub donor_map: Option<PathBuf>,  // (rescue) mapfile del donante (propio o de texto)
    pub allow_zeros: bool,           // (rescue/fill) escribir también sectores todo-cero del donante
    pub rate_limit: Option<u64>,     // (rescue) techo de bytes/s copiados a la imagen
    pub min_read_rate: rescue::MinRate, // (rescue) umbral de zona lenta en la fase directa
    pub max_error_rate: Option<u64>, // (rescue) aborto si los intentos fallidos superan esta tasa
    pub requeue: bool,               // (rescue) los tramos agotados vuelven a pendientes
    pub preview: Option<usize>,      // (rescue) avance hex+ASCII en el estado (líneas)
    pub log_rates: Option<PathBuf>,  // (rescue) registro de ritmo apto para gráficas
    // Clonado (clone): el origen explícito; el destino es --raw/--dev.
    pub from: Option<PathBuf>,
    // Verificación directa del firmware (prescan/speedtest):
    pub verify: bool,                // --verify: comprobar sin transferir datos
    pub timeout_secs: u64,           // --timeout N: plazo por comando directo (s)
    pub seek: bool,                  // (speedtest) modo seek: lecturas aleatorias cronometradas
    // Rescate: variantes ddrescue (U8).
    pub generate_map: bool,          // (rescue) -G: mapa aproximado desde la imagen destino
    pub skip_adaptive: bool,         // (rescue/clone) -K: salto geométrico tras bloques fallidos
    pub yes: bool,                   // --yes: segunda confirmación no interactiva (destructivos)
    // Mapa de superficie (surfmap):
    pub sectors: Option<u64>, // dominio; si falta se toma del checkpoint
    pub cells: usize,         // resolución del mapa (def 1024)
    pub out_path: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "uso: regrow                    abre el menú interactivo (arranque pelado)\n\
         \x20 regrow <scan|repair|refresh|regenerate|testwrite|prescan|speedtest> <imagen> [opciones]\n\
         \x20 regrow <…> --raw N [opciones]\n\
         \x20 regrow <…> --dev PATH [opciones]\n\
         \x20 regrow smart --raw N | --dev PATH\n\
         \x20 regrow selftest <status|short|long|conveyance|abort> --raw N | --dev PATH\n\
         \x20 regrow sanitize <block-erase|block-erase-enhanced|crypto-erase|overwrite> --raw N | --dev PATH\n\
         \x20                    saneado completo del medio físico, DELEGADO a las herramientas\n\
         \x20                    del sistema (puede tardar horas; irreversible). Exige\n\
         \x20                    --allow-data-loss y segunda confirmación: escribir SANEAR\n\
         \x20                    cuando se pida, o --yes en uso no interactivo. Jamás sobre\n\
         \x20                    un disco montado. En macOS/Windows documenta la delegación.\n\
         \x20 regrow clone --from PATH --raw N | --dev PATH [opciones]\n\
         \x20                    clonar sector a sector (forma ddrescue): el origen (--from, imagen\n\
         \x20                    .raw o nodo crudo) se copia al DESTINO crudo con las fases y el\n\
         \x20                    mapfile reanudable del rescate. irreversible sobre el destino:\n\
         \x20                    exige --allow-data-loss + --no-backup (sin sidecar en un disco)\n\
         \x20                    + segunda confirmación (escribir CLONAR, o --yes). Nunca sobre el\n\
         \x20                    mismo disco ni sobre uno montado. --verify aquí relee y\n\
         \x20                    contrastea lo escrito; --resume reanuda por el mapa.\n\
         \x20 regrow rescue <imagen-destino> --raw N | --dev PATH [opciones]\n\
         \x20 regrow surfmap --id N [--log-dir D] [--sectors N] [--cells N] [--out F]\n\
         \x20 regrow maps <status|done|diff|merge|invert|complete|retype|shift|annotate|list|from-list|import|export>\n\
         \x20                    operaciones entre mapfiles que nunca tocan discos ni imágenes:\n\
         \x20                    fusionar lo terminado de dos pasadas (--mode or|and|xor),\n\
         \x20                    contrastar mapas (diff; sale 1 si difieren), resumen (status),\n\
         \x20                    test de rescate completo (done; sale 0 si está todo),\n\
         \x20                    anotar un mapa como texto comentado (annotate), y\n\
         \x20                    importar/exportar listas estilo badblocks y bitmaps.\n\
         \x20                    `regrow maps` a secas describe cada orden.\n\
         \x20 regrow fs <status|ls|report> <imagen|--raw N|--dev PATH> [--map M] [--partition N]\n\
         \x20                    capa lógica de SOLO LECTURA: firmas y layout (MBR/GPT,\n\
         \x20                    NTFS/FAT32) con estado según el mapa, listado de la $MFT sin\n\
         \x20                    montar, e informe JSONL de los metadatos que mueren en cada\n\
         \x20                    tramo pendiente. Nunca escribe en medios ni imágenes.\n\
         \x20                    `regrow fs` a secas describe cada orden.\n\
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
         \x20 --verify           (prescan/speedtest) comprobar sectores sin transferir\n\
         \x20                    su contenido (lectura de verificación del firmware).\n\
         \x20                    Requiere nodo de disco ENTERO y soporte de la punta;\n\
         \x20                    si el medio no lo expone degrada a lecturas normales\n\
         \x20                    (nunca marca eso como sector dañado)\n\
         \x20 --timeout S        plazo en segundos de cada comando directo (def 30)\n\
         \x20 --seek             (speedtest) modo sondeo de posicionamiento: 256 lecturas de un\n\
         \x20                    sector en posiciones aleatorias del rango, cronometradas una a\n\
         \x20                    una (media/mediana/p95 en ms; posiciones reproducibles). Solo\n\
         \x20                    lectura; muestra la rotación del medio desde SMART cuando está\n\
         \x20                    disponible.\n\
         \x20 --resume           retomar la pasada interrumpida desde DISKn.state\n\
         \x20                    (en el menú se ofrece solo si la unidad tiene una)\n\
         \x20 --max-temp C       pausar la pasada (scan/repair/refresh/rescue) si el disco\n\
         \x20                    supera C °C (usa smartctl)\n\
         \x20 --ssd-wear-ok      permitir regenerate sobre medio sólido (SSD/NVMe):\n\
         \x20                    reescribir páginas sanas desgasta la NAND sin beneficio\n\
         \x20 testwrite          battery multi-patrón (00/AA/55/FF + restauración verificada):\n\
         \x20                    quirúrgica con --import-map (exige --backup o --allow-data-loss);\n\
         \x20                    en rango completo exige --backup o --allow-data-loss; nunca sobre\n\
         \x20                    un dispositivo montado. Flags nuevos: W escritura mala, M marginal\n\
         \x20 --block N          (rescue) sectores por bloque de la fase directa (def 128 = 64 KiB)\n\
         \x20 --retry N          (rescue) tentativas por sector fallido en el rascado (def 3)\n\
         \x20 --retry-wait S     (rescue) segundos de descanso tras un fallo LABORIOSO\n\
         \x20                    (el medio tardó 1 s o más en fallar): el cabezal agradece\n\
         \x20                    pausar (def 2; 0 = sin pausas). Un error inmediato del\n\
         \x20                    firmware no provoca pausa: dormir tras cada bloque muerto\n\
         \x20                    solo añade horas sin proteger nada\n\
         \x20 --backup-file R    (rescue) sidecar donde respaldar cada sector antes de\n\
         \x20                    sobrescribirlo (def: <imagen>.respaldo)\n\
         \x20 --no-backup        (rescue) desactivar el respaldo — no recomendado\n\
         \x20 --rate-limit B     (rescue) techo de bytes/s copiados a la imagen (sufijos\n\
         \x20                    K/M; 'off'); al superarlo duerme hasta cerrar la ventana —\n\
         \x20                    para convivir con una máquina en producción\n\
         \x20 --min-read-rate N  (rescue) en la fase directa, un bloque que se lea más\n\
         \x20                    despacio de N bytes/s (o 'auto' = décima parte del ritmo\n\
         \x20                    medio) NO se copia en ese momento: queda pendiente y lo\n\
         \x20                    retrabajan las fases lentas, que aceptan lecturas lentas\n\
         \x20 --max-error-rate B (rescue) detener con el mapfile guardado si los intentos\n\
         \x20                    fallidos superan B bytes/s sostenidos\n\
         \x20 --requeue          (rescue) al reanudar, los tramos que el mapa daba por\n\
         \x20                    agotados vuelven a pendientes para otra pasada entera\n\
         \x20 --preview [N]      (rescue) avanzar en pantalla (1..32 líneas, def 3) el\n\
         \x20                    hex+ASCII del último sector leído bien\n\
         \x20 --log-rates RUTA   (rescue) registrar el ritmo en un archivo: una línea\n\
         \x20                    por estado, columnas numéricas aptas para gráficas\n\
         \x20 --generate-map     (rescue) herencias de dd (estilo -G): deducir el mapa de la\n\
         \x20                    imagen destino existente — todo-cero = pendiente (se relee del\n\
         \x20                    origen), no-cero = bueno (no se releerá). Exige imagen existente;\n\
         \x20                    no combina con --resume ni --import-map\n\
         \x20 --skip-adaptive    (rescue/clone) salto adaptativo (estilo -K): tras 4 bloques\n\
         \x20                    fallidos seguidos la fase directa salta distancias crecientes\n\
         \x20                    en vez de martillar el clúster; lo saltado lo retrabajan las\n\
         \x20                    fases lentas\n\
         \x20 --no-reverse       (rescue) desactivar la pasada inversa final\n\
         \x20 --map F            mapfile propio: en rescue el de la imagen\n\
          \x20                    destino (def: <imagen-destino>.map); en las pasadas\n\
          \x20                    con --import-map, el mapa de resultados\n\
          \x20                    (def: <mapfile-ajeno>.regrow.map)\n\
         \x20 --export-map F     (rescue) exportar además el mapa en texto compatible\n\
         \x20 --import-map F     (rescue) continuar desde un mapfile de texto ajeno\n\
         \x20                    (rescue: la ruta posicional es la IMAGEN DESTINO sparse;\n\
         \x20                    el origen es SIEMPRE --raw/--dev; --resume retoma no linealmente)\n\
          \x20                    (scan/repair/refresh/regenerate/testwrite) PASADA QUIRÚRGICA: la\n\
          \x20                    pasada recorre SOLO los tramos que ese mapa deja\n\
          \x20                    pendientes (lo bueno no se lee ni se escribe) y anota lo\n\
          \x20                    reparado en --map; los tramos quedan en DISKn.ranges y\n\
          \x20                    --resume los retoma sin volver a dar el mapfile\n\
         \x20 --from PATH        (clone) origen del clonado: imagen .raw o nodo de dispositivo\n\
         \x20 --fill-from F      (rescue) modo relleno: completar los huecos pendientes de la\n\
         \x20                    imagen desde una SEGUNDA COPIA del origen (donante, archivo).\n\
         \x20                    No necesita --raw/--dev; los `+` de la imagen jamás se tocan;\n\
         \x20                    sin --donor-map no se escriben sectores todo-cero del donante\n\
         \x20 --donor-map F      (rescue/fill) mapfile del donante (propio o texto estilo\n\
         \x20                    ddrescue): solo se copia lo que su mapa marca como bueno\n\
         \x20 --allow-zeros      (rescue/fill) copiar también los sectores todo-cero del donante\n\
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

/// Tasa en bytes/s con sufijos: `500K`, `2M`, `1048576`; `off`/`0` desactiva.
fn parse_rate_str(name: &str, v: &str) -> Option<u64> {
    let v = v.trim().to_lowercase();
    if v == "off" || v == "0" {
        return None;
    }
    let (num, mul) = match v.chars().last() {
        Some('k') => (&v[..v.len() - 1], 1024u64),
        Some('m') => (&v[..v.len() - 1], 1024 * 1024),
        _ => (&v[..], 1u64),
    };
    match num.parse::<u64>() {
        Ok(x) if x > 0 => Some(x.saturating_mul(mul)),
        _ => {
            eprintln!("error: valor inválido para {name}: {v} (bytes/s, sufijos K o M, u 'off')");
            std::process::exit(2);
        }
    }
}

fn take_rate(args: &[String], i: &mut usize, name: &str) -> Option<u64> {
    *i += 1;
    if *i >= args.len() {
        eprintln!("error: {name} necesita valor");
        std::process::exit(2);
    }
    parse_rate_str(name, &args[*i])
}

/// `--min-read-rate auto|N[|K|M]|off`: política de zona lenta de la fase directa.
fn take_min_rate(args: &[String], i: &mut usize) -> rescue::MinRate {
    *i += 1;
    if *i >= args.len() {
        eprintln!("error: --min-read-rate necesita valor ('auto', bytes/s con sufijos K/M, u 'off')");
        std::process::exit(2);
    }
    let v = args[*i].trim().to_lowercase();
    if v == "auto" {
        return rescue::MinRate::Auto;
    }
    match parse_rate_str("--min-read-rate", &v) {
        None => rescue::MinRate::Off,
        Some(r) => rescue::MinRate::Bytes(r),
    }
}

/// Avance hex+ASCII del primer sector del último bloque leído bien
/// (`--preview`), con el estilo del `-P` de ddrescue.
fn hex_preview(lba: u64, data: &[u8], lines: usize) -> String {
    let lines = lines.clamp(1, 32);
    let mut s = format!("  visto en el sector {lba}:\n");
    for (k, chunk) in data.chunks(16).take(lines).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
            .collect();
        s.push_str(&format!("    {:04x}  {:<47}  |{}|\n", k * 16, hex.join(" "), ascii));
    }
    s
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
        retry_wait: 2,
        reverse: true,
        map_path: None,
        backup_file: None,
        no_backup: false,
        export_map: None,
        import_map: None,
        fill_from: None,
        donor_map: None,
        allow_zeros: false,
        rate_limit: None,
        min_read_rate: rescue::MinRate::Off,
        max_error_rate: None,
        requeue: false,
        preview: None,
        log_rates: None,
        from: None,
        verify: false,
        timeout_secs: 30,
        seek: false,
        generate_map: false,
        skip_adaptive: false,
        yes: false,
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
            "--backup" => o.backup_dir = Some(take_path(args, &mut i, "--backup")),            "--no-log" => o.no_log = true,
            "--merge" => o.merge = true,
            "--allow-data-loss" => o.allow_data_loss = true,
            "--prescan" => o.prescan = true,
            "--resume" => o.resume = true,
            "--ssd-wear-ok" => o.ssd_wear_ok = true,
            "--block" => o.block = take_num(args, &mut i, "--block").clamp(1, 65536) as usize,
            "--retry" => o.retries = take_num(args, &mut i, "--retry").clamp(1, 255) as u8,
            "--retry-wait" => o.retry_wait = take_num(args, &mut i, "--retry-wait").clamp(0, 3600),
            "--rate-limit" => o.rate_limit = take_rate(args, &mut i, "--rate-limit"),
            "--min-read-rate" => o.min_read_rate = take_min_rate(args, &mut i),
            "--max-error-rate" => o.max_error_rate = take_rate(args, &mut i, "--max-error-rate"),
            "--requeue" => o.requeue = true,
            "--preview" => {
                let n = if i + 1 < args.len()
                    && !args[i + 1].starts_with("--")
                    && args[i + 1].parse::<u64>().is_ok()
                {
                    take_num(args, &mut i, "--preview")
                } else {
                    3
                };
                o.preview = Some(n.clamp(1, 32) as usize);
            }
            "--log-rates" => o.log_rates = Some(take_path(args, &mut i, "--log-rates")),
            "--from" => o.from = Some(take_path(args, &mut i, "--from")),
            "--verify" => o.verify = true,
            "--timeout" => o.timeout_secs = take_num(args, &mut i, "--timeout").clamp(1, 3600),
            "--seek" => o.seek = true,
            "--generate-map" => o.generate_map = true,
            "--skip-adaptive" => o.skip_adaptive = true,
            "--yes" => o.yes = true,
            "--no-reverse" => o.reverse = false,
            "--map" => o.map_path = Some(take_path(args, &mut i, "--map")),
            "--backup-file" => o.backup_file = Some(take_path(args, &mut i, "--backup-file")),
            "--no-backup" => o.no_backup = true,
            "--export-map" => o.export_map = Some(take_path(args, &mut i, "--export-map")),
            "--import-map" => o.import_map = Some(take_path(args, &mut i, "--import-map")),
            "--fill-from" => o.fill_from = Some(take_path(args, &mut i, "--fill-from")),
            "--donor-map" => o.donor_map = Some(take_path(args, &mut i, "--donor-map")),
            "--allow-zeros" => o.allow_zeros = true,
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
    // Ctrl+C / kill abortan la pasada de forma limpia (registro parcial +
    // checkpoint), no matan el proceso dejando el progreso obsoleto.
    tui::install_signals();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        // Arranque pelado: abre el menú interactivo (la herramienta se maneja
        // principalmente por él; equivale a `regrow menu`). La ayuda para
        // scripting o modo por lotes sigue en `regrow --help`.
        println!("{}", version_line());
        return menu::run(&parse_common(&[]));
    }
    match args[0].as_str() {
        "--version" | "-V" | "version" => {
            println!("{}", version_line());
            ExitCode::SUCCESS
        }
        "menu" => menu::run(&parse_common(&args[1..])),
        "smart" => cli_smart(&args[1..]),
        "selftest" => cli_selftest(&args[1..]),
        "sanitize" => cli_sanitize(&args[1..]),
        "rescue" => cli_rescue(&parse_common(&args[1..])),
        "clone" => cli_clone(&parse_common(&args[1..])),
        "surfmap" => cli_surfmap(&parse_common(&args[1..])),
        "maps" => mapops::cli_maps(&args[1..]),
        "fs" => fsops::cli_fs(&args[1..]),
        "prescan" => cli_prescan(&parse_common(&args[1..])),
        "speedtest" => cli_speedtest(&parse_common(&args[1..])),
        "scan" => cli_pass(&parse_common(&args[1..]), RunMode::Scan),
        "repair" => cli_pass(&parse_common(&args[1..]), RunMode::Repair),
        "refresh" => cli_pass(&parse_common(&args[1..]), RunMode::Refresh),
        "regenerate" => cli_pass(&parse_common(&args[1..]), RunMode::Regenerate),
        "testwrite" => cli_pass(&parse_common(&args[1..]), RunMode::TestWrite),
        _ => usage(),
    }
}

/// Forense ATA de solo lectura (IDENTIFY + READ NATIVE MAX(48)) para las
/// cabeceras. `compacto`: una línea (scan/repair); detallado: bloque completo
/// (verbo smart). Un None significa «este nodo no cuenta», no «el disco está mal».
pub(crate) fn ata_forensics_lines(src: &dyn SectorSource, compacto: bool) -> Vec<String> {
    let Some(f) = src.ata_forensics() else {
        return Vec::new();
    };
    let id = &f.identity;
    let model = if id.model.is_empty() { "desconocido" } else { &id.model };
    let user = crate::ata::user_sectors(id);
    let nat_total = f.native_max.map(|m| m + 1);
    let hpa_txt = match (nat_total, user, f.hpa_active) {
        (Some(n), Some(u), Some(true)) => format!(
            "¡HPA ACTIVO! el medio atiende hasta {n} pero expone {u} ({} sectores ocultos)",
            f.hpa_hidden.unwrap_or(0)
        ),
        (Some(n), Some(u), _) => format!("user {u} sectores / nativo {n} sectores"),
        (Some(n), None, Some(true)) => format!("nativo {n} sectores (área user no declarada)"),
        (Some(n), None, _) => format!("nativo {n} sectores"),
        (None, Some(u), _) => format!("user {u} sectores / máximo nativo no reportado"),
        (None, None, _) => String::from("capacidades no reportadas"),
    };
    let mut feats: Vec<&str> = Vec::new();
    if id.hpa_supported {
        feats.push("HPA");
    }
    if id.dco_supported {
        feats.push("DCO");
    }
    if id.aam_supported {
        feats.push("AAM");
    }
    if id.apm_supported {
        feats.push("APM");
    }
    if compacto {
        return vec![format!(
            "forense ATA (solo lectura): {model} · {hpa_txt}{}",
            if feats.is_empty() {
                String::new()
            } else {
                format!(" · conjuntos {}", feats.join("/"))
            }
        )];
    }
    let mut out = vec![format!(
        "identificación ATA directa: modelo={} firmware={}",
        model,
        if id.firmware.is_empty() { "desconocido" } else { &id.firmware }
    )];
    out.push(format!("  capacidad: {hpa_txt}"));
    if !feats.is_empty() {
        out.push(format!("  conjuntos de características declarados: {}", feats.join(", ")));
    }
    if let Some(a) = id.aam_recommended {
        out.push(format!(
            "  nivel acústico recomendado por el fabricante: {a} (regrow NO ajusta AAM/APM: eso lo delega a smartctl)"
        ));
    }
    out.push(String::from(
        "  regrow no modifica HPA/DCO ni el configuration overlay: forense puro de lectura",
    ));
    out
}

fn cli_smart(args: &[String]) -> ExitCode {
    let o = parse_common(args);
    let (_label, dev, src) = match open_target(&o, false) {
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
    let smart_ok = match smart_query(&dev) {
        Ok(info) => {
            print_status(&info, &dev.display().to_string());
            true
        }
        Err(e) => {
            eprintln!("error consultando SMART de {}: {e}", dev.display());
            false
        }
    };
    let forense = ata_forensics_lines(&*src, false);
    if !forense.is_empty() {
        for line in forense {
            println!("{line}");
        }
    } else if !smart_ok {
        eprintln!(
            "y sin SMART tampoco hay paso ATA directo disponible para identificar {}: instale smartctl o use el nodo completo del disco (no una partición).",
            dev.display()
        );
        return ExitCode::from(1);
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

/// Saneado completo del medio físico, **delegado** a las herramientas del
/// sistema: no reinventamos el mecanismo del firmware — smartctl (primera
/// opción) o hdparm lo invocan con sus garantías. Doble confirmación
/// obligatoria (`--allow-data-loss` + teclear SANEAR, o `--yes` en uso no
/// interactivo) y jamás sobre un disco que participa en un montaje. En macOS
/// y Windows la delegación queda documentada, no ejecutada.
fn cli_sanitize(args: &[String]) -> ExitCode {
    let (mode, rest): (String, Vec<String>) = match args.first().map(|s| s.as_str()) {
        Some(m @ ("block-erase" | "block-erase-enhanced" | "crypto-erase" | "overwrite")) => {
            (m.to_string(), args[1..].to_vec())
        }
        _ => {
            eprintln!(
                "error: sanitize pide un modo: block-erase | block-erase-enhanced | crypto-erase | overwrite"
            );
            return ExitCode::from(2);
        }
    };
    let o = parse_common(&rest);
    let dev: PathBuf = match (o.dev.clone(), o.raw.map(RawDisk::native_name)) {
        (Some(p), _) => p,
        (None, Some(n)) => PathBuf::from(n),
        (None, None) => {
            eprintln!(
                "error: sanitize requiere --dev PATH o --raw N (una imagen no se sanea: se borra el archivo)"
            );
            return ExitCode::from(2);
        }
    };
    if let Some(mp) = mounted_overlap(&dev) {
        eprintln!(
            "Bloqueado: {} participa en el montaje {}: no se sanea un disco en uso. Desmóntalo primero.",
            dev.display(),
            mp
        );
        return ExitCode::from(3);
    }
    if !o.allow_data_loss {
        eprintln!(
            "Bloqueado: sanitize destruye TODO el contenido del medio físico (tablas de particiones incluidas). Si de verdad quieres hacerlo, añade --allow-data-loss y prepara la segunda confirmación."
        );
        return ExitCode::from(3);
    }
    if o.yes {
        // Uso no interactivo: la doble autorización son las dos banderas.
        eprintln!(
            "sanitize {mode} sobre {}: irreversible, puede tardar horas. --yes + --allow-data-loss aceptados.",
            dev.display()
        );
    } else {
        println!(
            "Vas a sanear {} (modo {mode}): irreversible, borra todo el medio físico y puede tardar horas.",
            dev.display()
        );
        println!("Escribe SANEAR y pulsa intro (cualquier otra cosa aborta):");
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                eprintln!("sin terminal no se puede pedir la confirmación tecleada: usa --yes junto con --allow-data-loss.");
                return ExitCode::from(3);
            }
            Ok(_) if line.trim() == "SANEAR" => {}
            Ok(_) => {
                eprintln!("confirmación no recibida: nada se saneó, abortado.");
                return ExitCode::from(3);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let exists = |cmd: &str| {
            std::process::Command::new(cmd).arg("--version").output().is_ok()
        };
        let (tool, tool_args): (String, Vec<String>) = if smart::smart_available() {
            (
                "smartctl".to_string(),
                vec![format!("--sanitize={mode}"), "--yes".to_string(), dev.display().to_string()],
            )
        } else if exists("hdparm") {
            let flag = match mode.as_str() {
                "block-erase" => "--sanitize-block-erase",
                "block-erase-enhanced" => "--sanitize-block-erase-enhanced",
                "crypto-erase" => "--sanitize-crypto-sector",
                "overwrite" => {
                    eprintln!("error: el modo overwrite requiere smartctl (hdparm no lo ofrece).");
                    return ExitCode::from(2);
                }
                _ => unreachable!(),
            };
            (
                "hdparm".to_string(),
                vec![flag.to_string(), "--yes".to_string(), dev.display().to_string()],
            )
        } else {
            eprintln!(
                "error: ni smartctl ni hdparm disponibles: no hay a quién delegar el saneado ATA."
            );
            return ExitCode::from(1);
        };
        eprintln!(
            "delegando en {tool} (modo {mode}) sobre {}… no interrumpas el proceso ni duermas la unidad.",
            dev.display()
        );
        match std::process::Command::new(&tool).args(&tool_args).status() {
            Ok(st) if st.success() => {
                println!("saneado delegado a {tool}: completado. Comprueba el resultado con regrow smart --dev {}", dev.display());
                ExitCode::SUCCESS
            }
            Ok(st) => {
                eprintln!(
                    "{tool} terminó con código {}: el saneado NO se completó (¿medio congelado? hay que desbloquearlo antes — eso también queda fuera de regrow).",
                    st.code().unwrap_or(-1)
                );
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("no se pudo ejecutar {tool}: {e}{}", perm_note(&e));
                ExitCode::from(1)
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (mode, dev);
        eprintln!(
            "en macOS el saneado queda delegado a la herramienta del fabricante del medio, o a diskutil secureErase 0 <disco> (una pasada de reescritura aleatoria; no es el mecanismo nativo ATA). regrow no la invoca."
        );
        ExitCode::from(3)
    }
    #[cfg(windows)]
    {
        let _ = (mode, dev);
        eprintln!(
            "en Windows el saneado ATA queda delegado a la herramienta del fabricante del medio; regrow no la invoca."
        );
        ExitCode::from(3)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (mode, dev);
        eprintln!("este SO no tiene una herramienta delegable de saneado ATA documentada.");
        ExitCode::from(3)
    }
}

fn cli_rescue(o: &CommonOpts) -> ExitCode {
    if o.fill_from.is_some() {
        return cli_fill(o);
    }
    if o.raw.is_none() && o.dev.is_none() {
        eprintln!("error: rescue necesita un origen crudo (--raw N | --dev PATH); la ruta posicional es la imagen DESTINO");
        return ExitCode::from(2);
    }
    let Some(dest) = o.image.clone() else {
        eprintln!("error: rescue necesita la imagen destino (ruta posicional antes de las opciones)");
        return ExitCode::from(2);
    };
    if o.generate_map {
        if o.resume {
            eprintln!("error: --generate-map construye el mapa desde cero: no combina con --resume (elige uno).");
            return ExitCode::from(2);
        }
        if o.import_map.is_some() {
            eprintln!("error: --generate-map construye el mapa desde la imagen destino; para un mapa ajeno usa --import-map (elige uno).");
            return ExitCode::from(2);
        }
        if !dest.exists() {
            eprintln!("error: --generate-map necesita una imagen destino EXISTENTE de la que deducir el mapa (herencias de dd). Para crear la imagen desde cero, omite el flag.");
            return ExitCode::from(2);
        }
    }
    let (display, dev, mut src) = match open_target(o, false) {
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
    let mut map = if o.generate_map {
        match rescue::generate_map_from_image(&mut img) {
            Ok(m) => {
                let (g, f, s, u) = m.stats();
                println!(
                    "mapa generado desde la imagen destino (estilo -G): {} sectores no-cero declarados buenos (NO se releerán del origen), {} todo-cero quedan pendientes",
                    g,
                    f + s + u
                );
                println!("aviso: si un cero era dato legítimo se releerá del origen (nada se pierde); si un no-cero era basura de otra herramienta queda preservado como bueno — usa esto solo con imágenes de rescate propias o copias fiables");
                if map_path.exists() {
                    println!(
                        "aviso: existe un mapfile en {}; --generate-map lo reconstruye desde la imagen y lo sobrescribirá",
                        map_path.display()
                    );
                }
                m
            }
            Err(e) => {
                eprintln!("error generando el mapa desde la imagen: {e}");
                return ExitCode::from(1);
            }
        }
    } else if let Some(imp) = &o.import_map {
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

    if o.requeue {
        let n = map.requeue_scraped();
        println!(
            "requeue: {} sectores que el mapa daba por agotados vuelven a pendientes",
            n
        );
    }
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
        retry_wait: Duration::from_secs(o.retry_wait),
        max_temp: o.max_temp,
        device: dev.clone(),
        backup: if o.no_backup {
            None
        } else {
            Some(o.backup_file.clone().unwrap_or_else(|| {
                let mut s = dest.to_string_lossy().into_owned();
                s.push_str(".respaldo");
                PathBuf::from(s)
            }))
        },
        rate_limit: o.rate_limit,
        min_read_rate: o.min_read_rate,
        max_error_rate: o.max_error_rate,
        preview_lines: o.preview.unwrap_or(0),
        verify_dest: false, // rescue escribe en una imagen de archivo
        skip_adaptive: o.skip_adaptive,
    };
    match &ropts.backup {
        Some(p) => println!(
            "respaldo sector a sector: cada sector de la imagen se guarda en {} ANTES de sobrescribirlo",
            p.display()
        ),
        None => println!("aviso: rescate SIN respaldo sector a sector (--no-backup) — lo pedido por el autor es respaldar siempre"),
    }
    if o.skip_adaptive {
        println!("salto adaptativo activado: tras 4 bloques fallidos seguidos la fase directa salta distancias crecientes; lo saltado lo retrabajan las fases lentas (el cabezal deja de martillar el clúster)");
    }
    let mut rates_log: Option<fs::File> = o.log_rates.as_ref().and_then(|p| {
        match fs::OpenOptions::new().create(true).append(true).open(p) {
            Ok(mut f) => {
                if f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                    let _ = writeln!(f, "# segundos sector buenos pendientes bloques_ok bloques_err KiBps");
                }
                Some(f)
            }
            Err(e) => {
                eprintln!("aviso: no podré escribir el registro de tasas {}: {e}", p.display());
                None
            }
        }
    });
    let t0 = Instant::now();
    let mut last_line = Instant::now();
    let rep = match rescue::run_rescue(&mut *src, &mut img, &mut map, &ropts, &map_path, |p: &rescue::Progress| {
        if last_line.elapsed().as_secs() >= 30 {
            last_line = Instant::now();
            let rate_kibs =
                p.copied_bytes as f64 / (t0.elapsed().as_secs().max(1) as f64) / 1024.0;
            println!(
                "── [{}] sector {} · buenos {} pendientes {} · bloques {}/{} ok-err · {:.0} KiB/s{} · {}",
                p.phase.label(),
                p.cur,
                p.good,
                p.pending,
                p.blocks_ok,
                p.block_errors,
                rate_kibs,
                if p.slow_deferred > 0 { format!(" · lentos {}", p.slow_deferred) } else { String::new() },
                format_hms(t0.elapsed().as_secs())
            );
            if let Some((lba, data)) = &p.preview {
                print!("{}", hex_preview(*lba, data, o.preview.unwrap_or(3)));
            }
            if let Some(f) = &mut rates_log {
                let _ = writeln!(
                    f,
                    "{} {} {} {} {} {} {:.1}",
                    t0.elapsed().as_secs(),
                    p.cur,
                    p.good,
                    p.pending,
                    p.blocks_ok,
                    p.block_errors,
                    rate_kibs
                );
                let _ = f.flush();
            }
        }
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error durante el rescate: {e}");
            if e.kind() == std::io::ErrorKind::Interrupted {
                eprintln!(
                    "  el mapa quedó guardado en {}; repite con --resume para continuar",
                    map_path.display()
                );
            }
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
    if rep.slow_deferred > 0 {
        println!(
            "  bloques lentos diferidos por --min-read-rate en la fase directa: {}",
            rep.slow_deferred
        );
    }
    if rep.adaptive_leaps > 0 {
        println!(
            "  saltos adaptativos en la fase directa (--skip-adaptive): {}",
            rep.adaptive_leaps
        );
    }
    if rep.failed + rep.scraped + rep.untried > 0 {
        println!(
            "  quedan pendientes: repite con --resume (origen y destino idénticos; mapa {})",
            map_path.display()
        );
    }
    ExitCode::SUCCESS
}

/// Abre el origen de `clone`: un nodo de bloque se abre como disco crudo de
/// lectura; un archivo normal, como imagen. En Windows solo el prefijo
/// `\\.\` designa un dispositivo.
fn open_clone_source(path: &Path) -> Result<(Box<dyn SectorSource>, Option<PathBuf>), String> {
    #[cfg(windows)]
    let is_device = path.display().to_string().starts_with("\\\\.\\");
    #[cfg(not(windows))]
    let is_device = {
        use std::os::unix::fs::FileTypeExt;
        matches!(fs::metadata(path), Ok(m) if m.file_type().is_block_device())
    };
    if is_device {
        return match RawDisk::open(path, false) {
            Ok(s) => Ok((Box::new(s), Some(path.to_path_buf()))),
            Err(e) => Err(format!(
                "error abriendo {}: {e}{}",
                path.display(),
                perm_note(&e)
            )),
        };
    }
    match FileImage::open_ro(path) {
        Ok(s) => Ok((Box::new(s), None)),
        Err(e) => Err(format!("error abriendo {}: {e}", path.display())),
    }
}

/// ¿Origen y destino puntean al mismo disco físico? Mismo nodo tras enlaces,
/// o una partición del disco origen: en ambos casos «clonar» sería una
/// autocopia destructiva. Solo se compara entre dispositivos (una imagen
/// jamás es «el mismo disco» que un nodo).
fn same_device(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| disk_key(&p.to_string_lossy());
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb || key(&ca) == key(&cb),
        _ => key(a) == key(b),
    }
}

/// `clone`: copia sector a sector un ORIGEN (imagen o disco crudo) a un
/// DESTINO crudo con las mismas fases y el mapfile reanudable del rescate —
/// la forma ddrescue de «devolver los datos a un disco» (p. ej. volcar la
/// imagen rescatada sobre un disco de reemplazo, o disco a disco).
/// El destino NO recibe sidecar: respaldar lo que había en el destino es
/// trabajo de `rescue`. Doble confirmación destructiva obligatoria.
fn cli_clone(o: &CommonOpts) -> ExitCode {
    let Some(from) = o.from.clone() else {
        eprintln!("error: clone necesita el origen: --from PATH (imagen .raw o nodo de dispositivo)");
        return ExitCode::from(2);
    };
    if o.raw.is_none() && o.dev.is_none() {
        eprintln!("error: clone necesita el destino crudo (--raw N | --dev PATH). Para clonar a un ARCHIVO usa rescue.");
        return ExitCode::from(2);
    }
    if o.image.is_some() {
        eprintln!("error: en clone la ruta posicional no es el destino; el destino se da con --raw/--dev (¿querías rescue?).");
        return ExitCode::from(2);
    }
    if o.backup_file.is_some() || !o.no_backup {
        eprintln!(
            "error: clone no respalda el contenido previo del destino (no hay sidecar en un disco): para eso existe rescue.\n\
             \x20 Si quieres el respaldo, rescata primero el destino a una imagen; si lo aceptas vacío, añade --no-backup."
        );
        return ExitCode::from(2);
    }
    let (mut src, src_dev) = match open_clone_source(&from) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    };
    let total = src.sector_count();
    let total32 = total.min(u32::MAX as u64) as u32;
    let end = o.end.unwrap_or(total32).min(total32);
    if o.start >= end {
        eprintln!("error: rango vacío (start={} end={end}, origen={total})", o.start);
        return ExitCode::from(1);
    }

    // Puerta de identidad ANTES de abrir nada en escritura.
    let dst_name = o
        .dev
        .clone()
        .unwrap_or_else(|| PathBuf::from(RawDisk::native_name(o.raw.unwrap_or_default())));
    if same_device(&from, &dst_name) {
        eprintln!(
            "Bloqueado: el origen ({}) y el destino ({}) son el mismo disco físico (o el destino es una partición del origen). Clonar así destruiría los datos que aún no se copiaron.",
            from.display(),
            dst_name.display()
        );
        return ExitCode::from(2);
    }
    if disk_key(&dst_name.to_string_lossy()).starts_with("loop") {
        eprintln!(
            "aviso: el destino es un dispositivo de bucle; clone no puede saber si está respaldado por un archivo. Asegúrate de que su archivo backing no es el propio origen."
        );
    }

    // Destino: disco crudo en escritura (la capa aplica la puerta de montaje,
    // O_EXCL y la lectura de solo-lectura si el nodo está protegido).
    let (dst_label, dst_dev, mut dst) = match open_target(&o, true) {
        Ok(v) => v,
        Err((msg, code)) => {
            eprintln!("{msg}");
            return code;
        }
    };
    // Puerta de tamaño: los offsets del clonado son los LBAs del origen; el
    // destino debe cubrir el rango (un destino más pequeño que el origen solo
    // vale recortando con --end).
    if dst.sector_count() < end as u64 {
        eprintln!(
            "error: el destino {} tiene {} sectores y el rango llega a {end}; usa --end {} o menos, o un destino mayor.",
            dst_label,
            dst.sector_count(),
            dst.sector_count()
        );
        return ExitCode::from(2);
    }

    // Confirmación destructiva doble (misma filosofía que sanitize).
    if !o.allow_data_loss {
        eprintln!(
            "Bloqueado: clone sobrescribe TODO el contenido del destino {} hasta el sector {end}. Si de verdad quieres hacerlo, añade --allow-data-loss y prepara la segunda confirmación."
        ,
            dst_label
        );
        return ExitCode::from(3);
    }
    if o.yes {
        eprintln!(
            "clone sobre {dst_label}: irreversible. --yes + --allow-data-loss aceptados.",
        );
    } else {
        println!(
            "Vas a clonar {} → {dst_label} (rango [{}..{end})): irreversible, sobrescribe el destino. Escribe CLONAR y pulsa intro (cualquier otra cosa aborta):",
            from.display(),
            o.start
        );
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                eprintln!("sin terminal no se puede pedir la confirmación tecleada: usa --yes junto con --allow-data-loss.");
                return ExitCode::from(3);
            }
            Ok(_) if line.trim() == "CLONAR" => {}
            Ok(_) => {
                eprintln!("confirmación no recibida: nada se clonó, abortado.");
                return ExitCode::from(3);
            }
        }
    }

    // Mapfile: dominio = sectores del origen; reanudación igual que rescue.
    let map_path = o.map_path.clone().unwrap_or_else(|| {
        let name = from
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "origen".to_string());
        PathBuf::from(format!("{name}.clone.map"))
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
                    println!("reanudando desde {}: {} sectores ya clonados preservados", map_path.display(), g);
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
    if o.requeue {
        let n = map.requeue_scraped();
        println!(
            "requeue: {} sectores que el mapa daba por agotados vuelven a pendientes",
            n
        );
    }

    println!(
        "{} → {dst_label}: clonado [{}..{end}) sobre {} sectores de origen (destino: {} sectores, medio: {}){}",
        from.display(),
        o.start,
        total,
        dst.sector_count(),
        src.media().label(),
        if dst.sector_count() < total {
            format!(" — el destino es más pequeño: {} sectores del final del origen quedan fuera", total - dst.sector_count())
        } else {
            String::new()
        }
    );
    if o.verify {
        println!("--verify en clone: cada bloque se relee del destino y se compara con lo escrito (contraste); un destino que miente aborta con el mapa guardado");
    }
    for line in ata_forensics_lines(&*dst, true) {
        println!("{line}");
    }
    println!("sin respaldo del destino (documentado): lo que había en {dst_label} se pierde; para conservarlo, rescátalo antes a una imagen");
    if o.skip_adaptive {
        println!("salto adaptativo activado: tras 4 bloques fallidos seguidos la fase directa salta distancias crecientes; lo saltado lo retrabajan las fases lentas");
    }

    let ropts = rescue::RescueOptions {
        start: o.start as u64,
        end: end as u64,
        block: o.block.max(1),
        retries: o.retries.max(1),
        reverse: o.reverse,
        save_every_ops: rescue::RESCUE_SAVE_EVERY_OPS,
        retry_wait: Duration::from_secs(o.retry_wait),
        max_temp: o.max_temp,
        device: src_dev.or_else(|| dst_dev.clone()),
        backup: None,
        rate_limit: o.rate_limit,
        min_read_rate: o.min_read_rate,
        max_error_rate: o.max_error_rate,
        preview_lines: o.preview.unwrap_or(0),
        verify_dest: o.verify,
        skip_adaptive: o.skip_adaptive,
    };
    let mut rates_log: Option<fs::File> = o.log_rates.as_ref().and_then(|p| {
        match fs::OpenOptions::new().create(true).append(true).open(p) {
            Ok(mut f) => {
                if f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                    let _ = writeln!(f, "# segundos sector buenos pendientes bloques_ok bloques_err KiBps");
                }
                Some(f)
            }
            Err(e) => {
                eprintln!("aviso: no podré escribir el registro de tasas {}: {e}", p.display());
                None
            }
        }
    });
    let t0 = Instant::now();
    let mut last_line = Instant::now();
    let rep = match rescue::run_rescue(&mut *src, &mut *dst, &mut map, &ropts, &map_path, |p: &rescue::Progress| {
        if last_line.elapsed().as_secs() >= 30 {
            last_line = Instant::now();
            let rate_kibs =
                p.copied_bytes as f64 / (t0.elapsed().as_secs().max(1) as f64) / 1024.0;
            println!(
                "── [{}] sector {} · buenos {} pendientes {} · bloques {}/{} ok-err · {:.0} KiB/s{} · {}",
                p.phase.label(),
                p.cur,
                p.good,
                p.pending,
                p.blocks_ok,
                p.block_errors,
                rate_kibs,
                if p.slow_deferred > 0 { format!(" · lentos {}", p.slow_deferred) } else { String::new() },
                format_hms(t0.elapsed().as_secs())
            );
            if let Some(f) = &mut rates_log {
                let _ = writeln!(
                    f,
                    "{} {} {} {} {} {} {:.1}",
                    t0.elapsed().as_secs(),
                    p.cur,
                    p.good,
                    p.pending,
                    p.blocks_ok,
                    p.block_errors,
                    rate_kibs
                );
                let _ = f.flush();
            }
        }
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error durante el clonado: {e}");
            if e.kind() == std::io::ErrorKind::Interrupted {
                eprintln!(
                    "  el mapa quedó guardado en {}; repite con --resume para continuar (mismos origen y destino)",
                    map_path.display()
                );
            }
            return ExitCode::from(1);
        }
    };
    if let Some(x) = &o.export_map {
        match map.export_text(x) {
            Ok(()) => println!("mapa de texto exportado a {}", x.display()),
            Err(e) => eprintln!("aviso: no se pudo exportar el mapa de texto: {e}"),
        }
    }
    println!("clonado terminado en {}", format_hms(rep.seconds));
    println!(
        "  clonados: {}  ilegibles en el origen (quedan sin copiar): {}  rascados: {}  sin reintentar aún: {}",
        rep.good, rep.failed, rep.scraped, rep.untried
    );
    if rep.failed + rep.scraped + rep.untried > 0 {
        if rep.adaptive_leaps > 0 {
            println!(
                "  saltos adaptativos en la fase directa (--skip-adaptive): {}",
                rep.adaptive_leaps
            );
        }
        println!(
            "  quedan pendientes: repite con --resume (mapa {}). Un origen con ilegibles NO puede clonarse byte a byte: compara el resultado solo en los sectores buenos del mapa.",
            map_path.display()
        );
    }
    ExitCode::SUCCESS
}

/// `rescue --fill-from`: relleno aditivo de los huecos pendientes de una
/// imagen desde una segunda copia del origen (el «donante»). No destructivo
/// sobre el disco: son dos archivos. El dominio lo fija la imagen existente
/// (o `--sectors` para crearla sparse). La imagen jamás recibe un byte que
/// no venga del donante, y solo en posiciones pendientes del mapa destino.
fn cli_fill(o: &CommonOpts) -> ExitCode {
    let Some(donor_path) = o.fill_from.clone() else {
        return ExitCode::from(2);
    };
    let Some(dest) = o.image.clone() else {
        eprintln!("error: el modo relleno necesita la imagen destino (ruta posicional antes de las opciones)");
        return ExitCode::from(2);
    };
    let sec = source::SECTOR_SIZE as u64;
    let total: u64 = if dest.exists() {
        match fs::metadata(&dest) {
            Ok(m) if m.len() % sec == 0 => m.len() / sec,
            Ok(m) => {
                eprintln!("error: la imagen {} tiene {} bytes, que no es múltiplo de 512", dest.display(), m.len());
                return ExitCode::from(1);
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        match o.sectors {
            Some(n) if n > 0 => n,
            _ => {
                eprintln!("error: la imagen destino no existe; indica --sectors N para crearla sparse antes del relleno");
                return ExitCode::from(2);
            }
        }
    };
    let mut img = match rescue::prepare_image(&dest, total) {
        Ok(im) => im,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let mut donor = match FileImage::open_ro(&donor_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: no se puede abrir el donante {}: {e}", donor_path.display());
            return ExitCode::from(2);
        }
    };
    let donor_total = donor.sector_count();

    // Mapa destino: import de texto ajeno, el propio existente (el relleno es
    // aditivo por diseño: se respeta siempre), o todo pendiente (imagen nueva).
    let map_path = o.map_path.clone().unwrap_or_else(|| {
        let mut s = dest.to_string_lossy().into_owned();
        s.push_str(".map");
        PathBuf::from(s)
    });
    let mut map = if let Some(imp) = &o.import_map {
        match mapfile::MapFile::import_text(imp, total) {
            Ok(m) => {
                let (g, f, s, u) = m.stats();
                println!("mapa ajeno importado desde {}: {} buenos · {} pendientes", imp.display(), g, f + s + u);
                m
            }
            Err(e) => {
                eprintln!("error importando el mapfile: {e}");
                return ExitCode::from(2);
            }
        }
    } else if map_path.exists() {
        match mapfile::MapFile::load(&map_path) {
            Ok(m) => {
                if m.sector_count != total {
                    eprintln!(
                        "error: el mapfile es de un dominio de {} sectores y la imagen tiene {total}",
                        m.sector_count
                    );
                    return ExitCode::from(1);
                }
                let (g, f, s, u) = m.stats();
                println!("usando el mapfile {}: {} buenos preservados · {} pendientes", map_path.display(), g, f + s + u);
                m
            }
            Err(e) => {
                eprintln!("error leyendo el mapfile: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        mapfile::MapFile::new(total)
    };

    // Mapa del donante: binario propio (RGROWMP1) o de texto estilo ddrescue.
    let dmap = match &o.donor_map {
        Some(dp) => match mapfile::MapFile::load(dp)
            .or_else(|_| mapfile::MapFile::import_text(dp, total))
        {
            Ok(m) => {
                let (g, _, _, _) = m.stats();
                println!("mapa del donante {}: {} sectores certificables", dp.display(), g);
                Some(m)
            }
            Err(e) => {
                eprintln!("error leyendo el mapfile del donante: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };

    let total32 = total.min(u32::MAX as u64) as u32;
    let start = o.start;
    let end = o.end.unwrap_or(total32).min(total32);
    if start >= end {
        eprintln!("error: rango vacío (start={start} end={end}, total={total32})");
        return ExitCode::from(1);
    }

    println!(
        "relleno: {} ({total} sectores) ← donante {} ({} sectores) · ventana [{start}..{end}) · mapa {}",
        dest.display(),
        donor_path.display(),
        donor_total,
        map_path.display()
    );
    if donor_total < total {
        println!("aviso: el donante llega hasta el sector {donor_total}; los pendientes más allá no podrán rellenarse desde él");
    }
    if dmap.is_none() {
        println!(
            "sin mapa del donante: se filtran sus sectores todo-cero (un hueco y un cero son indistinguibles en un archivo); usa --donor-map para certificar, o --allow-zeros para escribirlos igualmente"
        );
    }
    let backup_path = if o.no_backup {
        None
    } else {
        Some(o.backup_file.clone().unwrap_or_else(|| {
            let mut s = dest.to_string_lossy().into_owned();
            s.push_str(".respaldo");
            PathBuf::from(s)
        }))
    };
    match &backup_path {
        Some(p) => println!("respaldo sector a sector antes de escribir: {}", p.display()),
        None => println!("aviso: relleno SIN respaldo sector a sector (--no-backup) — lo pedido por el autor es respaldar siempre"),
    }

    let fopts = fill::FillOptions {
        start: start as u64,
        end: end as u64,
        block: o.block.max(1),
        allow_zeros: o.allow_zeros,
        backup: backup_path,
        save_every_ops: fill::FILL_SAVE_EVERY_OPS,
    };
    let t0 = Instant::now();
    let mut last_line = Instant::now();
    let rep = match fill::run_fill(
        &mut donor,
        &mut img,
        &mut map,
        dmap.as_ref(),
        &fopts,
        &map_path,
        |p: &fill::FillProgress| {
            if last_line.elapsed().as_secs() >= 30 {
                last_line = Instant::now();
                println!(
                    "── [relleno] sector {} · llenados {} · ceros omitidos {} · sin donante {} · pendientes {} · {}",
                    p.cur,
                    p.filled,
                    p.zero_skipped,
                    p.donor_missing,
                    p.pending,
                    format_hms(t0.elapsed().as_secs())
                );
            }
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error durante el relleno: {e}");
            if e.kind() == std::io::ErrorKind::Interrupted {
                eprintln!("  el mapa quedó guardado en {}", map_path.display());
            }
            return ExitCode::from(1);
        }
    };
    if let Some(x) = &o.export_map {
        match map.export_text(x) {
            Ok(()) => println!("mapa de texto exportado a {}", x.display()),
            Err(e) => eprintln!("aviso: no se pudo exportar el mapa de texto: {e}"),
        }
    }
    println!("relleno terminado en {}", format_hms(rep.seconds));
    println!(
        "  llenados desde el donante: {}  ceros omitidos: {}  sin dato en el donante: {}  lecturas fallidas del donante: {}",
        rep.filled, rep.zero_skipped, rep.donor_missing, rep.donor_errors
    );
    let (g, f, s, u) = map.stats();
    println!(
        "  imagen ahora: {} buenos · {} pendientes",
        g,
        f + s + u
    );
    ExitCode::SUCCESS
}

fn cli_surfmap(o: &CommonOpts) -> ExitCode {
    // Con --import-map el mapa se dibuja desde un mapfile de texto (propio o
    // de ddrescue) en lugar del registro DISKn.LOG: es el estado completo del
    // rescate — verde a salvo, rojo/naranja dominio pendiente.
    if let Some(mp) = &o.import_map {
        let total = match o.sectors {
            Some(t) => t,
            None => {
                eprintln!("error: --import-map necesita --sectors N (dominio de sectores del mapfile)");
                return ExitCode::from(2);
            }
        };
        let map = match mapfile::MapFile::import_text(mp, total) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error importando el mapfile: {e}");
                return ExitCode::from(2);
            }
        };
        let from = o.start as u64;
        let to = o.end.map(|e| e as u64).unwrap_or(total);
        let html = match surfmap::render_mapfile(&map, o.cells, from, to) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        };
        let out = o.out_path.clone().unwrap_or_else(|| {
            let mut s = mp.to_string_lossy().into_owned();
            s.push_str(".surfmap.html");
            PathBuf::from(s)
        });
        if let Err(e) = std::fs::write(&out, html.as_bytes()) {
            eprintln!("no se puede escribir {}: {e}", out.display());
            return ExitCode::from(1);
        }
        println!("estado del rescate escrito en {} (ábrelo en el navegador)", out.display());
        return ExitCode::SUCCESS;
    }
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

/// Cómo `--verify` se traduce en la práctica para este backend:
/// verificación directa del firmware (ATA READ VERIFY, Linux sobre nodo de
/// disco entero) o degradación a lecturas normales. Un «no soportado» jamás
/// se registra como sector dañado.
fn verify_note(o: &CommonOpts, src: &dyn SectorSource) -> String {
    if !o.verify {
        return String::new();
    }
    if src.supports_direct_verify() {
        " — verificación directa del firmware (sin transferir contenido)".to_string()
    } else {
        " — el medio no expone verificación directa: degrada a lecturas normales".to_string()
    }
}

/// RPM del medio desde SMART, para dar contexto a las cifras de `--seek`
/// (8 ms es un seek excelente a 5400 rpm y discreto a 7200). Donde no hay
/// smartctl o el nodo no responde SMART, simplemente no se muestra: ausencia
/// de información, jamás error.
fn rotation_note(dev: Option<&Path>) -> String {
    let Some(d) = dev else {
        return String::new();
    };
    if !smart::smart_available() {
        return String::new();
    }
    match smart::query(d) {
        Ok(info) => match info.rotation_rate.as_deref() {
            Some(r) => format!(" · rotación SMART: {r}"),
            None => String::new(),
        },
        Err(_) => String::new(),
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
    // Sondeo con el primer sector (comando no-data inocuo) para que la
    // cabecera refleje la realidad: hay nodos que responden a la consulta
    // de versión pero rechazan la orden real — el sondeo los apaga aquí.
    if o.verify && src.supports_direct_verify() {
        let _ = src.verify_chunk(start as u64, 1, Duration::from_secs(o.timeout_secs));
    }
    println!(
        "{label}: {} sectores (medio: {}), barrido bulk [{start}..{end}) — prescan{}",
        src.sector_count(),
        src.media().label(),
        verify_note(o, &*src),
    );

    let mut last_bar = String::new();
    let mut hooks = RunHooks::empty();
    let t_start = Instant::now();
    // Umbrales coherentes con el motor: delay = banda T4 .
    let mut eo = EngineOptions::new(RunMode::Scan, start, end);
    eo.set_bands(o.bands);
    eo.verify = o.verify;
    eo.op_timeout = Duration::from_secs(o.timeout_secs);
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
    let (label, dev, mut src) = match open_target(o, false) {
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
    if o.seek {
        // Modo seek (cosecha Victoria): tiempos de posicionamiento con
        // lecturas individuales en posiciones aleatorias. Solo lectura.
        println!(
            "{label}: {} sectores (medio: {}), sondeo de posicionamiento [{start}..{end}) — speedtest --seek ({} lecturas aleatorias){}",
            src.sector_count(),
            src.media().label(),
            speed::SEEK_SAMPLES,
            rotation_note(dev.as_deref()),
        );
        if src.media() == MediaKind::Ssd {
            println!("aviso: en medio sólido estas cifras son tiempo de acceso, no seek mecánico");
        }
        if o.verify {
            println!("aviso: en modo seek se ignora --verify (aquí interesa el tiempo de la lectura real)");
        }
        let rep = speed::run_seektest(
            &mut *src,
            start,
            end,
            speed::SEEK_SAMPLES,
            0, // semilla interna fija: el mapa de sondeos es reproducible
            |s| {
                let _ = write!(std::io::stdout(), "\rsondeados {} sectores", s.len());
                let _ = std::io::stdout().flush();
            },
        );
        let _ = writeln!(std::io::stdout());
        println!(
            "media: {:.2} ms · mediana: {:.2} ms · p95: {:.2} ms · máximo: {:.2} ms en {} sondeos ({:.1} s), {} lecturas fallidas",
            rep.avg_ms(),
            rep.median_ms(),
            rep.p95_ms(),
            rep.max_ms(),
            rep.samples.len(),
            rep.elapsed.as_secs_f64(),
            rep.errors,
        );
        if !rep.samples.is_empty() {
            println!("sondeos más lentos:");
            for s in rep.slowest(5) {
                println!("  sector {}: {:.2} ms", s.lba, s.secs * 1000.0);
            }
        }
        return ExitCode::SUCCESS;
    }
    // Sondeo de realidad antes de la cabecera (ver nota en cli_prescan).
    if o.verify && src.supports_direct_verify() {
        let _ = src.verify_chunk(start as u64, 1, Duration::from_secs(o.timeout_secs));
    }
    println!(
        "{label}: {} sectores (medio: {}), lectura bulk [{start}..{end}) — speedtest ({} KiB/bloque){}",
        src.sector_count(),
        src.media().label(),
        SPEED_CHUNK * 512 / 1024,
        verify_note(o, &*src),
    );
    let rep = run_speedtest(
        &mut *src,
        start,
        end,
        o.verify,
        Duration::from_secs(o.timeout_secs),
        |zones| {
            let done: u64 = zones.iter().map(|_| SPEED_CHUNK as u64).sum();
            let _ = write!(
                std::io::stdout(),
                "\rleídos {} sectores",
                done.min((end - start) as u64)
            );
            let _ = std::io::stdout().flush();
        },
    );
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

/// Plan de una **pasada quirúrgica** sobre disco (`scan|repair|refresh|
/// regenerate` con `--import-map`): la pasada recorre SOLO los tramos que un
/// mapfile ajeno declaró pendientes, y anota el resultado en un mapfile propio.
pub(crate) struct SurgicalPlan {
    /// Tramos a tocar `[inicio, fin)`, ascendentes y disjuntos.
    pub ranges: Vec<(u32, u32)>,
    /// Sectores pendientes totales: base del porcentaje y del ETA.
    pub total: u64,
    /// Mapa de resultados (None en `scan`: leer no arregla nada).
    pub map: Option<mapfile::MapFile>,
    pub map_path: PathBuf,
    pub export_path: Option<PathBuf>,
    /// Ventana efectiva de la pasada: del primer al último tramo (la barra y
    /// los contadores zooman sobre la zona dañada, no sobre el disco entero).
    pub window: (u32, u32),
}

/// Mapa donde una pasada quirúrgica anota lo reparado, cuando no se dio `--map`.
fn default_result_map(import: &Path) -> PathBuf {
    let mut s = import.to_string_lossy().into_owned();
    s.push_str(".regrow.map");
    PathBuf::from(s)
}

/// «1 tramo pendiente» / «7 tramos pendientes»: concuerda el número.
fn tramos_pendentes(n: usize) -> String {
    if n == 1 {
        "1 tramo pendiente".to_string()
    } else {
        format!("{n} tramos pendientes")
    }
}

/// Construye el plan quirúrgico de la pasada.
///
/// - Reanudando: manda el plan guardado (`diskstats/DISKn.ranges`), recortado
///   al punto del checkpoint. Así `--resume` no depende de volver a dar el
///   mapfile original.
/// - Arranque nuevo: importa el mapfile ajeno y toma sus tramos pendientes
///   dentro de la ventana pedida.
///
/// `Ok(None)` = pasada normal (rango continuo).
fn build_surgical_plan(
    o: &CommonOpts,
    mode: RunMode,
    total: u64,
    win: (u32, u32),
) -> Result<Option<SurgicalPlan>, ExitCode> {
    let rp = state::ranges_path(&o.log_dir, o.id);
    let write_mode = matches!(mode, RunMode::Repair | RunMode::Refresh | RunMode::Regenerate | RunMode::TestWrite);
    let pending_status = [mapfile::MapStatus::Failed, mapfile::MapStatus::Scraped];

    // 1) ¿Hay un plan guardado que reanudar?
    if o.resume {
        match state::RangePlan::load(&rp) {
            Ok(saved) if saved.unit == o.id && saved.mode == mode => {
                let ranges = saved.clamped(win.0, win.1);
                let map_path = PathBuf::from(&saved.result_map);
                let map = match mapfile::MapFile::load(&map_path) {
                    Ok(m) if m.sector_count == total => Some(m),
                    Ok(m) => {
                        eprintln!(
                            "error: el mapa de resultados {} declara {} sectores y la unidad tiene {total}",
                            map_path.display(),
                            m.sector_count
                        );
                        return Err(ExitCode::from(1));
                    }
                    Err(e) => {
                        eprintln!(
                            "error: la pasada quirúrgica reanuda anotando en {}, pero no se puede leer: {e}",
                            map_path.display()
                        );
                        return Err(ExitCode::from(1));
                    }
                };
                let pend: u64 = ranges.iter().map(|&(a, b)| (b - a) as u64).sum();
                println!(
                    "reanudando la pasada quirúrgica ({}): {} ({} sectores) según {}",
                    mode_word(mode),
                    tramos_pendentes(ranges.len()),
                    pend,
                    rp.display()
                );
                let window = match (ranges.first(), ranges.last()) {
                    (Some(f), Some(l)) => (f.0, l.1),
                    _ => win,
                };
                return Ok(Some(SurgicalPlan {
                    ranges,
                    total: pend,
                    map,
                    map_path,
                    export_path: o.export_map.clone(),
                    window,
                }));
            }
            Ok(saved) => {
                eprintln!(
                    "error: {} es el plan de una pasada {} de la unidad {}; bórralo para empezar limpia",
                    rp.display(),
                    mode_word(saved.mode),
                    saved.unit
                );
                return Err(ExitCode::from(1));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("error leyendo el plan de tramos {}: {e}", rp.display());
                return Err(ExitCode::from(1));
            }
        }
    }

    // 2) Sin plan guardado: se construye desde el mapfile ajeno, si se pidió.
    let Some(import) = &o.import_map else {
        // Pasada continua: no debe quedar ningún plan de tramos rezagado que
        // confunda a un `--resume` posterior.
        state::RangePlan::remove(&rp);
        return Ok(None);
    };
    let map = match mapfile::MapFile::import_text(import, total) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error importando el mapfile {}: {e}", import.display());
            return Err(ExitCode::from(2));
        }
    };
    let (good, _, scraped, _) = map.stats();
    // `pending_ranges` devuelve (inicio, LONGITUD): aquí se guarda (inicio, fin).
    let raw = map.pending_ranges(win.0 as u64, win.1 as u64, &pending_status);
    let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(raw.len());
    let mut pend: u64 = 0;
    for &(a, len) in &raw {
        if a + len > u32::MAX as u64 {
            eprintln!(
                "error: el mapfile llega al sector {}, por encima de los {} sectores (≈2 TiB) que esta versión maneja",
                a + len,
                u32::MAX
            );
            return Err(ExitCode::from(1));
        }
        ranges.push((a as u32, (a + len) as u32));
        pend += len;
    }
    println!(
        "mapa ajeno importado desde {}: {} sectores buenos NO se leen ni se escriben",
        import.display(),
        good
    );
    let ventana = if win.0 == 0 && win.1 as u64 == total.min(u32::MAX as u64) {
        "todo el disco".to_string()
    } else {
        format!("sectores {}..{}", win.0, win.1)
    };
    println!(
        "bisturí: {} ({} sectores, {:.2} GB; {} sectores ya rascados por otros) — ventana: {}",
        tramos_pendentes(ranges.len()),
        pend,
        pend as f64 * 512.0 / 1e9,
        scraped,
        ventana
    );
    let map_path = o.map_path.clone().unwrap_or_else(|| default_result_map(import));
    let window = match (ranges.first(), ranges.last()) {
        (Some(f), Some(l)) => (f.0, l.1),
        _ => win,
    };
    // El plan viaja en su sidecar para que un `--resume` posterior no necesite
    // ni el mapfile original ni repetir la ventana.
    let saved = state::RangePlan {
        mode,
        unit: o.id,
        window_start: win.0,
        window_end: win.1,
        source: import.to_string_lossy().into_owned(),
        result_map: map_path.to_string_lossy().into_owned(),
        ranges: ranges.clone(),
    };
    if let Err(e) = saved.save(&rp) {
        eprintln!("error guardando el plan de tramos {}: {e}", rp.display());
        return Err(ExitCode::from(1));
    }
    Ok(Some(SurgicalPlan {
        ranges,
        total: pend,
        map: write_mode.then_some(map),
        map_path,
        export_path: o.export_map.clone(),
        window,
    }))
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

    let write_mode = matches!(mode, RunMode::Repair | RunMode::Refresh | RunMode::Regenerate | RunMode::TestWrite);
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

    // Pasada quirúrgica (`--import-map`): la pasada recorre solo los tramos
    // pendientes del mapa ajeno, y su ventana se ciñe al primer y último tramo.
    let plan = match build_surgical_plan(&o, mode, src.sector_count(), (start, end)) {
        Ok(p) => p,
        Err(c) => return c,
    };
    let plan = match plan {
        None => None,
        Some(p) if p.ranges.is_empty() => {
            println!("no queda ningún tramo pendiente en la ventana: nada que hacer");
            Checkpoint::remove(&state_path(&o.log_dir, o.id));
            state::RangePlan::remove(&state::ranges_path(&o.log_dir, o.id));
            return ExitCode::SUCCESS;
        }
        Some(p) => {
            o.start = p.window.0;
            o.end = Some(p.window.1);
            Some(p)
        }
    };
    let (run_start, run_end) = match &plan {
        Some(p) => p.window,
        None => (start, end),
    };

    process_run(
        &mut *src,
        &display,
        dev_path.as_deref(),
        &o,
        run_start,
        run_end,
        mode,
        prev.as_ref(),
        plan,
    )
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
        RunMode::TestWrite => "BATTERY MULTI-PATRÓN",
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

/// Coste de desgaste actual de un medio sólido (para los avisos de las puertas).
fn wear_note(si: &smart::SmartInfo) -> String {
    let f = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "?".into());
    format!(
        "usado={} %  mediaErrors={}  wearLeveling={}",
        f(si.percentage_used),
        f(si.media_errors),
        f(si.wear_leveling)
    )
}

/// Severidad para agregar celdas al comprimir la barra (WriteFail y Bad mandan
/// sobre Delay/Rec/Scan/Empty; marginal manda sobre las bandas lentas de su
/// entorno: T1 < marginal < T3 < T4; las bandas se ordenan T1 < T2 < T3 < T4).
fn cell_rank(c: Cell) -> u8 {
    match c {
        Cell::WriteFail => 9,
        Cell::Bad => 8,
        Cell::Delay => 7,
        Cell::Band3 => 6,
        Cell::Marginal => 5,
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
    let danados = if mode == RunMode::TestWrite {
        // En la battery, "dañado" es lo que quedó malo o sospechoso tras la prueba.
        stats.bad + stats.write_fail + stats.marginal
    } else {
        stats.bad + stats.recovered
    };
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
    } else if mode == RunMode::TestWrite {
        let r = format!(
            "{} superaron la battery · {} marginales · {} escritura mala",
            stats.tested_ok, stats.marginal, stats.write_fail
        );
        if stats.marginal + stats.write_fail > 0 {
            paint.yellow(&r)
        } else {
            paint.cyan(&r)
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
    mut plan: Option<SurgicalPlan>,
) -> ExitCode {
    // Una interrupción pendiente de una pasada anterior (menú) no debe
    // abortar esta.
    tui::clear_interrupted();
    let mut opts = EngineOptions::new(mode, start, end);
    opts.delay = Duration::from_millis(o.delay_ms);
    opts.bands = o.bands;
    opts.allow_data_loss = o.allow_data_loss;
    opts.prescan = o.prescan && mode != RunMode::Regenerate;
    opts.verify = o.verify;
    opts.op_timeout = Duration::from_secs(o.timeout_secs);

    // Red de seguridad: una pasada quirúrgica interrumpida NO se reanuda como
    // barrido continuo (el menú no trae plan). Barrer el rango volvería a leer
    // —y en `regenerate` a reescribir— zonas que el mapa declara sanas.
    if prev.is_some() && plan.is_none() {
        let rp = state::ranges_path(&o.log_dir, o.id);
        if let Ok(saved) = state::RangePlan::load(&rp) {
            if saved.unit == o.id && saved.mode == mode {
                eprintln!(
                    "error: hay una pasada quirúrgica {} pendiente ({} sectores en {}, plan en {}). Reanúdala por CLI con --resume, o borra ese plan y el checkpoint para barrer el rango continuo.",
                    mode_word(mode),
                    saved.total(),
                    tramos_pendentes(saved.ranges.len()),
                    rp.display()
                );
                return ExitCode::from(1);
            }
        }
    }

    // Bisturí: la pasada visita solo los tramos del plan quirúrgico.
    let sparse_total: Option<u64> = plan.as_ref().map(|p| p.total);    let plan_ranges = plan.as_ref().map(|p| p.ranges.len()).unwrap_or(0);
    let surgical = plan.is_some();
    if let Some(p) = &plan {
        opts.ranges = Some(p.ranges.clone());
    }
    // El mapa de resultados se desprende del plan (los hooks prestan cada
    // pieza por separado). En `scan` no hay mapa: leer no arregla nada.
    let mut result: Option<(mapfile::MapFile, PathBuf, Option<PathBuf>)> = plan
        .as_mut()
        .and_then(|p| p.map.take().map(|m| (m, p.map_path.clone(), p.export_path.clone())));

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

    // Sondeo de realidad antes de la cabecera (ver nota en cli_prescan): el
    // prescan interno usará la verificación directa solo si el nodo la admite.
    if o.verify && opts.prescan && src.supports_direct_verify() {
        let _ = src.verify_chunk(start as u64, 1, opts.op_timeout);
    }
    say(format!(
        "{display}: {} sectores (medio: {}), rango [{start}..{end}) — modo {}{}",
        src.sector_count(),
        media.label(),
        mode_word(mode),
        if o.verify && opts.prescan {
            verify_note(o, src)
        } else {
            String::new()
        },
    ));
    // Forense ATA de solo lectura (solo donde hay paso directo en disco
    // entero): capacidad real, área oculta HPA, conjuntos declarados.
    for line in ata_forensics_lines(src, true) {
        say(line);
    }
    if surgical {
        say(format!(
            "pasada quirúrgica: {} ({} sectores); del resto del rango no se lee ni se escribe ni un sector",
            tramos_pendentes(plan_ranges),
            sparse_total.unwrap_or(0)
        ));
    }
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

    // Puertas de `testwrite` (battery multi-patrón):
    // 1) JAMÁS sobre un dispositivo montado: la battery reescribe sectores con
    //    patrones adversarios mientras el sistema de archivos los lee y escribe.
    // 2) Rango completo toca sectores SANOS: exige consentimiento (sidecar que
    //    permita restaurar, o permiso explícito de pérdida de datos).
    // 3) SSD: el mismo gate de desgaste que `regenerate` en rango completo;
    //    en la quirúrgica solo aviso (el gasto se limita a los pendientes).
    if mode == RunMode::TestWrite {
        if let Some(p) = dev_path {
            if let Some(mp) = mounted_overlap(p) {
                say(format!(
                    "Bloqueado: {} participa en el montaje {}; la battery reescribiría sectores mientras el sistema de archivos los usa. Desmóntalo primero o trabaja sobre una imagen.",
                    p.display(),
                    mp
                ));
                return ExitCode::from(3);
            }
        }
        // Ambas variantes escriben contenido adversario sobre sectores que
        // pueden guardar dato rescatable: exigen sidecar o consentimiento.
        if o.backup_dir.is_none() && !o.allow_data_loss {
            say(String::from(if surgical {
                "Bloqueado: la battery quirúrgica escribe patrones adversarios sobre los tramos pendientes, que suelen contener dato rescatable. Da --backup DIR (sidecar que permite restaurar) o --allow-data-loss si aceptas la pérdida."
            } else {
                "Bloqueado: la battery en rango completo escribe patrones adversarios también sobre sectores sanos. Da --backup DIR (sidecar que permite restaurar) o --allow-data-loss si aceptas la pérdida; o usa --import-map para la versión quirúrgica."
            }));
            return ExitCode::from(3);
        }
        if !surgical {
            match gate_regenerate(media, o.ssd_wear_ok) {
                Regate::Block => {
                    say(String::from(
                        "Bloqueado: el medio es sólido (SSD/NVMe); una battery de rango completo gastaría ciclos P/E de toda la NAND sin beneficio magnético. --ssd-wear-ok para continuar con aviso.",
                    ));
                    return ExitCode::from(3);
                }
                Regate::Warn => {
                    if media == MediaKind::Ssd {
                        say(String::from("aviso: battery sobre medio sólido: cada patrón escrito consume ciclos P/E"));
                    } else {
                        say(String::from(
                            "aviso: no se pudo confirmar el tipo de medio; si fuera sólido, la battery gastaría ciclos P/E de la NAND",
                        ));
                    }
                }
                Regate::Allow => {}
            }
        } else if media == MediaKind::Ssd && !o.ssd_wear_ok {
            say(String::from("aviso: la battery quirúrgica gasta ciclos P/E en los sectores pendientes de este medio sólido"));
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
            sparse_total.unwrap_or((end - start) as u64),
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
        };
        // Hook ui: sondea interrupciones (ESC en el panel vivo; bandera SIGINT/
        // SIGTERM en cualquier modo) y reescribe las filas. Su `false` aborta
        // el pase; la llamada final de `progress` deja el checkpoint en el
        // sector exacto de la pausa (--resume). Sin tty además emite aquí la
        // línea de estado de los ~30 s: el hook vive cada tick del motor, así
        // que la línea fluye incluso cuando el disco va lentísimo en un clúster
        // malo (con `progress` se secaría: solo va por polls de 8192 sectores).
        let mut ui = |cells: &[Cell; GRID_CELLS], stats: &EngineStats, sector: u32| -> bool {
            if tui::interrupted() {
                return false;
            }
            if live {
                if let Some(k) = tui::poll_key(0) {
                    if k == tui::ESC {
                        return false;
                    }
                }
            }
            if !live && last_status.elapsed().as_secs() >= 30 {
                last_status = Instant::now();
                // Con tramos del mapa, el % y el ETA se miden sobre los sectores
                // pendientes: el rango del disco no es el trabajo real.
                let (range, done) = match sparse_total {
                    Some(t) => (t, stats.scanned),
                    None => ((end - start) as u64, (sector - start) as u64),
                };
                let el = t_start.elapsed().as_secs().max(1);
                let pct = 100.0 * done as f64 / range.max(1) as f64;
                let eta = if done > 0 {
                    format_hms((range - done) * el / done)
                } else {
                    "?".to_string()
                };
                let battery = if mode == RunMode::TestWrite {
                    format!(" · W {} · M {}", stats.write_fail, stats.marginal)
                } else {
                    String::new()
                };
                println!(
                    "── sector {sector} ({pct:.2} %) · T{} U{} V{} X{} · ilegibles {}{battery} · {:.1} MB · transcurrido {} · ETA ~{eta}",
                    stats.slow1,
                    stats.slow2,
                    stats.slow3,
                    stats.delays,
                    stats.bad,
                    done as f64 * 512.0 / 1e6,
                    format_hms(el)
                );
            }
            if live {
                let rows = live_rows(
                    mode,
                    cells,
                    stats,
                    sparse_total.unwrap_or((end - start) as u64),
                    t_start.elapsed().as_secs(),
                    &paint,
                );
                let n = rows.len();
                paint.rows(n, &rows);
            }
            true
        };
        let hooks = RunHooks {
            backup: backup.as_mut().map(|b| b as &mut dyn SectorBackup),
            monitor: guard.as_mut().map(|g| g as &mut dyn engine::HealthMonitor),
            progress: Some(&mut progress),
            // El hook vive SIEMPRE: aporta la comprobación de interrupción
            // (Ctrl+C/kill limpios sin tty) y la línea de estado degradada.
            ui: Some(&mut ui),
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

    // Resultado de la pasada quirúrgica anotado en su mapa: lo verificado tras
    // reescribir (`R`) pasa a bueno; lo que sigue malo (`B`) queda pendiente con
    // un reintento más contabilizado. Se anota también en un pase abortado (sus
    // registros parciales son válidos): un SIGKILL brutal es lo único que
    // perdería esta anotación, y solo cuesta repetir sectores, no perderlos.
    if let Some((map, map_path, export_path)) = result.as_mut() {
        let (mut certified, mut still) = (0u64, 0u64);
        for r in &report.records {
            match r.flag {
                Flag::R => {
                    map.set(r.sector as u64, r.run as u64, mapfile::MapStatus::Good, 0);
                    certified += r.run as u64;
                }
                Flag::B | Flag::W | Flag::M => {
                    // `W` (escritura mala) y `M` (marginal) siguen pendientes:
                    // ambos merecen otra tentativa en un pase posterior. La
                    // clasificación fina vive en DISKn.LOG para los humanos.
                    let s = r.sector as u64;
                    let rt = map.retries_at(s).saturating_add(1);
                    map.set(s, r.run as u64, mapfile::MapStatus::Failed, rt);
                    still += r.run as u64;
                }
                _ => {}
            }
        }
        match map.save(map_path.as_path()) {
            Ok(()) => say(format!(
                "mapa de resultados: {} sectores reparados y verificados, {} siguen pendientes → {}",
                certified,
                still,
                map_path.display()
            )),
            Err(e) => {
                eprintln!(
                    "aviso: no se pudo guardar el mapa de resultados {}: {e}",
                    map_path.display()
                )
            }
        }
        if let Some(x) = export_path.as_ref() {
            match map.export_text(x) {
                Ok(()) => say(format!("mapa de texto exportado a {}", x.display())),
                Err(e) => eprintln!("aviso: no se pudo exportar el mapa de texto: {e}"),
            }
        }
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
    if mode == RunMode::TestWrite {
        say(format!(
            "{} sectores superaron la battery entera (4 patrones + restauración verificada)",
            totals.tested_ok
        ));
        if totals.marginal > 0 {
            say(format!(
                "aviso: {} sectores MARGINALES (M): escriben y restauran bien, pero fallan con algún patrón — sanos hoy, sospechosos mañana",
                totals.marginal
            ));
        }
        if totals.write_fail > 0 {
            say(format!(
                "{} sectores con RUTA DE ESCRITURA mala (W)",
                totals.write_fail
            ));
        }
    }
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
            "{} sectores sanos salteados (prescan o mapa ajeno)",
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
        if surgical {
            say(format!(
                "los tramos pendientes quedaron en {}: --resume los retoma sin volver a dar el mapfile",
                state::ranges_path(&o.log_dir, unit).display()
            ));
        }
        return ExitCode::from(1);
    }

    if !o.no_log {
        write_chain(o, unit, report.records.clone(), src.sector_count());
    }

    // Pasada completa: el checkpoint ya no hace falta.
    Checkpoint::remove(&sp);
    if surgical {
        // Idem el plan de tramos: la pasada quirúrgica terminó los suyos.
        state::RangePlan::remove(&state::ranges_path(&o.log_dir, unit));
    }

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
            retry_wait: 2,
            reverse: true,
            map_path: None,
            backup_file: None,
            no_backup: false,
            export_map: None,
            import_map: None,
            fill_from: None,
            donor_map: None,
            allow_zeros: false,
            rate_limit: None,
            min_read_rate: rescue::MinRate::Off,
            max_error_rate: None,
            requeue: false,
            preview: None,
            log_rates: None,
            from: None,
            verify: false,
            timeout_secs: 30,
            seek: false,
            generate_map: false,
            skip_adaptive: false,
            yes: false,
            sectors: None,
            cells: surfmap::SURFMAP_DEFAULT_CELLS,
            out_path: None,
        }
    }

    /// `--import-map` en las pasadas: los tramos pendientes se guardan como
    /// (inicio, FIN) — no (inicio, longitud) — y la ventana se ciñe a ellos.
    #[test]
    fn plan_quirurgico_convierte_longitudes_en_fines() {
        let dir = std::env::temp_dir().join(format!("regrow_plan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let map = dir.join("map.log");
        fs::write(
            &map,
            "# Mapfile. Created by GNU ddrescue version 1.29\n\
             #      pos        size  status\n\
             0x00000000  0x00100000  +\n\
             0x00100000  0x00000400  ?\n\
             0x00100400  0x000ff000  +\n\
             0x001ff400  0x00000c00  /\n\
             0x00200000  0x00200000  +\n",
        )
        .unwrap();
        let mut o = opts(&dir, 7, false);
        o.import_map = Some(map.clone());
        // Dominio 8192 sectores: el mapa cubre todo (lo no mapeado cuenta pendiente).
        let plan = build_surgical_plan(&o, RunMode::Repair, 8192, (0, 8192))
            .unwrap()
            .expect("el mapa deja tramos");
        assert_eq!(plan.ranges, vec![(2048, 2050), (4090, 4096)]);
        assert_eq!(plan.total, 8);
        assert_eq!(plan.window, (2048, 4096));
        assert_eq!(plan.map_path, default_result_map(&map));
        assert!(plan.map.is_some(), "repair sí anota su resultado");
        // El sidecar queda escrito y es reparseable (base del `--resume`).
        let saved = state::RangePlan::load(&state::ranges_path(&dir, 7)).unwrap();
        assert_eq!(saved.ranges, plan.ranges);
        assert_eq!(saved.mode, RunMode::Repair);
        assert_eq!(saved.window_end, 8192);
        state::RangePlan::remove(&state::ranges_path(&dir, 7));
        fs::remove_dir_all(&dir).ok();
    }

    /// En `scan` no hay mapa de resultados: leer no arregla nada.
    #[test]
    fn plan_quirurgico_scan_no_anota_mapa() {
        let dir = std::env::temp_dir().join(format!("regrow_plan_scan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let map = dir.join("map.log");
        fs::write(&map, "0x00000000  0x00000400  -\n").unwrap();
        let mut o = opts(&dir, 8, false);
        o.import_map = Some(map);
        let plan = build_surgical_plan(&o, RunMode::Scan, 2, (0, 2))
            .unwrap()
            .expect("hay tramos");
        assert_eq!(plan.ranges, vec![(0, 2)]);
        assert!(plan.map.is_none());
        state::RangePlan::remove(&state::ranges_path(&dir, 8));
        fs::remove_dir_all(&dir).ok();
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

    /// Techos de rescate: sufijos K/M, 'off' desactiva, 'auto' en el umbral
    /// de zona lenta.
    #[test]
    fn parse_controles_de_tasa_rescue() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let o = parse_common(&s(&["x.raw", "--raw", "1"]));
        assert_eq!(o.rate_limit, None);
        assert_eq!(o.max_error_rate, None);
        assert_eq!(o.min_read_rate, rescue::MinRate::Off);
        let o = parse_common(&s(&[
            "x.raw", "--raw", "1", "--rate-limit", "500K", "--max-error-rate", "2M",
            "--min-read-rate", "auto",
        ]));
        assert_eq!(o.rate_limit, Some(500 * 1024));
        assert_eq!(o.max_error_rate, Some(2 * 1024 * 1024));
        assert_eq!(o.min_read_rate, rescue::MinRate::Auto);
        let o = parse_common(&s(&["x.raw", "--raw", "1", "--min-read-rate", "64K"]));
        assert_eq!(o.min_read_rate, rescue::MinRate::Bytes(65536));
        let o = parse_common(&s(&["x.raw", "--raw", "1", "--rate-limit", "off"]));
        assert_eq!(o.rate_limit, None);
        let o = parse_common(&s(&["x.raw", "--raw", "1", "--requeue"]));
        assert!(o.requeue);
        let o = parse_common(&s(&["x.raw", "--raw", "1", "--preview"]));
        assert_eq!(o.preview, Some(3));
        let o = parse_common(&s(&["x.raw", "--raw", "1", "--preview", "8", "--log-rates", "ritmos.dat"]));
        assert_eq!(o.preview, Some(8));
        assert_eq!(o.log_rates, Some(PathBuf::from("ritmos.dat")));
    }

    #[test]
    fn hex_preview_estilo_ddrescue() {
        let data = b"Hola mundo!\0\0\0\0\0\0";
        let s = hex_preview(7, data, 1);
        assert!(s.contains("sector 7"), "{s}");
        assert!(s.contains("48 6f 6c 61 20 6d 75 6e 64 6f 21 00"), "{s}");
        assert!(s.contains("|Hola mundo!.....|"), "{s}");
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

    /// `--verify` enciende la comprobación directa; `--timeout` fija el plazo
    /// por comando (defecto 30 s, acotado a un rango sensato).
    #[test]
    fn parse_verify_y_timeout() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let o = parse_common(&s(&["speedtest"]));
        assert!(!o.verify);
        assert_eq!(o.timeout_secs, 30);
        let o = parse_common(&s(&["speedtest", "--verify", "--timeout", "90"]));
        assert!(o.verify);
        assert_eq!(o.timeout_secs, 90);
        let o = parse_common(&s(&["prescan", "--verify", "--timeout", "0"]));
        assert_eq!(o.timeout_secs, 1, "el plazo se acota al menos a 1 s");
    }

    /// `--seek` activa el modo de sondeo de posicionamiento en speedtest.
    #[test]
    fn parse_seek() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let o = parse_common(&s(&["speedtest"]));
        assert!(!o.seek);
        let o = parse_common(&s(&["speedtest", "--seek"]));
        assert!(o.seek);
        assert!(!o.verify, "seek no activa verify por sí solo");
    }

    /// sanitize exige modo y destino crudo; sin --allow-data-loss se rechaza
    /// antes de tocar nada (el dispositivo de prueba no existe a propósito).
    #[test]
    fn sanitize_exige_modo_destino_y_permiso() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        assert_eq!(cli_sanitize(&s(&[])), ExitCode::from(2), "falta el modo");
        assert_eq!(
            cli_sanitize(&s(&["sin-modo", "--dev", "/dev/x"])),
            ExitCode::from(2)
        );
        assert_eq!(
            cli_sanitize(&s(&["block-erase", "--id", "9"])),
            ExitCode::from(2),
            "sin --dev/--raw no hay medio que sanear"
        );
        assert_eq!(
            cli_sanitize(&s(&["block-erase", "--dev", "/dev/regrow-no-existe-zz"])),
            ExitCode::from(3),
            "sin --allow-data-loss no se llega ni a pedir confirmación"
        );
    }

    /// `--from` alimenta el origen del clonado.
    #[test]
    fn parse_from_origen_de_clone() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let o = parse_common(&s(&["clone", "--from", "src.raw", "--dev", "/dev/loop0"]));
        assert_eq!(o.from.as_deref(), Some(Path::new("src.raw")));
        let o = parse_common(&s(&["clone"]));
        assert!(o.from.is_none());
    }

    /// Gates de `clone` que abortan antes de tocar nada: sin origen, sin
    /// destino crudo, ruta posicional (eso es rescue), sin aceptar la ausencia
    /// de sidecar, y autocopia sobre el mismo disco.
    #[test]
    fn clone_gates_previos() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        // Sin --from.
        assert_eq!(
            cli_clone(&parse_common(&s(&["--dev", "/dev/regrow-no-existe-zz"]))),
            ExitCode::from(2)
        );
        // Sin destino crudo.
        assert_eq!(cli_clone(&parse_common(&s(&["--from", "src.raw"]))), ExitCode::from(2));
        // Ruta posicional: en clone no es el destino (eso es rescue).
        assert_eq!(
            cli_clone(&parse_common(&s(&["dest.raw", "--from", "src.raw", "--dev", "/dev/x"]))),
            ExitCode::from(2)
        );
        // Sin aceptar explícitamente la ausencia de sidecar.
        assert_eq!(
            cli_clone(&parse_common(&s(&["--from", "src.raw", "--dev", "/dev/x"]))),
            ExitCode::from(2),
            "falta --no-backup: clone no respalda el destino"
        );
        // Autocopia: origen y destino son literalmente el mismo nodo (se usa
        // un archivo real para que el origen abra y la puerta actúe).
        let tmp = std::env::temp_dir().join(format!("regrow_same_{}", std::process::id()));
        fs::write(&tmp, vec![0u8; 4096]).unwrap();
        assert_eq!(
            cli_clone(&parse_common(&s(&[
                "--from",
                &tmp.display().to_string(),
                "--dev",
                &tmp.display().to_string(),
                "--allow-data-loss",
                "--no-backup",
                "--yes",
            ]))),
            ExitCode::from(2),
            "mismo nodo: bloqueado antes de abrir el destino"
        );
        let _ = fs::remove_file(&tmp);
        // Partición del disco origen: la clave de disco unifica ambos nodos.
        assert!(same_device(Path::new("/dev/sdb"), Path::new("/dev/sdb5")));
        assert!(same_device(Path::new("/dev/nvme0n1"), Path::new("/dev/nvme0n1p2")));
        assert!(!same_device(Path::new("/dev/sda"), Path::new("/dev/sdb")));
    }

    /// Flujo completo de `clone` sobre archivos: en macOS RawDisk abre un
    /// archivo sin banderas especiales y el destino crudo puede ser un archivo
    /// normal. En Linux esta ruta se valida en campo contra dispositivos de
    /// bucle reales (las banderas O_DIRECT/O_SYNC/O_EXCL no son para archivos).
    #[cfg(target_os = "macos")]
    #[test]
    fn clone_completo_sobre_archivos_y_reanudacion() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let dir = std::env::temp_dir().join(format!("regrow_clone_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("origen.raw");
        let dst = dir.join("destino.raw");
        let map = dir.join("clon.map");
        let sectors = 1000u64;
        // Origen con patrón reconocible; destino preexistente con basura.
        let data: Vec<u8> = (0..(sectors as usize * 512)).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &data).unwrap();
        fs::write(&dst, vec![0x5Au8; sectors as usize * 512]).unwrap();

        let code = cli_clone(&parse_common(&s(&[
            "--from",
            &src.display().to_string(),
            "--dev",
            &dst.display().to_string(),
            "--map",
            &map.display().to_string(),
            "--block",
            "64",
            "--allow-data-loss",
            "--no-backup",
            "--yes",
        ])));
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(fs::read(&dst).unwrap(), data, "el destino quedó byte a byte igual al origen");
        assert!(map.exists(), "el mapfile del clonado se escribe");

        // Reanudación: todo bueno ⇒ segunda corrida no relee ni reescribe nada
        // (el mapa lo impide) y el destino sigue idéntico.
        let code = cli_clone(&parse_common(&s(&[
            "--from",
            &src.display().to_string(),
            "--dev",
            &dst.display().to_string(),
            "--map",
            &map.display().to_string(),
            "--resume",
            "--allow-data-loss",
            "--no-backup",
            "--yes",
        ])));
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(fs::read(&dst).unwrap(), data);

        // Con --verify (contraste bloque a bloque) sobre un destino honesto.
        let dst2 = dir.join("dest2.raw");
        fs::write(&dst2, vec![0u8; sectors as usize * 512]).unwrap();
        let code = cli_clone(&parse_common(&s(&[
            "--from",
            &src.display().to_string(),
            "--dev",
            &dst2.display().to_string(),
            "--map",
            &dir.join("clon2.map").display().to_string(),
            "--verify",
            "--allow-data-loss",
            "--no-backup",
            "--yes",
        ])));
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(fs::read(&dst2).unwrap(), data, "el contraste no altera el resultado");

        // Destino más pequeño que el rango: puerta de tamaño (exit 2).
        let small = dir.join("pequeno.raw");
        fs::write(&small, vec![0u8; 200 * 512]).unwrap();
        let code = cli_clone(&parse_common(&s(&[
            "--from",
            &src.display().to_string(),
            "--dev",
            &small.display().to_string(),
            "--map",
            &dir.join("clon3.map").display().to_string(),
            "--allow-data-loss",
            "--no-backup",
            "--yes",
        ])));
        assert_eq!(code, ExitCode::from(2), "el destino debe cubrir el rango clonado");
        assert!(fs::read(&small).unwrap().iter().all(|&b| b == 0), "no se tocó nada");

        // Sin --allow-data-loss pero con todo lo demás: puerta 3, intacto.
        // (usa un destino del tamaño correcto para llegar hasta la puerta).
        let code = cli_clone(&parse_common(&s(&[
            "--from",
            &src.display().to_string(),
            "--dev",
            &dst.display().to_string(),
            "--map",
            &dir.join("clon4.map").display().to_string(),
            "--no-backup",
        ])));
        assert_eq!(code, ExitCode::from(3));
        assert_eq!(fs::read(&dst).unwrap(), data, "sin permiso no se toca nada");

        let _ = fs::remove_dir_all(&dir);
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

    #[test]
    fn cli_parse_modo_relleno() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        // parse_common recibe args[1..]: el primer token es la imagen posicional.
        let o = parse_common(&s(&[
            "img.raw",
            "--fill-from",
            "don.img",
            "--donor-map",
            "d.log",
            "--allow-zeros",
        ]));
        assert_eq!(o.fill_from.as_deref(), Some(Path::new("don.img")));
        assert_eq!(o.donor_map.as_deref(), Some(Path::new("d.log")));
        assert!(o.allow_zeros);
        assert_eq!(o.image.as_deref(), Some(Path::new("img.raw")));
    }

    // ---- Puertas de `testwrite` (M16; la detección de montaje vive ahora en
    // la capa de dispositivo, con sus tests en source.rs) ----

    /// Imagen de prueba con contenido reconocible (no todo-cero).
    fn fixture_image(dir: &Path, sectors: u64) -> PathBuf {
        let p = dir.join("img.raw");
        let mut data = vec![0u8; (sectors * 512) as usize];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        fs::write(&p, &data).unwrap();
        p
    }

    fn tw_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("regrow_tw_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn testwrite_rango_completo_sin_sidecar_ni_permiso_queda_bloqueado() {
        let dir = tw_dir("full");
        let img = fixture_image(&dir, 8);
        let before = fs::read(&img).unwrap();
        let mut o = opts(&dir, 9, false);
        o.image = Some(img.clone());
        let mut src = FileImage::open_rw(&img).unwrap();
        let code = process_run(&mut src, "img", None, &o, 0, 8, RunMode::TestWrite, None, None);
        assert_eq!(code, ExitCode::from(3));
        assert_eq!(fs::read(&img).unwrap(), before, "bloqueado significa NO tocar nada");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn testwrite_quirurgica_sin_sidecar_tambien_queda_bloqueada() {
        let dir = tw_dir("surg");
        let img = fixture_image(&dir, 8);
        let mut o = opts(&dir, 10, false);
        o.image = Some(img.clone());
        let plan = SurgicalPlan {
            ranges: vec![(2, 4)],
            total: 2,
            map: None,
            map_path: dir.join("m.map"),
            export_path: None,
            window: (2, 4),
        };
        let mut src = FileImage::open_rw(&img).unwrap();
        let code = process_run(&mut src, "img", None, &o, 0, 8, RunMode::TestWrite, None, Some(plan));
        assert_eq!(code, ExitCode::from(3));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn testwrite_con_sidecar_escribe_patrones_y_deja_la_imagen_igual() {
        let dir = tw_dir("ok");
        let img = fixture_image(&dir, 8);
        let before = fs::read(&img).unwrap();
        let mut o = opts(&dir, 11, false);
        o.image = Some(img.clone());
        o.backup_dir = Some(dir.join("bk"));
        let mut src = FileImage::open_rw(&img).unwrap();
        let code = process_run(&mut src, "img", None, &o, 0, 8, RunMode::TestWrite, None, None);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(
            fs::read(&img).unwrap(),
            before,
            "tras la battery cada sector debe estar como su contenido original"
        );
        // El sidecar guarda copia de cada sector reescrito (5 escrituras × 8 sectores).
        assert!(dir.join("bk").join("DISK11.bak.raw").exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// `--generate-map` y `--skip-adaptive` se reconocen y no encienden nada más.
    #[test]
    fn parse_generate_map_y_skip_adaptive() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let o = parse_common(&s(&["d.raw", "--raw", "9"]));
        assert!(!o.generate_map);
        assert!(!o.skip_adaptive);
        let o = parse_common(&s(&[
            "d.raw",
            "--raw",
            "9",
            "--generate-map",
            "--skip-adaptive",
        ]));
        assert!(o.generate_map);
        assert!(o.skip_adaptive);
        assert!(!o.resume, "generate-map no implica resume");
    }

    /// Puertas de `--generate-map` ANTES de abrir origen ni destino: el mapa se
    /// deduce de una imagen que ya existe, así que choca con --resume (mapa
    /// continuo) y con --import-map (mapa ajeno), y exige la imagen.
    #[test]
    fn generate_map_gates_antes_de_tocar_nada() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let dir = std::env::temp_dir().join(format!("regrow_gmap_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("no-existe.raw").display().to_string();
        let exists = dir.join("herencia.raw");
        fs::write(&exists, vec![0u8; 4096]).unwrap();
        let exists = exists.display().to_string();
        let dev = "/dev/regrow-no-existe-zz";

        // Imagen inexistente: no hay de dónde deducir el mapa.
        assert_eq!(
            cli_rescue(&parse_common(&s(&[&missing, "--dev", dev, "--generate-map"]))),
            ExitCode::from(2)
        );
        // Choca con --resume.
        assert_eq!(
            cli_rescue(&parse_common(&s(&[&exists, "--dev", dev, "--generate-map", "--resume"]))),
            ExitCode::from(2)
        );
        // Choca con --import-map.
        assert_eq!(
            cli_rescue(&parse_common(&s(&[
                &exists,
                "--dev",
                dev,
                "--generate-map",
                "--import-map",
                "otro.map",
            ]))),
            ExitCode::from(2)
        );
        // Ninguna puerta tocó la imagen ni creó mapa.
        assert_eq!(fs::read(&exists).unwrap(), vec![0u8; 4096]);
        assert!(!dir.join("herencia.raw.map").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
