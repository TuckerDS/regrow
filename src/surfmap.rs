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
}
