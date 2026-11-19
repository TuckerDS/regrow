// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Menú interactivo de regrow: seis opciones (reparar / escanear / refrescar /
//! regenerar / estadísticas / salir), rango de sectores por número o Mb
//! (sufijo 'M'), y panel de estadísticas por unidad con un mapa visual de
//! 520 celdas (░ sin escanear, █ escaneado, T/U/V/D lentos por banda,
//! R recuperado, B dañado).
//!
//! Es solo UI: toda la ejecución pasa por `process_run` (motor + registros),
//! igual que el CLI. Textos redactados propios.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::engine::RunMode;
use crate::log::{Flag, DiskLog, PanelStats, Record};
use crate::source::{list_native_disks, FileImage, MediaKind, NativeDisk, RawDisk, SectorSource};
use crate::state::{
    load_pending, list_pending, resume_note, resume_plan, state_path, Checkpoint, ResumePlan,
};
use crate::{process_run, CommonOpts};

/// 1 Mb = 1048576 B / 512 B por sector. Los rangos aceptan sufijo M/m para
/// megabytes (equivale a ×2048 sectores).
pub const SECTORS_PER_MB: u64 = 1048576 / 512; // 2048

/// Celdas del mapa visual de una unidad.
const MAP_CELLS: usize = 0x208;

/// Destino de sectores crudos elegido (por argumento o pregunta interactiva).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Raw(PathBuf),
    Image(PathBuf),
}

impl Target {
    fn open(&self, write: bool) -> io::Result<(String, Box<dyn SectorSource>)> {
        match self {
            Target::Raw(p) => {
                let d = RawDisk::open(p, write)?;
                Ok((p.display().to_string(), Box::new(d)))
            }
            Target::Image(p) => {
                let f = if write {
                    FileImage::open_rw(p)?
                } else {
                    FileImage::open_ro(p)?
                };
                Ok((p.display().to_string(), Box::new(f)))
            }
        }
    }

    /// Ruta de dispositivo si el destino es crudo (para smartctl).
    fn raw_path(&self) -> Option<PathBuf> {
        match self {
            Target::Raw(p) => Some(p.clone()),
            Target::Image(_) => None,
        }
    }
}

/// Resuelve la entrada de unidad: índice de la lista mostrada (1..len),
/// número de disco crudo, ruta explícita o imagen de archivo.
fn resolve_unit(line: &str, disks: &[NativeDisk]) -> Option<Target> {
    let s = line.trim();
    if let Ok(k) = s.parse::<usize>() {
        if k >= 1 && k <= disks.len() {
            return Some(Target::Raw(disks[k - 1].path.clone()));
        }
    }
    parse_target(s)
}

/// Número solo → disco crudo nativo; /dev/… o \\.\… → crudo explícito;
/// cualquier otra cosa → imagen de archivo.
fn parse_target(line: &str) -> Option<Target> {
    let s = line.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<u32>() {
        return Some(Target::Raw(PathBuf::from(RawDisk::native_name(n))));
    }
    if s.starts_with("/dev/") || s.starts_with("\\\\.\\") || s.starts_with("\\\\?\\") {
        return Some(Target::Raw(PathBuf::from(s)));
    }
    Some(Target::Image(PathBuf::from(s)))
}

/// Sector dado como número o Mb con sufijo M/m (×2048); saturado al total.
fn parse_sector(s: &str, total: u64) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(mb) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        let mb: u64 = mb.trim().parse().ok()?;
        return Some(mb.saturating_mul(SECTORS_PER_MB).min(total));
    }
    let v: u64 = s.parse().ok()?;
    Some(v.min(total))
}

/// Lee una línea (sin CRLF). None en EOF (p. ej. stdin cerrado) ⇒ salida limpia.
fn ask(reader: &mut dyn BufRead, prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Some(line)
        }
    }
}

fn ask_sector(reader: &mut dyn BufRead, prompt: &str, default: u64, total: u64) -> Option<u64> {
    match ask(reader, prompt) {
        None => None,
        Some(l) if l.trim().is_empty() => Some(default),
        Some(l) => match parse_sector(&l, total) {
            Some(v) => Some(v),
            None => {
                println!("valor inválido: {l}");
                None
            }
        },
    }
}

fn target_from_opts(o: &CommonOpts) -> Option<Target> {
    if let Some(n) = o.raw {
        Some(Target::Raw(PathBuf::from(RawDisk::native_name(n))))
    } else if let Some(p) = &o.dev {
        Some(Target::Raw(p.clone()))
    } else {
        o.image.as_ref().map(|p| Target::Image(p.clone()))
    }
}

