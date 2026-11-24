// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Operaciones entre mapfiles al estilo `ddrescuelog`: fusionar, contrastar,
//! transformar e importar/exportar listas y bitmaps — sin tocar jamás un
//! disco ni una imagen.
//!
//! Semántica de las fusiones (leyenda propia: `+` terminado, `-` pendiente,
//! `?` agotado): los conjuntos `+` se combinan con la operación de conjuntos
//! pedida; los sectores que no resultan `+` quedan pendientes — `?` si algún
//! mapa de la operación los daba por agotados, `-` en otro caso. Todos los
//! operandos deben compartir dominio (para comparar mapas de desplazamientos
//! distintos, `shift` primero).
//!
//! Códigos de salida a la altura de `ddrescuelog`: `diff` sale 0 si los
//! mapas son iguales y 1 si difieren; `done` sale 0 si el dominio está
//!enteramente terminado.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::mapfile::{crc32, MapFile, MapStatus};

const SECTOR_BYTES: u64 = crate::source::SECTOR_SIZE as u64;

// ---------------------------------------------------------------- cobertura --

/// Cobertura completa del dominio: los huecos entre rangos se materializan
/// como `-` (pendiente sin intentar), que es exactamente cómo los entiende
/// el motor de rescate.
fn coverage(m: &MapFile) -> Vec<(u64, u64, MapStatus)> {
    let mut out = Vec::with_capacity(m.entries().len() + 1);
    let mut pos = 0u64;
    for e in m.entries() {
        if e.start > pos {
            out.push((pos, e.start - pos, MapStatus::Failed));
        }
        out.push((e.start, e.len, e.status));
        pos = e.start + e.len;
    }
    if pos < m.sector_count {
        out.push((pos, m.sector_count - pos, MapStatus::Failed));
    }
    out
}

// ------------------------------------------------------------- utilidades ----

/// Áreas de daño: corridas contiguas de sectores no terminados agrupadas por
/// **estado** (decisión D-A1 de la spec 001: un `?` contiguo a un `-` forman
/// dos áreas distintas; lo mismo contiguo se fusiona). Los huecos sin mapear
/// valen por `-`. Ordenadas por posición; su índice +1 es el id del área.
pub fn damage_areas(m: &MapFile) -> Vec<(u64, u64, MapStatus)> {
    let mut out: Vec<(u64, u64, MapStatus)> = Vec::new();
    for (s, l, st) in coverage(m) {
        if st == MapStatus::Good {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.2 == st && last.0 + last.1 == s {
                last.1 += l;
                continue;
            }
        }
        out.push((s, l, st));
    }
    out
}

fn gib_of(sectors: u64) -> String {
    format!("{:.2} GiB", (sectors as f64) * SECTOR_BYTES as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64) * 100.0 / (total as f64)
    }
}

/// Resumen humano del mapa: conteos por estado con porcentaje y tamaño.
pub fn summary_lines(m: &MapFile) -> Vec<String> {
    let (good, failed, scraped, untried) = m.stats();
    let areas = damage_areas(m);
    let a_scraped = areas.iter().filter(|a| a.2 == MapStatus::Scraped).count();
    let a_failed = areas.len() - a_scraped;
    vec![
        format!(
            "dominio: {} sectores = {} · {} rangos",
            m.sector_count,
            gib_of(m.sector_count),
            m.entries().len()
        ),
        format!("  + terminados : {:>12} ({:6.2} %)", good, pct(good, m.sector_count)),
        format!(
            "  - pendientes : {:>12} (explícitos {:>12} + huecos sin mapear {:>12})",
            failed + untried,
            failed,
            untried
        ),
        format!("  ? agotados   : {:>12} ({:6.2} %)", scraped, pct(scraped, m.sector_count)),
        format!(
            "  áreas de daño: {:>12} (pendientes {} · agotadas {})",
            areas.len(),
            a_failed,
            a_scraped
        ),
    ]
}

/// Dominio enteramente `+` (sin huecos ni pendientes).
pub fn is_done(m: &MapFile) -> bool {
    m.stats().0 == m.sector_count
}

// ------------------------------------------------------------------ merge ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeMode {
    /// `+` en cualquiera de los mapas → `+` (lo rescatado entre dos pasadas).
    Or,
    /// `+` solo si los dos mapas lo confirman.
    And,
    /// `+` en exactamente uno: los desacuerdos entre mapas.
    Xor,
}

fn combine_two(a: &[(u64, u64, MapStatus)], b: &[(u64, u64, MapStatus)], total: u64, mode: MergeMode) -> Vec<(u64, u64, MapStatus)> {
    let mut out: Vec<(u64, u64, MapStatus)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let mut pos = 0u64;
    while pos < total {
        let (a_start, a_len, a_st) = a[i];
        let (b_start, b_len, b_st) = b[j];
        let end = (a_start + a_len).min(b_start + b_len);
        let ga = a_st == MapStatus::Good;
        let gb = b_st == MapStatus::Good;
        let good = match mode {
            MergeMode::Or => ga || gb,
            MergeMode::And => ga && gb,
            MergeMode::Xor => ga != gb,
        };
        let st = if good {
            MapStatus::Good
        } else if a_st == MapStatus::Scraped || b_st == MapStatus::Scraped {
            MapStatus::Scraped
        } else {
            MapStatus::Failed
        };
        if let Some(last) = out.last_mut() {
            if last.2 == st && last.0 + last.1 == pos {
                last.1 += end - pos;
                pos = end;
                if pos >= a_start + a_len {
                    i += 1;
                }
                if pos >= b_start + b_len {
                    j += 1;
                }
                continue;
            }
        }
        out.push((pos, end - pos, st));
        pos = end;
        if pos >= a_start + a_len {
            i += 1;
        }
        if pos >= b_start + b_len {
            j += 1;
        }
    }
    out
}

/// Fusiona dos o más mapas (mismo dominio) con la operación pedida.
pub fn merge(maps: &[MapFile], mode: MergeMode) -> Result<MapFile, String> {
    if maps.len() < 2 {
        return Err("la fusión necesita al menos dos mapfiles".into());
    }
    let dom = maps[0].sector_count;
    for m in maps.iter().skip(1) {
        if m.sector_count != dom {
            return Err(format!(
                "dominios distintos ({} vs {}): usa `shift` antes de fusionar",
                dom, m.sector_count
            ));
        }
    }
    let mut cur = coverage(&maps[0]);
    for m in maps.iter().skip(1) {
        cur = combine_two(&cur, &coverage(m), dom, mode);
    }
    let mut out = MapFile::new(dom);
    for (s, l, st) in cur {
        out.set(s, l, st, 0);
    }
    Ok(out)
}

// ------------------------------------------------------------------- diff ----

/// Rangos donde dos mapas discrepan (mismo dominio). `finished_only` compara
/// solo el conjunto terminado: un sector `?` contra un `-` no cuenta como
/// diferencia, solo importa quién es `+`.
pub fn diff(a: &MapFile, b: &MapFile, finished_only: bool) -> Result<Vec<(u64, u64, MapStatus, MapStatus)>, String> {
    if a.sector_count != b.sector_count {
        return Err(format!(
            "dominios distintos ({} vs {}): usa `shift` antes de comparar",
            a.sector_count, b.sector_count
        ));
    }
    let ca = coverage(a);
    let cb = coverage(b);
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let mut pos = 0u64;
    while pos < a.sector_count {
        let (a_start, a_len, a_st) = ca[i];
        let (b_start, b_len, b_st) = cb[j];
        let end = (a_start + a_len).min(b_start + b_len);
        let diverge = if finished_only {
            (a_st == MapStatus::Good) != (b_st == MapStatus::Good)
        } else {
            a_st != b_st
        };
        if diverge {
            out.push((pos, end - pos, a_st, b_st));
        }
        pos = end;
        if pos >= a_start + a_len {
            i += 1;
        }
        if pos >= b_start + b_len {
            j += 1;
        }
    }
    Ok(out)
}

