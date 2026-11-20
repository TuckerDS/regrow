// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Mapa de superficie por sectores (M6): convierte un registro de pasada
//! (`DISKn.LOG`) en un mapa visual en HTML, abierto en cualquier navegador.
//!
//! Cada celda cubre un tramo igual del disco y se pinta con el PEOR caso
//! observado en él: rojo ilegible, naranja retardo severo (T4/`X`), amarillo
//! T3, crema T2, azul T1, verde recuperado, gris sano. Es la misma idea que
//! el mapa de superficie de las herramientas de taller, construida solo con
//! los datos que el escaneo ya dejó escritos: cero trabajo extra en la pasada.

use std::path::Path;

use crate::log::{DiskLog, Flag};
use crate::mapfile::{MapFile, MapStatus};

pub const SURFMAP_DEFAULT_CELLS: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Severity {
    Bad,   // S / B
    T4,    // X
    T3,    // V
    T2,    // U
    T1,    // T
    Rec,   // R
    Clean,
}

impl Severity {
    fn rank(self) -> u8 {
        match self {
            Severity::Bad => 6,
            Severity::T4 => 5,
            Severity::T3 => 4,
            Severity::T2 => 3,
            Severity::T1 => 2,
            Severity::Rec => 1,
            Severity::Clean => 0,
        }
    }
    fn css(self) -> &'static str {
        match self {
            Severity::Bad => "#c0392b",
            Severity::T4 => "#e67e22",
            Severity::T3 => "#f1c40f",
            Severity::T2 => "#f7dc6f",
            Severity::T1 => "#5dade2",
            Severity::Rec => "#2ecc71",
            Severity::Clean => "#22303c",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Severity::Bad => "ilegible",
            Severity::T4 => "retardo severo (T4)",
            Severity::T3 => "lento fuerte (T3)",
            Severity::T2 => "lento (T2)",
            Severity::T1 => "muy levemente lento (T1)",
            Severity::Rec => "recuperado",
            Severity::Clean => "sano / sin datos",
        }
    }
}

fn severity_of(flag: Flag) -> Severity {
    match flag {
        Flag::S | Flag::B => Severity::Bad,
        Flag::X | Flag::D => Severity::T4,
        Flag::V => Severity::T3,
        Flag::U => Severity::T2,
        Flag::T => Severity::T1,
        Flag::R => Severity::Rec,
        _ => Severity::Clean,
    }
}

/// Renderiza el mapa desde los bytes de un DISKn.LOG. `total_sectors` es el
/// dominio escaneado (el `end` del rango de la pasada).
pub fn render_surfmap(log_bytes: &[u8], total_sectors: u64, cells: usize) -> Result<String, String> {
    let cells = cells.clamp(16, 8192);
    let log = DiskLog::parse(log_bytes).map_err(|e| format!("no se puede leer el registro: {e:?}"))?;
    let span = (total_sectors + cells as u64 - 1) / cells as u64;
    let mut worst = vec![Severity::Clean; cells];
    let mut counts = vec![0u64; 7]; // por flag
    for r in &log.records {
        let sev = severity_of(r.flag);
        if sev == Severity::Clean {
            continue;
        }
        counts[sev.rank() as usize] += 1;
        let a = (r.sector as u64 / span) as usize;
        let b = ((r.sector as u64 + r.run as u64 - 1) / span) as usize;
        for c in a.min(cells - 1)..=b.min(cells - 1) {
            if sev.rank() > worst[c].rank() {
                worst[c] = sev;
            }
        }
    }
    let total_records = log.records.len();
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html lang=\"es\"><head><meta charset=\"utf-8\">\n");
    html.push_str("<title>regrow — mapa de superficie</title>\n<style>\n");
    html.push_str("body{font-family:sans-serif;background:#101820;color:#dfe6ee;margin:18px}\n");
    html.push_str(".map{display:flex;flex-wrap:wrap;gap:1px;max-width:1000px}\n");
    html.push_str(".c{width:9px;height:9px}\n");
    html.push_str("table{border-collapse:collapse;margin-top:12px}td{padding:3px 10px;border:1px solid #2c3e50}\n");
    html.push_str("</style></head><body>\n");
    html.push_str(&format!(
        "<h2>Mapa de superficie — {} sectores en {} celdas ({} sectores/celda)</h2>\n",
        total_sectors, cells, span
    ));
    html.push_str("<div class=\"map\">");
    for sev in &worst {
        html.push_str(&format!("<div class=\"c\" style=\"background:{}\"></div>", sev.css()));
    }
    html.push_str("</div>\n<table><tr><th>categoría</th><th>registros</th><th>columna</th></tr>\n");
    for sev in [
        Severity::Bad,
        Severity::T4,
        Severity::T3,
        Severity::T2,
        Severity::T1,
        Severity::Rec,
    ] {
        html.push_str(&format!(
            "<tr><td><span class=\"c\" style=\"background:{};display:inline-block\"></span> {}</td><td>{}</td><td>{}</td></tr>\n",
            sev.css(),
            sev.label(),
            counts[sev.rank() as usize],
            sev.rank()
        ));
    }
    html.push_str(&format!(
        "<tr><td>total registros en el log</td><td>{total_records}</td><td>—</td></tr>\n"
    ));
    html.push_str("</table>\n<p>regrow · mapa derivado del registro de la pasada; gris = sano o sin datos en la celda.</p>\n");
    html.push_str("</body></html>\n");
    Ok(html)
}