/// Muestra las unidades nativas detectadas (si hay) como guía para la pregunta
/// y las devuelve para que la selección por índice de lista sea intuitiva.
fn print_native_disks() -> Vec<NativeDisk> {
    let disks = list_native_disks();
    if disks.is_empty() {
        println!("(no se detectaron discos nativos accesibles — ¿falta root/admin?)");
        return disks;
    }
    println!("Unidades nativas detectadas (se requiere root/admin para reparar):");
    for (i, d) in disks.iter().enumerate() {
        println!(
            "  [{}] {:>14}  {:>12}  {}",
            i + 1,
            d.label,
            format_size(d.sectors),
            d.media.label()
        );
    }
    disks
}

/// Tamaño humano a partir de sectores (para el listado de unidades).
fn format_size(sectors: u64) -> String {
    let mb = sectors >> 11;
    if mb < 0x2800 {
        format!("{mb} Mb")
    } else {
        format!("{} Gb", mb >> 10)
    }
}

/// Número de unidad crudo a partir del nombre del nodo :
/// `rdiskN` y `PhysicalDriveN` usan el número directamente; `/dev/sdX`
/// (Linux, letra = índice como genera `native_name`) por la letra. None si
/// no se reconoce (el menú deja el id por defecto; `--id` lo fija).
pub(crate) fn raw_unit_number(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    if let Some(rest) = name.strip_prefix("rdisk") {
        return rest.parse::<u32>().ok();
    }
    if let Some(rest) = name.strip_prefix("PhysicalDrive") {
        return rest.parse::<u32>().ok();
    }
    if let Some(rest) = name.strip_prefix("sd") {
        let b = rest.as_bytes();
        if b.len() == 1 && b[0].is_ascii_lowercase() {
            return Some((b[0] - b'a') as u32);
        }
    }
    None
}

pub fn run(o: &CommonOpts) -> ExitCode {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    run_with(&mut reader, o)
}