// ------------------------------------------------------ transformaciones ----

/// `invert`: lo terminado pasa a pendiente y todo lo demás (incluidos los
/// huecos) pasa a terminado. Sirve para convertir un mapa de "lo bueno" en
/// un mapa de "lo que queda por hacer" (o al revés).
pub fn invert(m: &MapFile) -> MapFile {
    let mut out = MapFile::new(m.sector_count);
    for (s, l, st) in coverage(m) {
        let nst = if st == MapStatus::Good { MapStatus::Failed } else { MapStatus::Good };
        out.set(s, l, nst, 0);
    }
    out
}

/// `complete`: rellena los huecos sin mapear con el estado dado, dejando los
/// rangos explícitos intactos (con sus reintentos).
pub fn complete(m: &MapFile, fill: MapStatus) -> MapFile {
    let mut out = MapFile::new(m.sector_count);
    let mut pos = 0u64;
    for e in m.entries() {
        if e.start > pos {
            out.set(pos, e.start - pos, fill, 0);
        }
        out.set(e.start, e.len, e.status, e.retries);
        pos = e.start + e.len;
    }
    if pos < m.sector_count {
        out.set(pos, m.sector_count - pos, fill, 0);
    }
    out
}

/// `retype`: cambia el estado de todos los sectores cuyo estado esté en
/// `from` (los huecos cuentan como `-`) al estado `to`. Lo demás conserva su
/// estado y sus reintentos.
pub fn retype(m: &MapFile, from: &[MapStatus], to: MapStatus) -> MapFile {
    let mut out = MapFile::new(m.sector_count);
    for (s, l, st) in coverage(m) {
        if from.contains(&st) {
            out.set(s, l, to, 0);
        } else {
            out.set(s, l, st, m.retries_at(s));
        }
    }
    out
}

/// `shift`: desplaza todos los rangos `delta` sectores (positivo o negativo)
/// y recorta lo que sale del dominio. El dominio no cambia de tamaño.
pub fn shift(m: &MapFile, delta: i64) -> MapFile {
    let mut out = MapFile::new(m.sector_count);
    for e in m.entries() {
        let ns = e.start as i64 + delta;
        let a = ns.max(0) as u64;
        let b = (ns + e.len as i64).min(m.sector_count as i64) as u64;
        if a < b {
            out.set(a, b - a, e.status, e.retries);
        }
    }
    out
}

// --------------------------------------------------------------- annotate ----

/// Tamaño humano de un número de sectores (512 B).
fn human_of(sectors: u64) -> String {
    const K: u64 = 1024;
    let b = sectors * SECTOR_BYTES;
    if b < K {
        format!("{b} B")
    } else if b < K * K {
        format!("{:.1} KiB", b as f64 / K as f64)
    } else if b < K * K * K {
        format!("{:.1} MiB", b as f64 / (K * K) as f64)
    } else {
        format!("{:.2} GiB", b as f64 / (K * K * K) as f64)
    }
}

/// Como `coverage`, pero distinguiendo rangos explícitos (`Some(reintentos)`)
/// de huecos sin mapear (`None`), para anotar unos y otros por separado.
fn coverage_detailed(m: &MapFile) -> Vec<(u64, u64, MapStatus, Option<u8>)> {
    let mut out = Vec::with_capacity(m.entries().len() + 1);
    let mut pos = 0u64;
    for e in m.entries() {
        if e.start > pos {
            out.push((pos, e.start - pos, MapStatus::Failed, None));
        }
        out.push((e.start, e.len, e.status, Some(e.retries)));
        pos = e.start + e.len;
    }
    if pos < m.sector_count {
        out.push((pos, m.sector_count - pos, MapStatus::Failed, None));
    }
    out
}

/// Anotación legible por humanos del mapfile (spec 001): cabecera con
/// totales y áreas de daño, y una línea por tramo con las mismas columnas
/// que `export_text` — de modo que el archivo anotado sigue siendo un
/// mapfile de texto válido para ddrescue y para `maps import` (los
/// comentarios se ignoran al importar). `now` entra ya formateada y
/// inyectada: el cuerpo es determinista.
pub fn annotate_text(m: &MapFile, now: &str) -> String {
    let (good, failed, scraped, untried) = m.stats();
    let total = m.sector_count;
    let areas = damage_areas(m);
    let a_scraped = areas.iter().filter(|a| a.2 == MapStatus::Scraped).count();
    let a_failed = areas.len() - a_scraped;
    let mut s = String::new();
    s.push_str(&format!(
        "# Mapfile anotado por regrow — {now} (copia de lectura; el maestro no se toca)\n"
    ));
    s.push_str(&format!(
        "# Dominio: {total} sectores = {} · {} rangos\n",
        gib_of(total),
        m.entries().len()
    ));
    s.push_str(&format!(
        "#   + terminados: {good} ({:.2} %) · - pendientes: {} · ? agotados: {scraped} ({:.2} %)\n",
        pct(good, total),
        failed + untried,
        pct(scraped, total)
    ));
    s.push_str(&format!(
        "#   áreas de daño: {} (pendientes {a_failed} · agotadas {a_scraped}){}\n",
        areas.len(),
        if areas.is_empty() {
            " — rescate terminado"
        } else {
            ""
        }
    ));
    s.push_str("#      pos        size  status\n");
    let mut ai = 0usize;
    for (start, len, st, retries) in coverage_detailed(m) {
        // El cursor de áreas avanza monótonamente con la posición (ambas
        // listas se construyen en el mismo orden).
        let area_id: Option<usize> = if st == MapStatus::Good || areas.is_empty() {
            None
        } else {
            while ai + 1 < areas.len() && areas[ai].0 + areas[ai].1 <= start {
                ai += 1;
            }
            if areas[ai].0 <= start && start < areas[ai].0 + areas[ai].1 {
                Some(ai + 1)
            } else {
                None
            }
        };
        let label = match st {
            MapStatus::Good => "terminado",
            MapStatus::Scraped => "agotado",
            MapStatus::Failed if retries.is_some() => "pendiente",
            MapStatus::Failed => "sin mapear",
        };
        let mut line = format!(
            "0x{:08x}  0x{:08x}  {}   # {label} · {} ({:.2} %)",
            start * SECTOR_BYTES,
            len * SECTOR_BYTES,
            st.char(),
            human_of(len),
            pct(len, total),
        );
        if let Some(n) = area_id {
            line.push_str(&format!(" · área {n}"));
        }
        if let Some(r) = retries {
            if r > 0 {
                line.push_str(&format!(" · reintentos {r}"));
            }
        }
        s.push_str(&line);
        s.push('\n');
    }
    s
}

// ------------------------------------------------- listas y bitmaps externo --

/// Lista de bloques al estilo badblocks: un rango por línea `inicio-fin`
/// (inclusive) para los sectores con alguno de los estados pedidos. Tramos
/// contiguos de estados pedidos se emiten como un solo rango (p. ej. un
/// `-` seguido de un `?` cuenta como una sola zona pendiente).
pub fn list_text(m: &MapFile, wanted: &[MapStatus]) -> String {
    let mut s = String::new();
    let mut run_start: Option<u64> = None;
    let mut run_end = 0u64;
    for (start, len, st) in coverage(m) {
        if wanted.contains(&st) {
            match run_start {
                Some(_) => run_end = start + len,
                None => {
                    run_start = Some(start);
                    run_end = start + len;
                }
            }
            continue;
        }
        if let Some(a) = run_start.take() {
            s.push_str(&format!("{}-{}\n", a, run_end - 1));
        }
    }
    if let Some(a) = run_start {
        s.push_str(&format!("{}-{}\n", a, run_end - 1));
    }
    s
}