/// Escribe el mapa junto a su DISKn.LOG (o en `out` si se dio ruta).
pub fn write_surfmap(
    log_dir: &Path,
    unit: u32,
    total_sectors: u64,
    cells: usize,
    out: Option<&Path>,
) -> Result<std::path::PathBuf, String> {
    let log_path = log_dir.join(format!("DISK{unit}.LOG"));
    let bytes = std::fs::read(&log_path)
        .map_err(|e| format!("no se puede leer {}: {e}", log_path.display()))?;
    let html = render_surfmap(&bytes, total_sectors, cells)?;
    let out_path = match out {
        Some(p) => p.to_path_buf(),
        None => log_dir.join(format!("DISK{unit}.surfmap.html")),
    };
    std::fs::write(&out_path, html.as_bytes())
        .map_err(|e| format!("no se puede escribir {}: {e}", out_path.display()))?;
    Ok(out_path)
}

// -------------------------------------------------- mapa desde un mapfile ----

/// Estado de una celda cuando el mapa viene de un mapfile (propio o importado
/// de ddrescue). Como en el mapa de pasada, cada celda se pinta con el PEOR
/// caso que contiene: malo > rascado > rescatado > sin intentar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MapCell {
    Failed,
    Scraped,
    Good,
    Untried,
}

impl MapCell {
    fn rank(self) -> u8 {
        match self {
            MapCell::Failed => 3,
            MapCell::Scraped => 2,
            MapCell::Good => 1,
            MapCell::Untried => 0,
        }
    }
    fn css(self) -> &'static str {
        match self {
            MapCell::Failed => "#c0392b",
            MapCell::Scraped => "#e67e22",
            MapCell::Good => "#2ecc71",
            MapCell::Untried => "#22303c",
        }
    }
    fn label(self) -> &'static str {
        match self {
            MapCell::Failed => "malo / pendiente de reintentar",
            MapCell::Scraped => "rascado y sigue malo",
            MapCell::Good => "rescatado",
            MapCell::Untried => "sin intentar",
        }
    }
    fn of_status(s: MapStatus) -> MapCell {
        match s {
            MapStatus::Good => MapCell::Good,
            MapStatus::Failed => MapCell::Failed,
            MapStatus::Scraped => MapCell::Scraped,
        }
    }
}

fn fmt_gb(sectors: u64) -> String {
    format!("{:.2} GB", sectors as f64 * 512.0 / 1e9)
}