fn run_with(reader: &mut dyn BufRead, o: &CommonOpts) -> ExitCode {
    // Detección de la unidad objetivo.
    let target = match target_from_opts(o) {
        Some(t) => t,
        None => {
            let disks = print_native_disks();
            match ask(
                reader,
                "unidad: [nº de la lista], número de disco, /dev/… o imagen de archivo: ",
            ) {
                Some(line) => match resolve_unit(&line, &disks) {
                    Some(t) => t,
                    None => {
                        eprintln!("unidad no reconocida: {line}");
                        return ExitCode::from(1);
                    }
                },
                None => return ExitCode::SUCCESS,
            }
        }
    };
    // Salvo --id explícito, la unidad de registros es el número del disco
    // crudo elegido, igual que el CLI con --raw N.
    let mut o = o.clone();
    if !o.id_explicit {
        if let Target::Raw(p) = &target {
            if let Some(n) = raw_unit_number(p) {
                o.id = n;
            }
        }
    }
    let (total, media) = match target.open(false) {
        Ok((display, src)) => {
            let media = src.media();
            println!("{display}: {} sectores (medio: {})", src.sector_count(), media.label());
            if matches!(target, Target::Raw(_)) {
                println!("(aviso) para reparar/regenerar hace falta acceso exclusivo: desmonta los volúmenes del disco; algunos puentes USB o NVMe internos pueden limitar el acceso directo.");
            }
            (src.sector_count(), media)
        }
        Err(e) => {
            eprintln!("error abriendo la unidad: {e}");
            return ExitCode::from(1);
        }
    };
    let total = total.min(u32::MAX as u64) as u32;
    if total == 0 {
        eprintln!("error: la unidad reporta 0 sectores (tamaño no fiable)");
        return ExitCode::from(1);
    }
    let dev_path = target.raw_path();

    // Reanudación por detección: si esta unidad tiene el
    // checkpoint de una pasada interrumpida se avisa aquí, y la oferta de
    // reanudar aparece al elegir el modo correspondiente. No se añade una
    // opción fija «Reanudar» al menú: solo tiene sentido cuando existe algo
    // que reanudar, y el usuario no tiene por qué saber qué unidades lo tienen.
    let mut pending = match load_pending(&o.log_dir, o.id) {
        Ok(cp) => cp,
        Err(e) => {
            eprintln!(
                "aviso: no se pudo leer {}: {e}",
                state_path(&o.log_dir, o.id).display()
            );
            None
        }
    };
    if let Some(cp) = &pending {
        if resume_plan(cp, cp.mode, total as u64).is_some() {
            println!("Hay una pasada {}", resume_note(cp));
        } else {
            println!(
                "(checkpoint {}, pero ya no deja sectores pendientes en este disco)",
                resume_note(cp)
            );
        }
    }

    loop {
        println!();
        println!("1. Escanear y reparar sectores dañados");
        println!("2. Escanear sin reparar (solo listar dañados y lentos)");
        println!("3. Refrescar sectores lentos (releer y reescribir su propio contenido)");
        if media == MediaKind::Ssd {
            println!("4. Regenerar todos los sectores de un rango — BLOQUEADO en SSD (desgasta la NAND; requiera --ssd-wear-ok)");
        } else {
            println!("4. Regenerar todos los sectores de un rango (incluso los sanos)");
        }
        println!("5. Ver estadísticas");
        println!("6. Salir");
        let choice = match ask(reader, "Elige [1-6]: ") {
            Some(c) => c,
            None => break,
        };
        let mode = match choice.trim() {
            "1" => RunMode::Repair,
            "2" => RunMode::Scan,
            "3" => RunMode::Refresh,
            "4" => RunMode::Regenerate,
            "5" => {
                statistics(reader, &o);
                continue;
            }
            "6" => break,
            // Línea vacía: repite el menú sin ruido de error.
            "" => continue,
            _ => {
                println!("opción inválida (usa 1-6)");
                continue;
            }
        };
        if mode == RunMode::Refresh {
            println!("Refrescar relee cada sector lento y reescribe sus MISMOS bytes para forzar su reasignación.");
            println!("Solo toca sectores lentos o dañados (cualquier banda de latencia); los sanos no se escriben.");
            println!("(Con --backup DIR guardas una copia previa de cada sector tocado.)");
        }
        if mode == RunMode::Regenerate {
            // Puerta de desgaste en el menú: SSD confirmado se bloquea
            // salvo --ssd-wear-ok; medio desconocido exige confirmación extra.
            if media == MediaKind::Ssd && !o.ssd_wear_ok {
                println!("Bloqueado: el medio es sólido (SSD/NVMe). Regenerar páginas sanas consume ciclos P/E de toda la NAND sin beneficio.");
                println!("(Si aun así quieres continuar, relanza el menú con --ssd-wear-ok.)");
                continue;
            }
            println!("Atención: este modo necesita acceso exclusivo al disco.");
            println!("Regenerar todos los sectores puede tardar mucho tiempo.");
            println!("No lo uses si el disco contiene datos importantes sin copia de seguridad.");
            if media == MediaKind::Ssd {
                println!("Advertencia: el medio es sólido; --ssd-wear-ok acepta explícitamente ese desgaste.");
            } else if media == MediaKind::Unknown {
                println!("No se pudo confirmar el tipo de medio: si fuera un SSD, regenerar desgastaría la NAND sin ningún beneficio.");
            }
            match ask(reader, "¿Continuar? (y/N): ") {
                Some(l) if l.trim().eq_ignore_ascii_case("y") => {}
                _ => continue,
            }
        }
        // oferta de reanudación, solo si el checkpoint pendiente
        // corresponde a este modo y su rango sigue siendo válido.
        let mut resume_from: Option<ResumePlan> = None;
        let mut prev: Option<Checkpoint> = None;
        if let Some(cp) = &pending {
            if let Some(plan) = resume_plan(cp, mode, total as u64) {
                println!("Pasada {}", resume_note(cp));
                let reanudar = match ask(reader, "¿Reanudar esa pasada? (Y/n): ") {
                    Some(l) => l.trim().is_empty() || l.trim().eq_ignore_ascii_case("y"),
                    None => false,
                };
                if reanudar {
                    resume_from = Some(plan);
                    prev = Some(cp.clone());
                } else {
                    println!("Empezando de cero: el checkpoint anterior se reemplaza conforme avance la pasada nueva.");
                }
            }
        }
        // Rango: el del checkpoint si se reanuda; si no, el fijado por
        // --start/--end o preguntado (número o Mb con 'M').
        let total64 = total as u64;
        let (start, end) = match resume_from {
            Some(p) => (p.start as u64, p.end as u64),
            None => {
                if o.start != 0 || o.end.is_some() {
                    let e = o.end.map(|e| (e as u64).min(total64)).unwrap_or(total64);
                    (o.start as u64, e)
                } else {
                    let start = match ask_sector(
                        reader,
                        "primer sector (vacío = 0, Mb con 'M'): ",
                        0,
                        total64,
                    ) {
                        Some(v) => v,
                        None => continue,
                    };
                    let end = match ask_sector(
                        reader,
                        "último sector exclusivo (vacío = total, Mb con 'M'): ",
                        total64,
                        total64,
                    ) {
                        Some(v) => v,
                        None => continue,
                    };
                    (start, end)
                }
            }
        };
        if start >= end {
            println!("rango vacío (start={start} end={end}, total={total})");
            continue;
        }
        let write_mode = mode != RunMode::Scan;
        let (display, mut src) = match target.open(write_mode) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error abriendo la unidad: {e}");
                continue;
            }
        };
        // Reanudar implica preservar el histórico de la cadena de logs (misma
        // regla que el CLI con --resume). El clone por pasada evita que el
        // merge se propague a las pasadas siguientes del menú.
        let oo = if prev.is_some() {
            CommonOpts {
                merge: true,
                ..o.clone()
            }
        } else {
            o.clone()
        };
        // process_run abre el respaldo (si --backup) en cada pasada, sobre el
        // mismo archivo disperso DISKn.bak.raw.
        let code = process_run_menu(
            &mut *src,
            &display,
            dev_path.as_deref(),
            &oo,
            start as u32,
            end as u32,
            mode,
            prev.as_ref(),
        );
        if code != ExitCode::SUCCESS {
            println!("(la pasada terminó con error o fue interrumpida; el menú continúa)");
        }
        // El checkpoint cambia con cada pasada (se borra al completar y se
        // actualiza al abortar): se releé para la próxima oferta.
        pending = load_pending(&o.log_dir, o.id).ok().flatten();
    }
    ExitCode::SUCCESS
}