/// Bitmap de 1 bit por sector: bit 1 = el sector tiene uno de los estados
/// pedidos. `big_endian` ordena los bits de cada byte del bit más
/// significativo al menos significativo (como los bitmap-be de ddrescuelog);
/// en caso contrario, al revés (bitmap-le).
pub fn bitmap_bytes(m: &MapFile, wanted: &[MapStatus], big_endian: bool) -> Vec<u8> {
    let n = (m.sector_count as usize).div_ceil(8);
    let mut bits = vec![0u8; n];
    for (start, len, st) in coverage(m) {
        if wanted.contains(&st) {
            for sector in start..start + len {
                let byte = (sector / 8) as usize;
                let bit = sector % 8;
                let mask = if big_endian { 1u8 << (7 - bit) } else { 1u8 << bit };
                bits[byte] |= mask;
            }
        }
    }
    bits
}

/// Crea un mapa desde una lista estilo badblocks (`inicio-fin` o un número
/// suelto, por línea o separados por comas): lo listado queda `-` (pendiente)
/// y el resto del dominio, `+`. El dominio debe declararse (una lista de
/// bloques malos no dice el tamaño del disco).
pub fn from_list(text: &str, domain: u64) -> Result<MapFile, String> {
    if domain == 0 {
        return Err("el dominio de una lista debe declararse con --domain N".into());
    }
    let mut m = MapFile::new(domain);
    m.set(0, domain, MapStatus::Good, 0);
    let mut listed = 0u64;
    let mut bad = String::new();
    for token in text.split([',', ' ', '\t', '\n', '\r']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (a, b) = match token.split_once('-') {
            Some((a, b)) => (a.trim(), Some(b.trim())),
            None => (token, None),
        };
        let Ok(start) = a.parse::<u64>() else {
            bad = token.to_string();
            break;
        };
        let end = match b {
            Some(b) => match b.parse::<u64>() {
                Ok(b) => b,
                Err(_) => {
                    bad = token.to_string();
                    break;
                }
            },
            None => start,
        };
        if start > end || end >= domain {
            bad = token.to_string();
            break;
        }
        m.set(start, end - start + 1, MapStatus::Failed, 0);
        listed += end - start + 1;
    }
    if !bad.is_empty() {
        return Err(format!("entrada inválida en la lista: '{bad}' (se esperan bloques o rangos 'inicio-fin')"));
    }
    if listed == 0 {
        return Err("la lista no contiene ningún bloque del dominio".into());
    }
    Ok(m)
}

/// Crea un mapa desde un bitmap de 1 bit por sector (1 = bloque marcado →
/// `-` pendiente, 0 → `+`). Si `domain` es 0 se asume que cada byte aporta
/// 8 sectores.
pub fn from_bitmap(bytes: &[u8], domain: u64, big_endian: bool) -> Result<MapFile, String> {
    if bytes.is_empty() {
        return Err("bitmap vacío".into());
    }
    let bits_available = bytes.len() as u64 * 8;
    let dom = if domain == 0 { bits_available } else { domain };
    if dom > bits_available {
        return Err(format!(
            "el bitmap cubre {} sectores pero el dominio declarado es {}",
            bits_available, dom
        ));
    }
    let mut m = MapFile::new(dom);
    m.set(0, dom, MapStatus::Good, 0);
    let mut pos = 0u64;
    while pos < dom {
        let byte = (pos / 8) as usize;
        let bit = pos % 8;
        let mask = if big_endian { 1u8 << (7 - bit) } else { 1u8 << bit };
        let marked = bytes[byte] & mask != 0;
        let mut end = pos + 1;
        while end < dom {
            let ebyte = (end / 8) as usize;
            let ebit = end % 8;
            let emask = if big_endian { 1u8 << (7 - ebit) } else { 1u8 << ebit };
            if (bytes[ebyte] & emask != 0) != marked {
                break;
            }
            end += 1;
        }
        if marked {
            m.set(pos, end - pos, MapStatus::Failed, 0);
        }
        pos = end;
    }
    Ok(m)
}

pub fn parse_status_char(c: char) -> Option<MapStatus> {
    MapStatus::from_char(c)
}

/// Dominio mínimo sugerido por un mapfile de texto: el mayor fin de rango
/// que contiene (en sectores). Sirve para convertir mapas de texto sin
/// declarar el tamaño del disco; el dominio real sigue siendo decisión del
/// operador (con `--domain`).
pub fn min_domain_from_text(text: &str) -> Option<u64> {
    let legacy_sectors = text
        .lines()
        .any(|l| l.starts_with("# domain:") && l.contains("sectores de 512 B"));
    let mut max_end = 0u64;
    for line in text.lines() {
        let line = match line.split_once('#') {
            Some((before, _)) => before,
            None => line,
        };
        let cols: Vec<&str> = line.trim().split_whitespace().collect();
        if cols.len() != 3 {
            continue;
        }
        let Ok(pos) = u64::from_str_radix(cols[0].trim_start_matches("0x").trim_start_matches("0X"), 16) else {
            continue;
        };
        let Ok(size) = u64::from_str_radix(cols[1].trim_start_matches("0x").trim_start_matches("0X"), 16) else {
            continue;
        };
        let end = if legacy_sectors {
            pos + size
        } else {
            if pos % SECTOR_BYTES != 0 || size % SECTOR_BYTES != 0 {
                continue;
            }
            (pos + size) / SECTOR_BYTES
        };
        max_end = max_end.max(end);
    }
    if max_end > 0 {
        Some(max_end)
    } else {
        None
    }
}

// ----------------------------------------------------------------- CLI -------

fn maps_usage() -> ExitCode {
    eprintln!(
        "uso: regrow maps <orden> [mapfiles] [opciones]\n\
         \x20 status <map>                     resumen por estado (conteos, %, tamaños, áreas de daño)\n\
         \x20 done <map>                       sale 0 si el dominio está enteramente terminado\n\
         \x20 annotate <map> --out <archivo>   copia de texto comentada (cabecera con totales y\n\
         \x20     áreas; líneas legibles por humanos que siguen siendo mapfile de texto válido;\n\
         \x20     jamás reescribe la entrada)\n\
         \x20 diff <mapA> <mapB> [--finished-only]   contrastar; sale 0 si iguales, 1 si difieren\n\
         \x20 merge --mode <or|and|xor> <mapA> <mapB> […] --out <map>   fusionar lo terminado\n\
         \x20 invert <map> --out <map>         lo terminado ↔ lo pendiente\n\
         \x20 complete <map> --out <map> [--type +|-|?]   rellenar huecos sin mapear (def -)\n\
         \x20 retype <map> --out <map> --from \"<-?>\" --to <+|-|?>   cambiar estados\n\
         \x20 shift <map> --out <map> --shift N   desplazar rangos N sectores (recorta)\n\
         \x20 list <map> [--types \"-|+|?…\"] [--format list|bitmap-be|bitmap-le] [--out F]\n\
         \x20     exportar bloques (lista estilo badblocks 'inicio-fin' o bitmap 1 bit/sector)\n\
         \x20 from-list <archivo> --out <map> [--format list|bitmap-be|bitmap-le] [--domain N]\n\
         \x20     crear mapa desde una lista badblocks o un bitmap (lo listado queda pendiente)\n\
         \x20 import <map-texto> --out <map> [--domain N]\n\
         \x20     convertir un mapfile de texto (ddrescue o exportación propia) a binario;\n\
         \x20     sin --domain se usa el dominio mínimo sugerido por el propio texto\n\
         \x20 export <map> --out <archivo-texto>   exportar al formato de texto de ddrescue\n\
         \x20 dfxml <map> --out <archivo.xml> [--source ETIQUETA]\n\
         \x20     exportar evidencia forense DFXML: un byte_run por tramo (offsets en\n\
         \x20     bytes, estado finished/pending/exhausted, huecos como pendientes) más\n\
         \x20     tamaño de sector y huella CRC32 del mapa\n\
         Ningún orden toca discos ni imágenes: operan solo sobre mapfiles."
    );
    ExitCode::from(2)
}