/// Renderiza el mapa desde un `MapFile` (dominio = su `sector_count`). Con un
/// mapfile importado de ddrescue es el estado completo del rescate: lo verde
/// ya está a salvo y ningún rescate lo reescribe; rojo y naranja son el
/// dominio de trabajo pendiente.
pub fn render_mapfile(map: &MapFile, cells: usize) -> Result<String, String> {
    let cells = cells.clamp(16, 8192);
    let total = map.sector_count;
    if total == 0 {
        return Err("el mapfile no tiene dominio".into());
    }
    let span = (total + cells as u64 - 1) / cells as u64;
    let mut worst = vec![MapCell::Untried; cells];
    let mut sectors_by = [0u64; 4];
    let mut blocks_by = [0u64; 4];
    let mut problems: Vec<(u64, u64, MapCell)> = Vec::new();
    for e in map.entries() {
        let cell = MapCell::of_status(e.status);
        sectors_by[cell.rank() as usize] += e.len;
        blocks_by[cell.rank() as usize] += 1;
        if cell != MapCell::Good {
            problems.push((e.start, e.len, cell));
        }
        let a = (e.start / span) as usize;
        let b = ((e.start + e.len - 1) / span) as usize;
        for c in a.min(cells - 1)..=b.min(cells - 1) {
            if cell.rank() > worst[c].rank() {
                worst[c] = cell;
            }
        }
    }
    // El hueco entre entradas es "sin intentar" (mismo significado que en el
    // propio motor de rescate).
    let known = sectors_by[0] + sectors_by[1] + sectors_by[2] + sectors_by[3];
    let untried = total.saturating_sub(known);
    problems.sort_by_key(|p| std::cmp::Reverse(p.1));

    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html lang=\"es\"><head><meta charset=\"utf-8\">\n");
    html.push_str("<title>regrow — estado del rescate (mapfile)</title>\n<style>\n");
    html.push_str("body{font-family:sans-serif;background:#101820;color:#dfe6ee;margin:18px}\n");
    html.push_str(".map{display:flex;flex-wrap:wrap;gap:1px;max-width:1000px}\n");
    html.push_str(".c{width:9px;height:9px}\n");
    html.push_str("table{border-collapse:collapse;margin-top:12px}td{padding:3px 10px;border:1px solid #2c3e50}\n");
    html.push_str("</style></head><body>\n");
    html.push_str(&format!(
        "<h2>Estado del rescate — {} sectores ({}) en {} celdas ({} sectores/celda)</h2>\n",
        total,
        fmt_gb(total),
        cells,
        span
    ));
    html.push_str("<div class=\"map\">");
    for (i, sev) in worst.iter().enumerate() {
        let a = i as u64 * span;
        let b = (a + span).min(total);
        html.push_str(&format!(
            "<div class=\"c\" style=\"background:{}\" title=\"sectores {}–{} ({}–{}): {}\"></div>",
            sev.css(),
            a,
            b,
            fmt_gb(a),
            fmt_gb(b),
            sev.label()
        ));
    }
    html.push_str("</div>\n<table><tr><th>estado</th><th>bloques</th><th>sectores</th><th>%</th></tr>\n");
    for sev in [MapCell::Failed, MapCell::Scraped, MapCell::Good, MapCell::Untried] {
        let sec = if sev == MapCell::Untried { untried } else { sectors_by[sev.rank() as usize] };
        html.push_str(&format!(
            "<tr><td><span class=\"c\" style=\"background:{};display:inline-block\"></span> {}</td><td>{}</td><td>{}</td><td>{:.2}%</td></tr>\n",
            sev.css(),
            sev.label(),
            blocks_by[sev.rank() as usize],
            sec,
            sec as f64 * 100.0 / total as f64
        ));
    }
    html.push_str("</table>\n");
    if !problems.is_empty() {
        html.push_str("<h3>Mayor dominio pendiente (tramos más grandes)</h3>\n");
        html.push_str("<table><tr><th>sectores desde–hasta</th><th>tamaño</th><th>estado</th></tr>\n");
        for (a, len, cell) in problems.iter().take(15) {
            html.push_str(&format!(
                "<tr><td>{}–{}</td><td>{} MB</td><td>{}</td></tr>\n",
                a,
                a + len,
                len * 512 / 1_048_576,
                cell.label()
            ));
        }
        html.push_str("</table>\n");
    }
    html.push_str("<p>regrow · verde = ya rescatado (nunca se reescribe); rojo = malo confirmado; naranja = rascado y sigue malo; gris = sin intentar. Pasa el cursor sobre las celdas para ver el rango.</p>\n");
    html.push_str("</body></html>\n");
    Ok(html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapa_pinta_la_peor_severidad_de_cada_celda() {
        // Cuerpo gramatical simple; parseo tolerante de regrow.
        let body = "0=S1\r\n100=X1\r\n200=T1\r\n300=V2\r\n400=R1\r\n500=T1\r\n";
        let html = render_surfmap(body.as_bytes(), 1000, 20).unwrap(); // span 50
        // Celda 0 (sectores 0..50) con S ⇒ rojo; celda 2 (100..150) con X ⇒ naranja.
        // El patrón excluye el CSS del <body>, que también lleva "background:".
        let cells: Vec<&str> = html
            .match_indices("style=\"background:")
            .map(|(i, _)| &html[i..i + 40])
            .collect();
        assert!(cells.len() >= 20);
        assert!(cells[0].contains("#c0392b"), "celda 0 roja");
        assert!(cells[2].contains("#e67e22"), "celda 2 naranja (X)");
        assert!(cells[4].contains("#5dade2"), "celda 4 azul (T en 200)");
        assert!(cells[6].contains("#f1c40f"), "celda 6 amarilla (V en 300)");
        assert!(cells[8].contains("#2ecc71"), "celda 8 verde (R en 400)");
        assert!(html.contains("<td>1</td>"), "un ilegible registrado");
    }

    #[test]
    fn corrida_larga_mancha_varias_celdas() {
        // Corrida X de 400 sectores desde 0: debe tocar las 8 primeras celdas.
        let body = "0=X400\r\n";
        let html = render_surfmap(body.as_bytes(), 1000, 20).unwrap();
        let cells: Vec<&str> = html
            .match_indices("style=\"background:")
            .map(|(i, _)| &html[i..i + 40])
            .collect();
        for c in &cells[0..8] {
            assert!(c.contains("#e67e22"), "celda en la corrida debe ser naranja");
        }
        assert!(!cells[9].contains("#e67e22"));
    }

    #[test]
    fn mapa_de_mapfile_pinta_peor_caso_por_celda() {
        let mut m = MapFile::new(1000);
        m.set(0, 500, MapStatus::Good, 0);
        m.set(500, 100, MapStatus::Failed, 2);
        m.set(600, 100, MapStatus::Scraped, 3);
        // 700..1000 sin entradas: quedan como "sin intentar".
        // (16 es el mínimo de celdas: span = ceil(1000/16) = 63 sectores.)
        let html = render_mapfile(&m, 16).unwrap();
        let cells: Vec<&str> = html
            .match_indices("style=\"background:")
            .map(|(i, _)| &html[i..i + 40])
            .collect();
        assert!(cells[0].contains("#2ecc71"), "celda 0 verde (Good)");
        assert!(cells[6].contains("#2ecc71"), "celda 6 verde (Good)");
        assert!(cells[7].contains("#c0392b"), "celda 7 roja (Failed en 500)");
        assert!(cells[10].contains("#e67e22"), "celda 10 naranja (Scraped)");
        assert!(cells[15].contains("#22303c"), "celda 15 gris (sin intentar)");
        // La tabla de estado debe reflejar el dominio completo.
        assert!(html.contains("<td>300</td>"), "300 sectores sin intentar");
        assert!(html.contains("<td>500</td>"), "500 sectores rescatados");
        // Y el rango grande pendiente debe aparecer en la tabla de dominios.
        assert!(html.contains("500–600"), "tabla de tramos pendientes");
    }

    #[test]
    fn celda_mixta_se_pinta_con_su_peor_caso() {
        // Una celda que contiene Good y Failed se pinta roja (peor caso).
        // 16 celdas, span 63: Good 0..89 toca celdas 0–1; Failed 90..99, celda 1.
        let mut m = MapFile::new(1000);
        m.set(0, 90, MapStatus::Good, 0);
        m.set(90, 10, MapStatus::Failed, 1);
        let html = render_mapfile(&m, 16).unwrap();
        let cells: Vec<&str> = html
            .match_indices("style=\"background:")
            .map(|(i, _)| &html[i..i + 40])
            .collect();
        assert!(cells[0].contains("#2ecc71"), "celda 0 verde (solo Good)");
        assert!(cells[1].contains("#c0392b"), "celda 1 mixta: se pinta con lo peor");
    }
}