/// El menú delega en `process_run` exactamente como el CLI.
fn process_run_menu(
    src: &mut dyn SectorSource,
    display: &str,
    dev_path: Option<&Path>,
    o: &CommonOpts,
    start: u32,
    end: u32,
    mode: RunMode,
    prev: Option<&Checkpoint>,
) -> ExitCode {
    process_run(src, display, dev_path, o, start, end, mode, prev)
}

// ---------- Panel de estadísticas por unidad ----------

/// Una entrada del mapa de unidades: archivo DISK*.LOG (o legacy HDD*.LOG)
/// del directorio de registros.
struct MapEntry {
    unit: u32,
    path: PathBuf,
    log: DiskLog,
}

fn drive_map(dir: &Path) -> io::Result<Vec<MapEntry>> {
    let mut out = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // Solo los .LOG activos (los rotados .001-.005 quedan fuera);
            // se aceptan también los nombres legacy para importar históricos.
            let num = name
                .strip_prefix("DISK")
                .or_else(|| name.strip_prefix("HDD"))
                .and_then(|r| r.strip_suffix(".LOG"));
            if let Some(num) = num {
                if let Ok(n) = num.parse::<u32>() {
                    let bytes = fs::read(&path)?;
                    if let Ok(log) = DiskLog::parse(&bytes) {
                        out.push(MapEntry {
                            unit: n,
                            path,
                            log,
                        });
                    }
                }
            }
        }
    }
    out.sort_by_key(|e| e.unit);
    Ok(out)
}