fn take_out(args: &[String], i: &mut usize) -> Option<PathBuf> {
    if *i + 1 < args.len() {
        *i += 1;
        Some(PathBuf::from(&args[*i]))
    } else {
        None
    }
}

fn err(msg: String) -> ExitCode {
    eprintln!("error: {msg}");
    ExitCode::from(2)
}

/// Escritura atómica de un archivo de texto derivado: temporal en el mismo
/// directorio + `fsync` + rename, el mismo patrón del mapfile binario. Un
/// corte jamás deja un anotado a medias ni un `.tmp` engañoso.
fn write_text_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp_os = path.as_os_str().to_os_string();
    tmp_os.push(".tmp");
    let tmp = PathBuf::from(&tmp_os);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// ¿Dos rutas refieren al mismo archivo? (para rechazar operaciones in situ).
fn same_path(a: &Path, b: &Path) -> bool {
    let ra = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let rb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ra == rb
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 => out.push_str(&format!("&#{};", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// DFXML (Digital Forensics XML) derivado del mapfile: un `byte_run` por cada
/// tramo contiguo del dominio — los rangos del mapa con su estado y los
/// huecos como pendientes. Los offsets y longitudes van en bytes. Tokens de
/// estado en inglés (finished/pending/exhausted) para que los consuman
/// herramientas forenses externas. Jamás toca el mapfile.
pub fn dfxml_document(m: &MapFile, source_label: &str) -> String {
    let map_crc = crc32(&m.serialize());
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<dfxml xmlns=\"https://www.dcilab.org/xmlns/dfxml\">\n");
    out.push_str("  <fileobject>\n");
    out.push_str(&format!("    <filename>{}</filename>\n", xml_escape(source_label)));
    out.push_str(&format!(
        "    <filesize>{}</filesize>\n",
        m.sector_count.saturating_mul(crate::source::SECTOR_SIZE as u64)
    ));
    out.push_str("    <byte_runs>\n");
    let mut pos = 0u64;
    for e in m.entries() {
        if e.start > pos {
            out.push_str(&dfxml_run(pos, e.start - pos, "pending"));
        }
        let st = match e.status {
            MapStatus::Good => "finished",
            MapStatus::Failed => "pending",
            MapStatus::Scraped => "exhausted",
        };
        out.push_str(&dfxml_run(e.start, e.len, st));
        pos = e.start.saturating_add(e.len);
    }
    if pos < m.sector_count {
        out.push_str(&dfxml_run(pos, m.sector_count - pos, "pending"));
    }
    out.push_str("    </byte_runs>\n  </fileobject>\n");
    out.push_str("  <processing>\n");
    out.push_str(&format!(
        "    <machine>regrow {}</machine>\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str("    <sector_size>512</sector_size>\n");
    out.push_str(&format!("    <mapfile_crc32>{map_crc:08x}</mapfile_crc32>\n"));
    out.push_str("  </processing>\n</dfxml>\n");
    out
}

fn dfxml_run(start: u64, len: u64, status: &str) -> String {
    format!(
        "      <byte_run offset=\"{}\" length=\"{}\" status=\"{}\"/>\n",
        start.saturating_mul(crate::source::SECTOR_SIZE as u64),
        len.saturating_mul(crate::source::SECTOR_SIZE as u64),
        status
    )
}

pub fn cli_maps(args: &[String]) -> ExitCode {
    let Some(op) = args.first() else { return maps_usage() };
    match op.as_str() {
        "status" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            match MapFile::load(&PathBuf::from(path)) {
                Ok(m) => {
                    for line in summary_lines(&m) {
                        println!("{line}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => err(e),
            }
        }
        "done" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            match MapFile::load(&PathBuf::from(path)) {
                Ok(m) => {
                    if is_done(&m) {
                        println!("rescate terminado: {} sectores al 100 %", m.sector_count);
                        ExitCode::SUCCESS
                    } else {
                        let (g, _, _, _) = m.stats();
                        println!(
                            "rescate incompleto: {}/{} sectores terminados ({:.2} %)",
                            g,
                            m.sector_count,
                            pct(g, m.sector_count)
                        );
                        ExitCode::from(1)
                    }
                }
                Err(e) => err(e),
            }
        }
        "diff" => {
            let (Some(pa), Some(pb)) = (args.get(1), args.get(2)) else { return maps_usage() };
            let finished_only = args.iter().any(|a| a == "--finished-only");
            let a = match MapFile::load(&PathBuf::from(pa)) {
                Ok(m) => m,
                Err(e) => return err(e),
            };
            let b = match MapFile::load(&PathBuf::from(pb)) {
                Ok(m) => m,
                Err(e) => return err(e),
            };
            let ranges = match diff(&a, &b, finished_only) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            if ranges.is_empty() {
                println!("mapas iguales en {} sectores", a.sector_count);
                return ExitCode::SUCCESS;
            }
            let shown = ranges.iter().take(40);
            for (start, len, sa, sb) in shown {
                println!(
                    "  {}..{} ({} sectores): '{}' → '{}'",
                    start,
                    start + len - 1,
                    len,
                    sa.char(),
                    sb.char()
                );
            }
            if ranges.len() > 40 {
                println!("  … y {} tramos más", ranges.len() - 40);
            }
            let total: u64 = ranges.iter().map(|r| r.1).sum();
            println!("mapas distintos: {} sectores en {} tramos", total, ranges.len());
            ExitCode::from(1)
        }
        "merge" => {
            let mut mode = None;
            let mut out = None;
            let mut maps: Vec<PathBuf> = Vec::new();
            let mut i = 1usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--mode" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        mode = Some(match v.as_str() {
                            "or" => MergeMode::Or,
                            "and" => MergeMode::And,
                            "xor" => MergeMode::Xor,
                            _ => return err(format!("modo de fusión desconocido '{v}' (or|and|xor)")),
                        });
                        i += 1;
                    }
                    "--out" => out = take_out(args, &mut i),
                    _ => maps.push(PathBuf::from(&args[i])),
                }
                i += 1;
            }
            let Some(mode) = mode else {
                return err("falta --mode <or|and|xor>".into());
            };
            let Some(out) = out else { return err("falta --out <mapfile destino>".into()) };
            if maps.len() < 2 {
                return err("la fusión necesita al menos dos mapfiles".into());
            }
            let loaded: Vec<MapFile> = match maps.iter().map(|p| MapFile::load(p)).collect::<Result<Vec<_>, _>>() {
                Ok(v) => v,
                Err(e) => return err(e),
            };
            match merge(&loaded, mode) {
                Ok(m) => {
                    match m.save(&out) {
                        Ok(()) => {
                            let (g, _, _, _) = m.stats();
                            println!(
                                "fusión ({}): {} sectores terminados de {} — guardado en {}",
                                format!("{:?}", mode).to_lowercase(),
                                g,
                                m.sector_count,
                                out.display()
                            );
                            ExitCode::SUCCESS
                        }
                        Err(e) => err(format!("no se puede escribir {}: {e}", out.display())),
                    }
                }
                Err(e) => err(e),
            }
        }
        "invert" | "complete" | "retype" | "shift" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            let mut out: Option<PathBuf> = None;
            let mut fill = MapStatus::Failed;
            let mut from: Vec<MapStatus> = Vec::new();
            let mut to: Option<MapStatus> = None;
            let mut delta: Option<i64> = None;
            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--out" => out = take_out(args, &mut i),
                    "--type" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        let c = v.chars().next().unwrap_or(' ');
                        let Some(st) = parse_status_char(c) else {
                            return err(format!("estado desconocido '{v}' (+|-|?)"));
                        };
                        fill = st;
                        i += 1;
                    }
                    "--from" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        for c in v.chars() {
                            let Some(st) = parse_status_char(c) else {
                                return err(format!("estado desconocido '{c}' en --from"));
                            };
                            from.push(st);
                        }
                        i += 1;
                    }
                    "--to" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        let c = v.chars().next().unwrap_or(' ');
                        let Some(st) = parse_status_char(c) else {
                            return err(format!("estado desconocido '{v}' (+|-|?)"));
                        };
                        to = Some(st);
                        i += 1;
                    }
                    "--shift" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        match v.parse::<i64>() {
                            Ok(n) => delta = Some(n),
                            Err(_) => return err(format!("--shift necesita un entero de desplazamiento, no '{v}'")),
                        }
                        i += 1;
                    }
                    _ => return err(format!("opción no reconocida '{}'", args[i])),
                }
                i += 1;
            }
            let Some(out) = out else { return err("falta --out <mapfile destino>".into()) };
            let m = match MapFile::load(&PathBuf::from(path)) {
                Ok(m) => m,
                Err(e) => return err(e),
            };
            let result = match op.as_str() {
                "invert" => invert(&m),
                "complete" => complete(&m, fill),
                "retype" => {
                    let Some(to) = to else { return err("retype necesita --from \"…\" y --to <+|-|?>".into()) };
                    if from.is_empty() {
                        return err("retype necesita --from \"…\" con los estados a cambiar".into());
                    }
                    retype(&m, &from, to)
                }
                _ => {
                    let Some(delta) = delta else { return err("shift necesita --shift N (sectores)".into()) };
                    shift(&m, delta)
                }
            };
            match result.save(&out) {
                Ok(()) => {
                    for line in summary_lines(&result) {
                        println!("{line}");
                    }
                    println!("guardado en {}", out.display());
                    ExitCode::SUCCESS
                }
                Err(e) => err(format!("no se puede escribir {}: {e}", out.display())),
            }
        }
        "list" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            let mut wanted: Vec<MapStatus> = vec![MapStatus::Failed];
            let mut format = "list";
            let mut out: Option<PathBuf> = None;
            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--types" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        wanted.clear();
                        for c in v.chars() {
                            let Some(st) = parse_status_char(c) else {
                                return err(format!("estado desconocido '{c}' en --types"));
                            };
                            wanted.push(st);
                        }
                        i += 1;
                    }
                    "--format" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        if v != "list" && v != "bitmap-be" && v != "bitmap-le" {
                            return err(format!("formato desconocido '{v}' (list|bitmap-be|bitmap-le)"));
                        }
                        format = v;
                        i += 1;
                    }
                    "--out" => out = take_out(args, &mut i),
                    _ => return err(format!("opción no reconocida '{}'", args[i])),
                }
                i += 1;
            }
            let m = match MapFile::load(&PathBuf::from(path)) {
                Ok(m) => m,
                Err(e) => return err(e),
            };
            if format == "list" {
                let text = list_text(&m, &wanted);
                match &out {
                    Some(p) => {
                        if let Err(e) = std::fs::write(p, text.as_bytes()) {
                            return err(format!("no se puede escribir {}: {e}", p.display()));
                        }
                        println!("guardado en {}", p.display());
                    }
                    None => print!("{text}"),
                }
                ExitCode::SUCCESS
            } else {
                let big_endian = format == "bitmap-be";
                let bytes = bitmap_bytes(&m, &wanted, big_endian);
                let Some(out) = out else {
                    return err("los bitmaps requieren --out <archivo> (no se imprimen)".into());
                };
                match std::fs::write(&out, &bytes) {
                    Ok(()) => {
                        println!(
                            "bitmap ({}): {} bytes = {} sectores — guardado en {}",
                            format,
                            bytes.len(),
                            m.sector_count,
                            out.display()
                        );
                        ExitCode::SUCCESS
                    }
                    Err(e) => err(format!("no se puede escribir {}: {e}", out.display())),
                }
            }
        }
        "from-list" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            let mut format = "list";
            let mut domain = 0u64;
            let mut out: Option<PathBuf> = None;
            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--format" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        if v != "list" && v != "bitmap-be" && v != "bitmap-le" {
                            return err(format!("formato desconocido '{v}' (list|bitmap-be|bitmap-le)"));
                        }
                        format = v;
                        i += 1;
                    }
                    "--domain" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        match v.parse::<u64>() {
                            Ok(n) => domain = n,
                            Err(_) => return err(format!("--domain necesita un número de sectores, no '{v}'")),
                        }
                        i += 1;
                    }
                    "--out" => out = take_out(args, &mut i),
                    _ => return err(format!("opción no reconocida '{}'", args[i])),
                }
                i += 1;
            }
            let Some(out) = out else { return err("falta --out <mapfile destino>".into()) };
            let path = Path::new(path);
            let m = if format == "list" {
                let text = match std::fs::read_to_string(path) {
                    Ok(t) => t,
                    Err(e) => return err(format!("no se puede leer {}: {e}", path.display())),
                };
                match from_list(&text, domain) {
                    Ok(m) => m,
                    Err(e) => return err(e),
                }
            } else {
                let bytes = match std::fs::read(path) {
                    Ok(b) => b,
                    Err(e) => return err(format!("no se puede leer {}: {e}", path.display())),
                };
                match from_bitmap(&bytes, domain, format == "bitmap-be") {
                    Ok(m) => m,
                    Err(e) => return err(e),
                }
            };
            match m.save(&out) {
                Ok(()) => {
                    for line in summary_lines(&m) {
                        println!("{line}");
                    }
                    println!("guardado en {}", out.display());
                    ExitCode::SUCCESS
                }
                Err(e) => err(format!("no se puede escribir {}: {e}", out.display())),
            }
        }
        "annotate" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            let mut out: Option<PathBuf> = None;
            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--out" => out = take_out(args, &mut i),
                    _ => return err(format!("opción no reconocida '{}'", args[i])),
                }
                i += 1;
            }
            let Some(out) = out else {
                return err("annotate necesita --out <archivo> (nunca reescribe la entrada)".into());
            };
            let inp = Path::new(path);
            if same_path(inp, &out) {
                return err("annotate no escribe sobre su propia entrada: usa --out distinto".into());
            }
            let m = match MapFile::load(inp) {
                Ok(m) => m,
                Err(e) => return err(e),
            };
            let now = crate::log::format_asctime_utc(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            );
            match write_text_atomic(&out, &annotate_text(&m, &now)) {
                Ok(()) => {
                    let areas = damage_areas(&m);
                    println!(
                        "anotación escrita en {} — {} áreas de daño (pendientes {}, agotadas {})",
                        out.display(),
                        areas.len(),
                        areas.iter().filter(|a| a.2 == MapStatus::Failed).count(),
                        areas.iter().filter(|a| a.2 == MapStatus::Scraped).count(),
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => err(format!("no se puede escribir {}: {e}", out.display())),
            }
        }
        "dfxml" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            let mut out: Option<PathBuf> = None;
            let mut source: Option<String> = None;
            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--out" => out = take_out(args, &mut i),
                    "--source" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        source = Some(v.clone());
                        i += 1;
                    }
                    _ => return err(format!("opción no reconocida '{}'", args[i])),
                }
                i += 1;
            }
            let Some(out) = out else {
                return err("dfxml necesita --out <archivo.xml>".into());
            };
            let m = match MapFile::load(Path::new(path)) {
                Ok(m) => m,
                Err(e) => return err(e),
            };
            let label = source.unwrap_or_else(|| path.to_string());
            match write_text_atomic(&out, &dfxml_document(&m, &label)) {
                Ok(()) => {
                    println!(
                        "DFXML escrito en {} — {} sectores de evidencia ({} rangos del mapa + huecos)",
                        out.display(),
                        m.sector_count,
                        m.entries().len()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => err(format!("no se puede escribir {}: {e}", out.display())),
            }
        }
        "import" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            let mut domain = 0u64;
            let mut out: Option<PathBuf> = None;
            let mut i = 2usize;
            while i < args.len() {
                match args[i].as_str() {
                    "--domain" => {
                        let Some(v) = args.get(i + 1) else { return maps_usage() };
                        match v.parse::<u64>() {
                            Ok(n) => domain = n,
                            Err(_) => return err(format!("--domain necesita un número de sectores, no '{v}'")),
                        }
                        i += 1;
                    }
                    "--out" => out = take_out(args, &mut i),
                    _ => return err(format!("opción no reconocida '{}'", args[i])),
                }
                i += 1;
            }
            let Some(out) = out else { return err("falta --out <mapfile destino>".into()) };
            let path = Path::new(path);
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => return err(format!("no se puede leer {}: {e}", path.display())),
            };
            let domain = if domain > 0 {
                domain
            } else {
                match min_domain_from_text(&text) {
                    Some(d) => {
                        eprintln!(
                            "aviso: dominio derivado del propio mapfile ({d} sectores); \
                             confirme con --domain si el disco es más grande"
                        );
                        d
                    }
                    None => return err("no se puede derivar el dominio del texto: use --domain N".into()),
                }
            };
            match MapFile::import_text(path, domain) {
                Ok(m) => match m.save(&out) {
                    Ok(()) => {
                        for line in summary_lines(&m) {
                            println!("{line}");
                        }
                        println!("guardado en {}", out.display());
                        ExitCode::SUCCESS
                    }
                    Err(e) => err(format!("no se puede escribir {}: {e}", out.display())),
                },
                Err(e) => err(e),
            }
        }
        "export" => {
            let Some(path) = args.get(1) else { return maps_usage() };
            let Some(out) = args.get(2) else { return maps_usage() };
            let m = match MapFile::load(&PathBuf::from(path)) {
                Ok(m) => m,
                Err(e) => return err(e),
            };
            match m.export_text(&PathBuf::from(out)) {
                Ok(()) => {
                    println!("exportado {} (formato de texto ddrescue) a {}", path, out);
                    ExitCode::SUCCESS
                }
                Err(e) => err(format!("no se puede escribir {out}: {e}")),
            }
        }
        _ => maps_usage(),
    }
}