/// Mapa visual del estado de una unidad: 520 celdas '░'; los glifos B/R y las
/// bandas de lentitud T/U/V/D (legacy A/N→B, C/D→R, P→X) se pintan por
/// corrida; R y B mandan sobre las bandas, y entre bandas manda la peor
/// . El span S…L pinta '█' solo donde la celda aún está '░'.
/// span = (fin−inicio)/520.
fn render_map(span_start: u64, span_end: u64, records: &[Record]) -> Vec<char> {
    let mut cells = vec!['\u{2591}'; MAP_CELLS];
    let span = span_end.saturating_sub(span_start);
    if span == 0 {
        return cells;
    }
    let cell_span = (span + MAP_CELLS as u64 - 1) / MAP_CELLS as u64;
    let idx = |sec: u64| (((sec - span_start) / cell_span).min(MAP_CELLS as u64 - 1)) as usize;
    // Severidad de las marcas de lentitud  : ░ < T < U < V < D;
    // B/R/Escanado mandan sobre todas ellas.
    let rank = |c: char| -> u8 {
        match c {
            '\u{2591}' => 0,
            'T' => 1,
            'U' => 2,
            'V' => 3,
            'D' => 4,
            _ => 9,
        }
    };
    for r in records {
        let (lo, hi) = (idx(r.sector as u64), idx(r.end() as u64));
        match r.flag.canonical() {
            Flag::R => {
                for c in lo..=hi {
                    cells[c] = 'R';
                }
            }
            Flag::B => {
                for c in lo..=hi {
                    if cells[c] != 'R' {
                        cells[c] = 'B';
                    }
                }
            }
            f @ (Flag::X | Flag::T | Flag::U | Flag::V) => {
                let (glyph, rk) = match f {
                    Flag::T => ('T', 1u8),
                    Flag::U => ('U', 2),
                    Flag::V => ('V', 3),
                    _ => ('D', 4), // X: la peor banda T4 (glifo clásico 'D')
                };
                for c in lo..=hi {
                    if rank(cells[c]) < rk {
                        cells[c] = glyph;
                    }
                }
            }
            _ => {}
        }
    }
    for c in idx(span_start)..=idx(span_end - 1) {
        if cells[c] == '\u{2591}' {
            cells[c] = '\u{2588}'; // '█' escaneado, solo si aún '░'
        }
    }
    cells
}

fn statistics(reader: &mut dyn BufRead, o: &CommonOpts) {
    let entries = match drive_map(&o.log_dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error leyendo {}: {e}", o.log_dir.display());
            return;
        }
    };
    if entries.is_empty() {
        println!(
            "El mapa de unidades está vacío: no hay DISK*.LOG en {}",
            o.log_dir.display()
        );
        return;
    }
    for e in &entries {
        println!();
        println!("Estadísticas — unidad {}", e.unit);
        // El log registro solo anomalías + bordes S/L. Con L la pasada está
        // cerrada y el span es exacto; sin L (pasada interrumpida) solo se
        // conoce el arranque y las anomalías registradas hasta la pausa.
        if let Some((s, en, closed)) = crate::log::scan_extent(&e.log) {
            if closed {
                println!("Escaneados {} sectores [{s}..{en})", en - s);
            } else {
                println!(
                    "Pasada interrumpida: sin marcador de cierre (L); rango iniciado en {s}, anomalías registradas hasta {}",
                    en - 1
                );
            }
            println!(
                "Mapa: ░ sin escanear · █ escaneado · T/U/V lentos por banda · D muy lento · R recuperado · B dañado"
            );
            for chunk in render_map(s, en, &e.log.records).chunks(104) {
                println!("|{}|", chunk.iter().collect::<String>());
            }
        }
        let ps = PanelStats::from_records(&e.log.records);
        println!("{} sectores dañados persistentes", ps.remain_bad);
        println!("{} sectores recuperados", ps.ever_recovered);
        let lento = ps.delays + ps.slow1 + ps.slow2 + ps.slow3;
        if lento > 0 {
            println!(
                "{} sectores lentos registrados (T1 {} · T2 {} · T3 {} · T4 {})",
                lento, ps.slow1, ps.slow2, ps.slow3, ps.delays
            );
        }
        if let Some(when) = crate::log::log_created(&e.log.raw_footer) {
            println!("Última pasada registrada: {when}");
        }
    }
    // los checkpoints pendientes también se ven aquí, para que
    // el usuario sepa en qué unidad hay una pasada interrumpida que reanudar.
    let pend = list_pending(&o.log_dir);
    if !pend.is_empty() {
        println!();
        println!("Pasadas interrumpidas (checkpoint):");
        for (unit, cp) in &pend {
            println!("  unidad {}: {}", unit, resume_note(cp));
        }
    }
    loop {
        println!();
        println!("1. Listar sectores escaneados");
        println!("2. Listar sectores dañados");
        println!("3. Listar sectores recuperados");
        println!("4. Listar sectores lentos por banda");
        println!("5. Borrar estadísticas y checkpoints del mapa de unidades");
        println!("6. Volver");
        let choice = match ask(reader, "Estadísticas [1-6]: ") {
            Some(c) => c,
            None => return,
        };
        match choice.trim() {
            "1" => list_flag(&entries, &[Flag::S, Flag::L]),
            "2" => list_flag(&entries, &[Flag::B]),
            "3" => list_flag(&entries, &[Flag::R]),
            "4" => list_flag(&entries, &[Flag::T, Flag::U, Flag::V, Flag::X]),
            "5" => {
                let ok = match ask(
                    reader,
                    "¿Borrar DISK*.LOG y sus checkpoints DISKn.state (se conservan las copias rotadas .001-.005)? (y/N): ",
                ) {
                    Some(l) => l.trim().eq_ignore_ascii_case("y"),
                    None => return,
                };
                if ok {
                    for e in &entries {
                        if let Err(err) = fs::remove_file(&e.path) {
                            eprintln!("error borrando {}: {err}", e.path.display());
                        }
                    }
                    // Un checkpoint sin su log parcial no tiene sentido: la
                    // reanudación perdería los flags ya registrados.
                    for (unit, _) in list_pending(&o.log_dir) {
                        Checkpoint::remove(&state_path(&o.log_dir, unit));
                    }
                    println!("Estadísticas y checkpoints borrados.");
                    return;
                }
            }
            "6" => return,
            _ => println!("entrada inválida (usa 1-6)"),
        }
    }
}

fn list_flag(entries: &[MapEntry], flags: &[Flag]) {
    for e in entries {
        let mut n = 0usize;
        for r in &e.log.records {
            if flags.contains(&r.flag) {
                println!("unidad {}: {}", e.unit, r.render_line());
                n += 1;
            }
        }
        if n == 0 {
            let letters: String = flags.iter().map(|f| f.char()).collect();
            println!("unidad {}: sin registros [{}]", e.unit, letters);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("regrow_menu_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts(image: PathBuf, dir: &PathBuf, id: u32) -> CommonOpts {
        CommonOpts {
            image: Some(image),
            raw: None,
            dev: None,
            start: 0,
            end: None,
            id,
            id_explicit: false,
            log_dir: dir.join("diskstats"),
            no_log: false,
            merge: false,
            delay_ms: 500,
            bands: Some([5, 20, 50, 500]),
            allow_data_loss: false,
            backup_dir: None,
            prescan: false,
            resume: false,
            max_temp: None,
            ssd_wear_ok: false,
        }
    }

    /// el menú deriva la unidad de registros del disco crudo
    /// elegido (rdisk2 ⇒ DISK2.LOG), como el CLI con --raw.
    #[test]
    fn raw_unit_number_mapea_nombres() {
        assert_eq!(raw_unit_number(Path::new("/dev/rdisk2")), Some(2));
        assert_eq!(raw_unit_number(Path::new("/dev/rdisk0")), Some(0));
        assert_eq!(raw_unit_number(Path::new("/dev/sda")), Some(0));
        assert_eq!(raw_unit_number(Path::new("/dev/sdb")), Some(1));
        assert_eq!(raw_unit_number(Path::new("/dev/sdaa")), None);
        assert_eq!(raw_unit_number(Path::new("img.raw")), None);
    }

    #[test]
    fn parse_sector_mb_y_numeros() {
        assert_eq!(parse_sector("100", 1000), Some(100));
        assert_eq!(parse_sector("3M", 100_000), Some(3 * SECTORS_PER_MB));
        assert_eq!(parse_sector("2m", 1000), Some(1000)); // saturado al total
        assert_eq!(parse_sector(" 5 ", 100), Some(5));
        assert_eq!(parse_sector("", 10), None);
        assert_eq!(parse_sector("x", 10), None);
    }

    #[test]
    fn parse_target_tipos() {
        assert_eq!(
            parse_target("1"),
            Some(Target::Raw(PathBuf::from(RawDisk::native_name(1))))
        );
        assert_eq!(
            parse_target("/dev/rdisk2"),
            Some(Target::Raw(PathBuf::from("/dev/rdisk2")))
        );
        assert_eq!(
            parse_target("disk.raw"),
            Some(Target::Image(PathBuf::from("disk.raw")))
        );
        assert_eq!(parse_target("  "), None);
    }

    #[test]
    fn resolve_unit_lista_o_disco() {
        let disks = vec![NativeDisk {
            path: PathBuf::from("/dev/rdisk2"),
            sectors: 100,
            label: "rdisk2".into(),
            media: MediaKind::Unknown,
        }];
        // Índice de la lista mostrada (1-based).
        assert_eq!(
            resolve_unit("1", &disks),
            Some(Target::Raw(PathBuf::from("/dev/rdisk2")))
        );
        // Fuera de la lista: se interpreta como número de disco crudo.
        assert_eq!(resolve_unit("4", &disks), Some(Target::Raw(PathBuf::from("/dev/rdisk4"))));
        // Ruta explícita e imagen de archivo siguen funcionando.
        assert_eq!(
            resolve_unit("/dev/rdisk9", &disks),
            Some(Target::Raw(PathBuf::from("/dev/rdisk9")))
        );
        assert_eq!(
            resolve_unit("disco.img", &disks),
            Some(Target::Image(PathBuf::from("disco.img")))
        );
    }

    #[test]
    fn render_map_prioridad_r_sobre_b_y_span_u() {
        let recs = vec![
            Record { sector: 0, flag: Flag::S, run: 1 },
            Record { sector: 10, flag: Flag::B, run: 2 },
            Record { sector: 11, flag: Flag::R, run: 1 },
            Record { sector: 99, flag: Flag::L, run: 1 },
            Record { sector: 20, flag: Flag::X, run: 1 },
            Record { sector: 10, flag: Flag::X, run: 1 },
            // Bandas: T/U/V se pintan y la peor gana; B/R mandan sobre ellas.
            Record { sector: 10, flag: Flag::T, run: 1 },
            Record { sector: 30, flag: Flag::T, run: 1 },
            Record { sector: 32, flag: Flag::T, run: 1 },
            Record { sector: 32, flag: Flag::V, run: 1 },
            Record { sector: 34, flag: Flag::V, run: 1 },
            Record { sector: 34, flag: Flag::X, run: 1 },
        ];
        let cells = render_map(0, 100, &recs);
        assert_eq!(cells.len(), MAP_CELLS);
        assert_eq!(cells[0], '\u{2588}'); // '█' avance
        assert_eq!(cells[10], 'B');
        assert_eq!(cells[11], 'R'); // R gana sobre B
        assert_eq!(cells[20], 'D'); // 'D' lento (flag X)
        assert_eq!(cells[10], 'B'); // B manda sobre un D intentado encima
        assert_eq!(cells[10], 'B'); // ...y sobre una banda T  
        assert_eq!(cells[30], 'T'); // banda T1
        assert_eq!(cells[32], 'V'); // T3 gana sobre T1 en la misma celda
        assert_eq!(cells[34], 'D'); // X (T4) gana sobre V
        assert_eq!(cells[50], '\u{2588}');
        assert_eq!(cells[MAP_CELLS - 1], '\u{2591}'); // fuera del span: '░'
    }

    #[test]
    fn menu_escanea_y_panel_estadisticas() {
        let dir = temp_dir("scan");
        let img = dir.join("img.raw");
        fs::write(&img, vec![0xA5u8; 16 * 512]).unwrap();
        let o = opts(img, &dir, 77);
        // escanear (2) → rango vacío → vacío → estadísticas (5) → listar
        // escaneados (1) → volver (6) → salir (6).
        let mut input = std::io::Cursor::new(b"2\n\n\n5\n1\n6\n6\n");
        let code = run_with(&mut input, &o);
        assert_eq!(code, ExitCode::SUCCESS);
        let bytes = fs::read(o.log_dir.join("DISK77.LOG")).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("0=S1"));
        assert!(text.contains("15=L1"));
        assert!(o.log_dir.join("disklist.cfg").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn menu_regenerate_declinado_no_escribe() {
        let dir = temp_dir("regen");
        let img = dir.join("img.raw");
        fs::write(&img, vec![0xA5u8; 8 * 512]).unwrap();
        let o = opts(img.clone(), &dir, 78);
        let mut input = std::io::Cursor::new(b"4\nn\n6\n");
        let code = run_with(&mut input, &o);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(fs::read(&img).unwrap(), vec![0xA5u8; 8 * 512]);
        assert!(!o.log_dir.join("DISK78.LOG").exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// Refresco (opción 3 del menú) sobre medio sano: no escribe nada
    /// (ningún sector supera la banda T1) pero registra el par S/L.
    #[test]
    fn menu_refrescar_sanos_no_escribe() {
        let dir = temp_dir("refr");
        let img = dir.join("img.raw");
        fs::write(&img, vec![0xA5u8; 8 * 512]).unwrap();
        let o = opts(img.clone(), &dir, 79);
        // 3 = refrescar; rango vacío (inicio y fin por defecto); 6 = salir.
        let mut input = std::io::Cursor::new(b"3\n\n\n6\n");
        let code = run_with(&mut input, &o);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(fs::read(&img).unwrap(), vec![0xA5u8; 8 * 512]);
        let t = fs::read_to_string(o.log_dir.join("DISK79.LOG")).unwrap();
        assert!(t.contains("0=S1") && t.contains("7=L1"), "log del refresco: {t}");
        assert!(!t.contains("=T1") && !t.contains("=X1"), "sin bandas: {t}");
        fs::remove_dir_all(&dir).ok();
    }

    /// Checkpoint de una pasada de ESCANEO cortada en 49): el
    /// menú lo detecta al abrir la unidad, ofrece reanudar al elegir ESCANEO,
    /// y la pasada continuada cierra el rango y fusiona los flags previos.
    #[test]
    fn menu_reanuda_desde_checkpoint() {
        use crate::engine::EngineStats;
        let dir = temp_dir("resume");
        let img = dir.join("img.raw");
        fs::write(&img, vec![0xA5u8; 100 * 512]).unwrap();
        let o = opts(img.clone(), &dir, 42);
        fs::create_dir_all(&o.log_dir).unwrap();
        let cp = Checkpoint {
            mode: RunMode::Scan,
            start: 0,
            end: 100,
            last_sector: 49,
            stats: EngineStats {
                scanned: 50,
                ..Default::default()
            },
            unit: 42,
        };
        cp.save(&state_path(&o.log_dir, 42)).unwrap();
        fs::write(
            &o.log_dir.join("DISK42.LOG"),
            "0=S1\r\n10=B2\r\n\r\nDISK 42: 50 Kb in 100 sectors\r\nCreated=X",
        )
        .unwrap();
        // 2 = escanear; línea vacía = aceptar la oferta de reanudar (default Y);
        // luego estadísticas (5), listar (1), volver (6), salir (6).
        let mut input = std::io::Cursor::new(b"2\n\n5\n1\n6\n6\n");
        let code = run_with(&mut input, &o);
        assert_eq!(code, ExitCode::SUCCESS);
        let t = fs::read_to_string(o.log_dir.join("DISK42.LOG")).unwrap();
        assert!(t.contains("0=S1"), "span histórico preservado: {t}");
        assert!(t.contains("10=B2"), "flags de la pasada cortada: {t}");
        assert!(t.contains("99=L1"), "la pasada reanudada cierra con L: {t}");
        assert!(!t.contains("50=S1"), "un solo borde S: {t}");
        // Pasada completa: el checkpoint ya no hace falta.
        assert!(!state_path(&o.log_dir, 42).exists());
        fs::remove_dir_all(&dir).ok();
    }

    /// Rechazada la oferta, el menú escanea desde cero y la rotación conserva
    /// el log parcial anterior en .001 (el checkpoint se reemplaza/borra).
    #[test]
    fn menu_declina_reanudar_y_rota_el_log_parcial() {
        use crate::engine::EngineStats;
        let dir = temp_dir("nore");
        let img = dir.join("img.raw");
        fs::write(&img, vec![0xA5u8; 100 * 512]).unwrap();
        let o = opts(img.clone(), &dir, 43);
        fs::create_dir_all(&o.log_dir).unwrap();
        Checkpoint {
            mode: RunMode::Scan,
            start: 0,
            end: 100,
            last_sector: 49,
            stats: EngineStats {
                scanned: 50,
                ..Default::default()
            },
            unit: 43,
        }
        .save(&state_path(&o.log_dir, 43))
        .unwrap();
        fs::write(
            &o.log_dir.join("DISK43.LOG"),
            "0=S1\r\n10=B2\r\n\r\nDISK 43: 50 Kb in 100 sectors\r\nCreated=X",
        )
        .unwrap();
        // 2 = escanear; "n" = no reanudar; rango vacío (dos líneas en blanco); 6 = salir.
        let mut input = std::io::Cursor::new(b"2\nn\n\n\n6\n");
        let code = run_with(&mut input, &o);
        assert_eq!(code, ExitCode::SUCCESS);
        let t = fs::read_to_string(o.log_dir.join("DISK43.LOG")).unwrap();
        assert!(t.contains("0=S1") && t.contains("99=L1"), "pasada nueva: {t}");
        assert!(!t.contains("10=B2"), "sin merge: {t}");
        let t001 = fs::read_to_string(o.log_dir.join("DISK43.001")).unwrap();
        assert!(t001.contains("10=B2"), "el log cortado queda en .001: {t001}");
        assert!(!state_path(&o.log_dir, 43).exists());
        fs::remove_dir_all(&dir).ok();
    }
}