// ----------------------------------------------------------------- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("regrow_mapops_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn mapa(dom: u64, rangos: &[(u64, u64, MapStatus)]) -> MapFile {
        let mut m = MapFile::new(dom);
        for (s, l, st) in rangos {
            m.set(*s, *l, *st, 0);
        }
        m
    }

    fn entradas(m: &MapFile) -> Vec<(u64, u64, MapStatus)> {
        m.entries().iter().map(|e| (e.start, e.len, e.status)).collect()
    }

    #[test]
    fn coverage_materializa_huecos_como_pendientes() {
        let m = mapa(100, &[(10, 10, MapStatus::Good), (40, 5, MapStatus::Scraped)]);
        assert_eq!(
            coverage(&m),
            vec![
                (0, 10, MapStatus::Failed),
                (10, 10, MapStatus::Good),
                (20, 20, MapStatus::Failed),
                (40, 5, MapStatus::Scraped),
                (45, 55, MapStatus::Failed),
            ]
        );
    }

    #[test]
    fn merge_or_suma_lo_terminado_de_dos_pasadas() {
        // Pasada A: bueno [0,40); pasada B: bueno [30,60) sobre el mismo dominio.
        let a = mapa(100, &[(0, 40, MapStatus::Good), (40, 10, MapStatus::Scraped)]);
        let b = mapa(100, &[(0, 30, MapStatus::Scraped), (30, 30, MapStatus::Good)]);
        let or = merge(&[a.clone(), b], MergeMode::Or).unwrap();
        assert_eq!(entradas(&or), vec![(0, 60, MapStatus::Good), (60, 40, MapStatus::Failed)]);
        // Los que ambos mapas dan por agotados siguen marcados como agotados.
        let a2 = mapa(100, &[(0, 50, MapStatus::Good), (50, 10, MapStatus::Scraped)]);
        let b2 = mapa(100, &[(0, 45, MapStatus::Good), (45, 15, MapStatus::Scraped)]);
        let or2 = merge(&[a2, b2], MergeMode::Or).unwrap();
        assert_eq!(entradas(&or2), vec![(0, 50, MapStatus::Good), (50, 10, MapStatus::Scraped), (60, 40, MapStatus::Failed)]);
    }

    #[test]
    fn merge_and_y_xor_contrastan_mapas() {
        let a = mapa(100, &[(0, 40, MapStatus::Good)]);
        let b = mapa(100, &[(30, 30, MapStatus::Good)]);
        let and = merge(&[a.clone(), b.clone()], MergeMode::And).unwrap();
        assert_eq!(entradas(&and), vec![(0, 30, MapStatus::Failed), (30, 10, MapStatus::Good), (40, 60, MapStatus::Failed)]);
        let xor = merge(&[a, b], MergeMode::Xor).unwrap();
        assert_eq!(entradas(&xor), vec![(0, 30, MapStatus::Good), (30, 10, MapStatus::Failed), (40, 20, MapStatus::Good), (60, 40, MapStatus::Failed)]);
    }

    #[test]
    fn merge_tres_mapas_y_dominio_distinto_rechazado() {
        let a = mapa(100, &[(0, 10, MapStatus::Good)]);
        let b = mapa(100, &[(50, 10, MapStatus::Good)]);
        let c = mapa(100, &[(90, 10, MapStatus::Good)]);
        let or = merge(&[a.clone(), b, c], MergeMode::Or).unwrap();
        // Tres islas buenas, dos huecos explícitos entre ellas.
        assert_eq!(or.entries().len(), 5);
        let d = mapa(200, &[(0, 10, MapStatus::Good)]);
        let e = merge(&[a, d], MergeMode::Or).unwrap_err();
        assert!(e.contains("dominios distintos"), "{e}");
    }

    #[test]
    fn diff_identico_vs_discrepante() {
        let a = mapa(100, &[(0, 40, MapStatus::Good), (40, 10, MapStatus::Scraped)]);
        let b = mapa(100, &[(0, 40, MapStatus::Good), (40, 10, MapStatus::Scraped)]);
        assert!(diff(&a, &b, false).unwrap().is_empty());
        // `?` contra `-` discrepa en diff completo, pero no si solo importa lo terminado.
        let c = mapa(100, &[(0, 40, MapStatus::Good), (40, 10, MapStatus::Failed)]);
        let d = diff(&a, &c, false).unwrap();
        assert_eq!(d, vec![(40, 10, MapStatus::Scraped, MapStatus::Failed)]);
        assert!(diff(&a, &c, true).unwrap().is_empty());
        // Un hueco cuenta como `-` a efectos de diff (y `-` contra `?` sí discrepa
        // cuando comparamos todos los estados).
        let e1 = mapa(100, &[(0, 40, MapStatus::Good)]);
        let e2 = mapa(100, &[(0, 40, MapStatus::Good), (40, 60, MapStatus::Scraped)]);
        let d2 = diff(&e1, &e2, false).unwrap();
        assert_eq!(d2, vec![(40, 60, MapStatus::Failed, MapStatus::Scraped)]);
    }

    #[test]
    fn done_solo_si_todo_terminado() {
        let mut m = mapa(100, &[(0, 100, MapStatus::Good)]);
        assert!(is_done(&m));
        m.set(50, 1, MapStatus::Failed, 0);
        assert!(!is_done(&m));
        // Huecos sin mapear: no terminado.
        let parcial = mapa(100, &[(0, 40, MapStatus::Good)]);
        assert!(!is_done(&parcial));
    }

    #[test]
    fn invert_intercambia_terminado_y_pendiente() {
        let m = mapa(100, &[(0, 40, MapStatus::Good), (40, 10, MapStatus::Scraped)]);
        let inv = invert(&m);
        assert_eq!(entradas(&inv), vec![(0, 40, MapStatus::Failed), (40, 60, MapStatus::Good)]);
        // Doble inversión recupera el conjunto terminado original.
        let inv2 = invert(&inv);
        assert_eq!(
            inv2.entries().iter().filter(|e| e.status == MapStatus::Good).map(|e| (e.start, e.len)).collect::<Vec<_>>(),
            vec![(0, 40)]
        );
    }

    #[test]
    fn complete_rellena_huecos_dejando_los_rangos_intactos() {
        let mut m = mapa(100, &[(10, 10, MapStatus::Good), (40, 5, MapStatus::Scraped)]);
        m.set(10, 10, MapStatus::Good, 2); // reintentos de un rango explícito
        let c = complete(&m, MapStatus::Scraped);
        // Los huecos y los agotados vecinos se fusionan (mismo estado, mismos
        // reintentos); el rango bueno queda partido en el medio.
        assert_eq!(
            entradas(&c),
            vec![
                (0, 10, MapStatus::Scraped),
                (10, 10, MapStatus::Good),
                (20, 80, MapStatus::Scraped),
            ]
        );
        assert_eq!(c.retries_at(10), 2, "un rango explícito conserva sus reintentos");
        assert_eq!(c.entries().last().unwrap().start + c.entries().last().unwrap().len, 100);
    }

    #[test]
    fn retype_cambia_estados_y_preserva_los_demas() {
        let m = mapa(100, &[(0, 30, MapStatus::Good), (30, 20, MapStatus::Scraped), (50, 10, MapStatus::Scraped)]);
        // "lo agotado vuelve a pendiente" (preparar otra pasada entera):
        let r = retype(&m, &[MapStatus::Scraped], MapStatus::Failed);
        assert_eq!(entradas(&r), vec![(0, 30, MapStatus::Good), (30, 70, MapStatus::Failed)]);
        // Huecos: cambiar `-` (huecos cuentan como `-`) a agotado.
        let con_huecos = mapa(100, &[(20, 10, MapStatus::Good)]);
        let r2 = retype(&con_huecos, &[MapStatus::Failed], MapStatus::Scraped);
        assert_eq!(
            entradas(&r2),
            vec![(0, 20, MapStatus::Scraped), (20, 10, MapStatus::Good), (30, 70, MapStatus::Scraped)]
        );
    }

    #[test]
    fn shift_desplaza_y_recorta_sin_cambiar_dominio() {
        let m = mapa(100, &[(0, 40, MapStatus::Good), (60, 30, MapStatus::Scraped)]);
        let s = shift(&m, 10);
        assert_eq!(s.sector_count, 100);
        assert_eq!(entradas(&s), vec![(10, 40, MapStatus::Good), (70, 30, MapStatus::Scraped)]);
        // Hacia atrás: lo que sale del dominio se recorta.
        let s2 = shift(&m, -10);
        assert_eq!(entradas(&s2), vec![(0, 30, MapStatus::Good), (50, 30, MapStatus::Scraped)]);
    }

    #[test]
    fn lista_badblocks_roundtrip() {
        let m = mapa(100, &[(10, 5, MapStatus::Good), (20, 3, MapStatus::Scraped), (40, 1, MapStatus::Good)]);
        // Tramos contiguos pedidos (pendiente+agotado+pendiente) salen como un solo rango.
        let l = list_text(&m, &[MapStatus::Failed, MapStatus::Scraped]);
        assert_eq!(l, "0-9\n15-39\n41-99\n");
        let recon = from_list(&l, 100).unwrap();
        // Lo listado queda pendiente y lo demás terminado.
        assert_eq!(
            entradas(&recon),
            vec![
                (0, 10, MapStatus::Failed),
                (10, 5, MapStatus::Good),
                (15, 25, MapStatus::Failed),
                (40, 1, MapStatus::Good),
                (41, 59, MapStatus::Failed),
            ]
        );
        // Malformaciones se rechazan, no se ignoran a medias.
        assert!(from_list("10-\n", 100).is_err());
        assert!(from_list("120-130", 100).is_err());
        assert!(from_list("", 100).is_err());
        assert!(from_list("5-9", 0).is_err());
    }

    #[test]
    fn bitmap_roundtrip_be_y_le() {
        let m = mapa(20, &[(0, 3, MapStatus::Good), (3, 2, MapStatus::Scraped), (10, 1, MapStatus::Good)]);
        // Pendientes: sectores {2? no: 0,1,2 son buenos} → pendientes 3,4,5..9 y 11..19.
        let be = bitmap_bytes(&m, &[MapStatus::Failed, MapStatus::Scraped], true);
        assert_eq!(be.len(), 3); // ceil(20/8)
        let recon = from_bitmap(&be, 20, true).unwrap();
        assert_eq!(
            entradas(&recon),
            vec![(0, 3, MapStatus::Good), (3, 7, MapStatus::Failed), (10, 1, MapStatus::Good), (11, 9, MapStatus::Failed)]
        );
        let le = bitmap_bytes(&m, &[MapStatus::Failed, MapStatus::Scraped], false);
        let recon_le = from_bitmap(&le, 20, false).unwrap();
        assert_eq!(entradas(&recon_le), entradas(&recon));
        assert_ne!(le, be, "el orden de bits debe distinguir BE de LE");
        // Bitmap más corto que el dominio declarado: rechazado.
        assert!(from_bitmap(&be, 25, true).is_err());
        // Sin dominio: se asumen 8 sectores por byte.
        let auto = from_bitmap(&be, 0, true).unwrap();
        assert_eq!(auto.sector_count, 24);
    }

    #[test]
    fn summary_y_guardado_del_resultado() {
        let dir = tmpdir("ops");
        let a = mapa(1000, &[(0, 400, MapStatus::Good)]);
        let b = mapa(1000, &[(300, 100, MapStatus::Good), (600, 50, MapStatus::Scraped)]);
        let or = merge(&[a, b], MergeMode::Or).unwrap();
        let p = dir.join("merged.map");
        or.save(&p).unwrap();
        let loaded = MapFile::load(&p).unwrap();
        assert_eq!(loaded.entries(), or.entries());
        let lines = summary_lines(&loaded);
        assert!(lines[1].contains("terminados"), "{lines:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn areas_se_agrupan_por_estado_no_terminado() {
        // D-A1: un `?` contiguo a un `-` forma DOS áreas; lo mismo contiguo se fusiona.
        let m = mapa(
            100,
            &[
                (10, 5, MapStatus::Good),
                (15, 10, MapStatus::Scraped),
                (25, 5, MapStatus::Failed),
                (30, 10, MapStatus::Failed), // mismo estado contiguo ⇒ sigue la misma área
                (40, 1, MapStatus::Good),
            ],
        );
        assert_eq!(
            damage_areas(&m),
            vec![
                (0, 10, MapStatus::Failed),  // hueco inicial
                (15, 10, MapStatus::Scraped),
                (25, 15, MapStatus::Failed),
                (41, 59, MapStatus::Failed),
            ]
        );
    }

    #[test]
    fn areas_huecos_y_mapas_extremos() {
        // Huecos sin mapear valen por `-`.
        let m = mapa(50, &[(10, 5, MapStatus::Good), (20, 3, MapStatus::Scraped)]);
        assert_eq!(
            damage_areas(&m),
            vec![
                (0, 10, MapStatus::Failed),
                (15, 5, MapStatus::Failed),
                (20, 3, MapStatus::Scraped),
                (23, 27, MapStatus::Failed),
            ]
        );
        // Mapa sin rangos: todo el dominio es una sola área pendiente.
        assert_eq!(damage_areas(&mapa(30, &[])), vec![(0, 30, MapStatus::Failed)]);
        // Mapa completo: cero áreas.
        assert!(damage_areas(&mapa(100, &[(0, 100, MapStatus::Good)])).is_empty());
    }

    #[test]
    fn status_incluye_areas_de_dano() {
        let m = mapa(100, &[(10, 5, MapStatus::Good), (15, 10, MapStatus::Scraped)]);
        let lines = summary_lines(&m);
        let areas = lines.iter().find(|l| l.contains("áreas de daño"));
        assert!(areas.is_some(), "{lines:?}");
        // 3 áreas: hueco inicial (-), agotado (?), hueco final (-).
        assert!(areas.unwrap().contains("3"), "{areas:?}");
    }

    #[test]
    fn annotate_golden_completo() {
        let mut m = MapFile::new(50);
        m.set(10, 5, MapStatus::Good, 0);
        m.set(20, 3, MapStatus::Scraped, 2);
        m.set(30, 4, MapStatus::Failed, 1);
        let text = annotate_text(&m, "2026-11-24 12:00:00 UTC");
        let expected = "\
# Mapfile anotado por regrow — 2026-11-24 12:00:00 UTC (copia de lectura; el maestro no se toca)
# Dominio: 50 sectores = 0.00 GiB · 3 rangos
#   + terminados: 5 (10.00 %) · - pendientes: 42 · ? agotados: 3 (6.00 %)
#   áreas de daño: 4 (pendientes 3 · agotadas 1)
#      pos        size  status
0x00000000  0x00001400  -   # sin mapear · 5.0 KiB (20.00 %) · área 1
0x00001400  0x00000a00  +   # terminado · 2.5 KiB (10.00 %)
0x00001e00  0x00000a00  -   # sin mapear · 2.5 KiB (10.00 %) · área 2
0x00002800  0x00000600  ?   # agotado · 1.5 KiB (6.00 %) · área 3 · reintentos 2
0x00002e00  0x00000e00  -   # sin mapear · 3.5 KiB (14.00 %) · área 4
0x00003c00  0x00000800  -   # pendiente · 2.0 KiB (8.00 %) · área 4 · reintentos 1
0x00004400  0x00002000  -   # sin mapear · 8.0 KiB (32.00 %) · área 4
";
        assert_eq!(text, expected);
    }

    #[test]
    fn annotate_mapa_completo_y_reparsing_del_anotado() {
        let done = mapa(100, &[(0, 100, MapStatus::Good)]);
        let text = annotate_text(&done, "2026-11-24 12:00:00 UTC");
        assert!(text.contains("rescate terminado"), "{text}");
        assert!(text.contains("áreas de daño: 0"), "{text}");

        // El cuerpo anotado sigue siendo un mapfile de texto válido: los
        // importadores ignoran los comentarios y recuperan el mismo conjunto.
        let dir = tmpdir("annot_rep");
        let mut m = MapFile::new(50);
        m.set(10, 5, MapStatus::Good, 0);
        m.set(20, 3, MapStatus::Scraped, 2);
        let p = dir.join("anotado.txt");
        fs::write(&p, annotate_text(&m, "2026-11-24 12:00:00 UTC").as_bytes()).unwrap();
        let back = MapFile::import_text(&p, 50).unwrap();
        assert_eq!(back.stats().0, m.stats().0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn annotate_cli_escribe_atachado_y_rechaza_in_situ() {
        let dir = tmpdir("annot_cli");
        let mut m = MapFile::new(50);
        m.set(10, 5, MapStatus::Good, 0);
        m.set(20, 3, MapStatus::Scraped, 2);
        let inp = dir.join("maestro.map");
        m.save(&inp).unwrap();

        // Rechazo total sin `--out` y rechazo escribir sobre la propia entrada.
        assert_ne!(
            cli_maps(&["annotate".into(), inp.display().to_string()]),
            ExitCode::SUCCESS
        );
        assert_ne!(
            cli_maps(&[
                "annotate".into(),
                inp.display().to_string(),
                "--out".into(),
                inp.display().to_string()
            ]),
            ExitCode::SUCCESS,
            "annotate jamás reescribe la entrada (RF-3)"
        );
        assert!(MapFile::load(&inp).is_ok(), "el maestro debe quedar intacto");

        // Salida normal: archivo atómico sin .tmp residual y re-importable.
        let outp = dir.join("maestro.anotado.txt");
        let code = cli_maps(&[
            "annotate".into(),
            inp.display().to_string(),
            "--out".into(),
            outp.display().to_string(),
        ]);
        assert_eq!(code, ExitCode::SUCCESS);
        let residuos: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|f| f.ends_with(".tmp"))
            .collect();
        assert!(residuos.is_empty(), "quedó tmp: {residuos:?}");
        let text = fs::read_to_string(&outp).unwrap();
        assert!(text.contains("áreas de daño"), "{text}");
        let back = MapFile::import_text(&outp, 50).unwrap();
        assert_eq!(back.stats().0, m.stats().0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn importar_y_exportar_mapas_de_texto() {        let dir = tmpdir("text");
        // Mapfile de texto al estilo ddrescue: posiciones en bytes, leyenda 1.23+.
        let txt = dir.join("mapa.map.txt");
        fs::write(
            &txt,
            "# Mapfile. Created by ddrescue\n\
             #      pos        size  status\n\
             0x00000000  0x00001c00  +\n\
             0x00001c00  0x00000800  /\n\
             0x00002400  0x00001000  -\n",
        )
        .unwrap();
        // 0x1c00 = 7 KiB = 14 sectores; dominio derivado = (0x3400)/512 = 26.
        assert_eq!(min_domain_from_text(&fs::read_to_string(&txt).unwrap()), Some(26));
        let m = MapFile::import_text(&txt, 26).unwrap();
        assert_eq!(
            entradas(&m),
            vec![
                (0, 14, MapStatus::Good),
                (14, 4, MapStatus::Scraped),
                (18, 8, MapStatus::Failed),
            ]
        );
        // Exportación legible por ddrescue: bytes hex y leyenda propia de ddrescue.
        let out = dir.join("redo.map.txt");
        m.export_text(&out).unwrap();
        let s = fs::read_to_string(&out).unwrap();
        assert!(s.contains("0x00001c00  0x00000800  /"), "{s}");
        // Roundtrip binario↔texto conserva el conjunto terminado.
        let bin = dir.join("m.map");
        m.save(&bin).unwrap();
        let back = MapFile::load(&bin).unwrap();
        assert_eq!(back.stats().0, m.stats().0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dfxml_golden() {
        let mut m = MapFile::new(100);
        m.set(0, 10, MapStatus::Good, 0);
        m.set(20, 10, MapStatus::Scraped, 3);
        m.set(40, 10, MapStatus::Failed, 0);
        let doc = dfxml_document(&m, "disco.img");
        let expected_runs = [
            "      <byte_run offset=\"0\" length=\"5120\" status=\"finished\"/>",
            "      <byte_run offset=\"5120\" length=\"5120\" status=\"pending\"/>",
            "      <byte_run offset=\"10240\" length=\"5120\" status=\"exhausted\"/>",
            "      <byte_run offset=\"15360\" length=\"5120\" status=\"pending\"/>",
            "      <byte_run offset=\"20480\" length=\"5120\" status=\"pending\"/>",
            "      <byte_run offset=\"25600\" length=\"25600\" status=\"pending\"/>",
        ];
        for line in expected_runs {
            assert!(doc.contains(line), "falta «{line}» en:\n{doc}");
        }
        assert_eq!(doc.matches("<byte_run ").count(), 6);
        assert!(doc.contains("<filesize>51200</filesize>"));
        assert!(doc.contains("<sector_size>512</sector_size>"));
        assert!(doc.contains("<filename>disco.img</filename>"));
        assert!(doc.contains("<mapfile_crc32>"));
        // determinista: dos llamadas, mismo documento
        assert_eq!(doc, dfxml_document(&m, "disco.img"));
        // escapeo de etiquetas hostiles
        let raro = dfxml_document(&m, "a&b<img>");
        assert!(raro.contains("<filename>a&amp;b&lt;img&gt;</filename>"));
    }
}
