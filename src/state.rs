// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Checkpoint de pasada para reanudación (`--resume`).
//!
//! El motor reporta progreso periódicamente; `main` lo serializa en
//! `diskstats/DISK<n>.state` como texto plano `clave=valor` (CRLF). Al
//! completar con éxito el pase, el archivo se borra. Un pase abortado o
//! interrumpido deja el estado para continuar con `--resume`.

use std::fs;
use std::io;
use std::path::Path;

use crate::engine::{EngineStats, RunMode};

/// Estado serializable de una pasada en curso (o interrumpida).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub mode: RunMode,
    /// Rango original de la pasada (exclusivo).
    pub start: u32,
    pub end: u32,
    /// Último sector COMPLETADO; `--resume` reanuda en `last_sector + 1`.
    pub last_sector: u32,
    /// Contadores acumulados de las pasadas ya realizadas.
    pub stats: EngineStats,
    /// Identificador de unidad (para validar que el checkpoint corresponde).
    pub unit: u32,
}

pub fn state_path(log_dir: &Path, unit: u32) -> std::path::PathBuf {
    log_dir.join(format!("DISK{unit}.state"))
}

fn mode_word(m: RunMode) -> &'static str {
    match m {
        RunMode::Scan => "scan",
        RunMode::Repair => "repair",
        RunMode::Refresh => "refresh",
        RunMode::Regenerate => "regenerate",
        RunMode::TestWrite => "testwrite",
    }
}

fn parse_mode(s: &str) -> Option<RunMode> {
    match s {
        "scan" => Some(RunMode::Scan),
        "repair" => Some(RunMode::Repair),
        "refresh" => Some(RunMode::Refresh),
        "regenerate" => Some(RunMode::Regenerate),
        "testwrite" => Some(RunMode::TestWrite),
        _ => None,
    }
}

impl Checkpoint {
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let s = self.stats;
        let body = format!(
            "regrow.state.v1\r\n\
             mode={}\r\nunit={}\r\nstart={}\r\nend={}\r\nlast_sector={}\r\n\
             scanned={}\r\nbad={}\r\nrecovered={}\r\ndelays={}\r\n\
             regenerated_ok={}\r\ndata_lost={}\r\nbacked_up={}\r\nhealthy_skipped={}\r\n\
             slow1={}\r\nslow2={}\r\nslow3={}\r\n\
             tested_ok={}\r\nmarginal={}\r\nwrite_fail={}\r\n",
            mode_word(self.mode),
            self.unit,
            self.start,
            self.end,
            self.last_sector,
            s.scanned,
            s.bad,
            s.recovered,
            s.delays,
            s.regenerated_ok,
            s.data_lost,
            s.backed_up,
            s.healthy_skipped,
            s.slow1,
            s.slow2,
            s.slow3,
            s.tested_ok,
            s.marginal,
            s.write_fail,
        );
        // Escritura atómica: tempfile + rename (un corte no corrompe el estado).
        let tmp = path.with_extension("state.tmp");
        fs::write(&tmp, body.as_bytes())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> io::Result<Checkpoint> {
        let text = fs::read_to_string(path)?;
        let mut kv: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        let mut mode: Option<RunMode> = None;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line == "regrow.state.v1" || line.is_empty() {
                continue;
            }
            let (k, v) = match line.split_once('=') {
                Some(p) => p,
                None => return Err(io::Error::other("checkpoint mal formado")),
            };
            match k {
                "mode" => mode = Some(parse_mode(v).ok_or_else(|| io::Error::other("modo inválido"))?),
                _ => {
                    let n = v
                        .parse::<u64>()
                        .map_err(|_| io::Error::other("checkpoint mal formado"))?;
                    kv.insert(k.to_string(), n);
                }
            }
        }
        let get = |k: &str| -> io::Result<u64> {
            kv.get(k).copied().ok_or_else(|| io::Error::other(format!("campo ausente: {k}")))
        };
        let mode = mode.ok_or_else(|| io::Error::other("campo ausente: mode"))?;
        Ok(Checkpoint {
            mode,
            unit: get("unit")? as u32,
            start: get("start")? as u32,
            end: get("end")? as u32,
            last_sector: get("last_sector")? as u32,
            stats: EngineStats {
                scanned: get("scanned")?,
                bad: get("bad")?,
                recovered: get("recovered")?,
                delays: get("delays")?,
                regenerated_ok: get("regenerated_ok")?,
                data_lost: get("data_lost")?,
                backed_up: get("backed_up")?,
                // Tolerante a campos ausentes (contadores opcionales).
                healthy_skipped: kv.get("healthy_skipped").copied().unwrap_or(0),
                slow1: kv.get("slow1").copied().unwrap_or(0),
                slow2: kv.get("slow2").copied().unwrap_or(0),
                slow3: kv.get("slow3").copied().unwrap_or(0),
                tested_ok: kv.get("tested_ok").copied().unwrap_or(0),
                marginal: kv.get("marginal").copied().unwrap_or(0),
                write_fail: kv.get("write_fail").copied().unwrap_or(0),
            },
        })
    }

    pub fn remove(path: &Path) {
        let _ = fs::remove_file(path);
    }

    /// Suma de contadores (checkpoint previo + pasada actual).
    pub fn merge_stats(prev: EngineStats, cur: EngineStats) -> EngineStats {
        EngineStats {
            scanned: prev.scanned + cur.scanned,
            bad: prev.bad + cur.bad,
            recovered: prev.recovered + cur.recovered,
            delays: prev.delays + cur.delays,
            regenerated_ok: prev.regenerated_ok + cur.regenerated_ok,
            data_lost: prev.data_lost + cur.data_lost,
            backed_up: prev.backed_up + cur.backed_up,
            healthy_skipped: prev.healthy_skipped + cur.healthy_skipped,
            slow1: prev.slow1 + cur.slow1,
            slow2: prev.slow2 + cur.slow2,
            slow3: prev.slow3 + cur.slow3,
            tested_ok: prev.tested_ok + cur.tested_ok,
            marginal: prev.marginal + cur.marginal,
            write_fail: prev.write_fail + cur.write_fail,
        }
    }
}

/// Plan de tramos de una **pasada quirúrgica**: los tramos pendientes que un
/// mapfile ajeno (`--import-map` en scan/repair/refresh/regenerate) declaró
/// objetivos. Se guarda junto al checkpoint para que `--resume` sepa QUÉ
/// tramos quedan por tocar aunque ya no se pase el mapfile original.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangePlan {
    pub mode: RunMode,
    pub unit: u32,
    /// Ventana de la pasada (exclusivo): acota los tramos.
    pub window_start: u32,
    pub window_end: u32,
    /// Mapfile ajeno del que se derivaron los tramos (informativo).
    pub source: String,
    /// Mapa donde la pasada anota lo reparado (RGROWMP1).
    pub result_map: String,
    /// Tramos pendientes `[inicio, fin)`, ascendentes y disjuntos.
    pub ranges: Vec<(u32, u32)>,
}

pub fn ranges_path(log_dir: &Path, unit: u32) -> std::path::PathBuf {
    log_dir.join(format!("DISK{unit}.ranges"))
}

impl RangePlan {
    /// Sectores pendientes totales del plan.
    pub fn total(&self) -> u64 {
        self.ranges.iter().map(|&(a, b)| (b - a) as u64).sum()
    }

    /// Tramos recortados a `[from, to)`. De un tramo que empieza antes de
    /// `from` se conserva la parte restante. Vacío si ya no queda nada.
    pub fn clamped(&self, from: u32, to: u32) -> Vec<(u32, u32)> {
        self.ranges
            .iter()
            .filter_map(|&(a, b)| {
                let a = a.max(from);
                let b = b.min(to);
                if a < b {
                    Some((a, b))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let mut body = String::from("regrow.ranges.v1\r\n");
        body.push_str(&format!(
            "mode={}\r\nunit={}\r\nwindow={} {}\r\nsource={}\r\nresult_map={}\r\n",
            mode_word(self.mode),
            self.unit,
            self.window_start,
            self.window_end,
            self.source,
            self.result_map,
        ));
        for &(a, b) in &self.ranges {
            body.push_str(&format!("{a} {b}\r\n"));
        }
        // Escritura atómica: tempfile + rename (un corte no corrompe el plan).
        let tmp = path.with_extension("ranges.tmp");
        fs::write(&tmp, body.as_bytes())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> io::Result<RangePlan> {
        let text = fs::read_to_string(path)?;
        let bad = || io::Error::other("plan de tramos mal formado");
        let num = |s: &str| s.parse::<u32>().map_err(|_| bad());
        let mut mode: Option<RunMode> = None;
        let mut unit: Option<u32> = None;
        let mut window: Option<(u32, u32)> = None;
        let mut source = String::new();
        let mut result_map = String::new();
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line == "regrow.ranges.v1" || line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                match k {
                    "mode" => {
                        mode = Some(parse_mode(v).ok_or_else(|| io::Error::other("modo inválido"))?);
                    }
                    "unit" => unit = Some(num(v)?),
                    "window" => {
                        let (a, b) = v.split_once(' ').ok_or_else(bad)?;
                        window = Some((num(a)?, num(b)?));
                    }
                    "source" => source = v.to_string(),
                    "result_map" => result_map = v.to_string(),
                    _ => return Err(bad()),
                }
            } else {
                let (a, b) = line.split_once(' ').ok_or_else(bad)?;
                ranges.push((num(a)?, num(b)?));
            }
        }
        let mode = mode.ok_or_else(|| io::Error::other("campo ausente: mode"))?;
        let unit = unit.ok_or_else(|| io::Error::other("campo ausente: unit"))?;
        let (window_start, window_end) = window.ok_or_else(|| io::Error::other("campo ausente: window"))?;
        if ranges.iter().any(|&(a, b)| a >= b)
            || ranges.windows(2).any(|w| w[0].1 > w[1].0 || w[0].1 > w[1].1)
        {
            return Err(io::Error::other("plan de tramos corrupto (vacíos, solapados o desordenados)"));
        }
        Ok(RangePlan {
            mode,
            unit,
            window_start,
            window_end,
            source,
            result_map,
            ranges,
        })
    }

    pub fn remove(path: &Path) {
        let _ = fs::remove_file(path);
    }
}

/// Plan de reanudación validado . Un checkpoint solo se puede
/// reanudar si es del modo que se va a ejecutar, su rango original sigue
/// dentro del disco y todavía quedan sectores pendientes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumePlan {
    pub start: u32,
    pub end: u32,
    /// Sectores que la pasada interrumpida ya completó (se acumulan en el
    /// resumen y sirven para validar que el checkpoint corresponde).
    pub done: u64,
}

pub fn resume_plan(cp: &Checkpoint, mode: RunMode, total_sectors: u64) -> Option<ResumePlan> {
    if cp.mode != mode {
        return None;
    }
    // El disco pudo cambiar (otra unidad en el mismo número) o encogerse.
    let end = (cp.end as u64).min(total_sectors) as u32;
    let start = cp.last_sector.saturating_add(1).min(end);
    if start >= end {
        return None; // rango agotado: no queda nada que reanudar
    }
    Some(ResumePlan {
        start,
        end,
        done: cp.stats.scanned,
    })
}

/// Una línea legible del estado interrumpido (menú y panel de estadísticas).
pub fn resume_note(cp: &Checkpoint) -> String {
    format!(
        "{} interrumpida: {} sectores completados, reanuda en el sector {} (rango {}..{})",
        crate::mode_word(cp.mode),
        cp.stats.scanned,
        cp.last_sector.saturating_add(1),
        cp.start,
        cp.end,
    )
}

/// Checkpoint pendiente de una unidad. `Ok(None)` si no hay archivo o el
/// checkpoint es de otra unidad; `Err` distinto de `NotFound` = archivo dañado
/// (el llamador lo reporta).
pub fn load_pending(log_dir: &Path, unit: u32) -> io::Result<Option<Checkpoint>> {
    match Checkpoint::load(&state_path(log_dir, unit)) {
        Ok(cp) if cp.unit == unit => Ok(Some(cp)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Todos los checkpoints pendientes del directorio de registros (panel de
/// estadísticas), ordenados por unidad. Un archivo ilegible se ignora: el panel
/// de mapa de unidades no debe fallar por un estado corrupto.
pub fn list_pending(log_dir: &Path) -> Vec<(u32, Checkpoint)> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(log_dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if let Some(num) = name.strip_prefix("DISK").and_then(|r| r.strip_suffix(".state")) {
                if let Ok(n) = num.parse::<u32>() {
                    if let Ok(cp) = Checkpoint::load(&path) {
                        if cp.unit == n {
                            out.push((n, cp));
                        }
                    }
                }
            }
        }
    }
    out.sort_by_key(|(u, _)| *u);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_roundtrip_atomico() {
        let dir = std::env::temp_dir().join(format!("regrow_state_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cp = Checkpoint {
            mode: RunMode::Regenerate,
            start: 1000,
            end: 2_000_000,
            last_sector: 459_999,
            stats: EngineStats {
                scanned: 459_000,
                bad: 3,
                recovered: 2,
                delays: 1,
                slow1: 4,
                slow2: 0,
                slow3: 1,
                regenerated_ok: 458_994,
                data_lost: 0,
                backed_up: 2,
                healthy_skipped: 0,
                tested_ok: 5,
                marginal: 2,
                write_fail: 1,
            },
            unit: 7,
        };
        let p = state_path(&dir, 7);
        cp.save(&p).unwrap();
        assert_eq!(p.file_name().unwrap(), "DISK7.state");
        let back = Checkpoint::load(&p).unwrap();
        assert_eq!(back.mode, RunMode::Regenerate);
        assert_eq!(back.start, 1000);
        assert_eq!(back.end, 2_000_000);
        assert_eq!(back.last_sector, 459_999);
        assert_eq!(back.stats, cp.stats);
        assert_eq!(back.unit, 7);
        // sin residuo .tmp
        assert!(!dir.join("DISK7.state.tmp").exists());
        Checkpoint::remove(&p);
        assert!(!p.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkpoint_corrupto_error_claro() {
        let dir = std::env::temp_dir().join(format!("regrow_state_bad_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = state_path(&dir, 1);
        fs::write(&p, "regrow.state.v1\r\nmode=scan\r\nunit=1\r\n").unwrap();
        let e = Checkpoint::load(&p).unwrap_err();
        assert!(e.to_string().contains("ausente"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_stats_suma_contadores() {
        let a = EngineStats {
            scanned: 10,
            bad: 1,
            ..Default::default()
        };
        let b = EngineStats {
            scanned: 5,
            recovered: 4,
            ..Default::default()
        };
        let m = Checkpoint::merge_stats(a, b);
        assert_eq!(m.scanned, 15);
        assert_eq!(m.bad, 1);
        assert_eq!(m.recovered, 4);
    }

    fn cp_scan(start: u32, end: u32, last: u32) -> Checkpoint {
        Checkpoint {
            mode: RunMode::Scan,
            start,
            end,
            last_sector: last,
            stats: EngineStats {
                scanned: (last - start + 1) as u64,
                ..Default::default()
            },
            unit: 3,
        }
    }

    /// El checkpoint solo reanuda su propio modo, y solo si
    /// todavía quedan sectores pendientes dentro del disco.
    #[test]
    fn resume_plan_valida_modo_rango_y_discos() {
        let cp = cp_scan(0, 1_000, 499);
        let p = resume_plan(&cp, RunMode::Scan, 1_000).unwrap();
        assert_eq!((p.start, p.end, p.done), (500, 1_000, 500));
        // Otro modo: no es reanudable con él.
        assert_eq!(resume_plan(&cp, RunMode::Repair, 1_000), None);
        // Rango agotado (la pasada ya completó su rango).
        assert_eq!(resume_plan(&cp_scan(0, 100, 99), RunMode::Scan, 100), None);
        // El disco encogió por debajo del rango guardado: se recorta al total.
        let p = resume_plan(&cp, RunMode::Scan, 600).unwrap();
        assert_eq!((p.start, p.end), (500, 600));
        // El disco es ahora más pequeño que el progreso: nada que reanudar.
        assert_eq!(resume_plan(&cp, RunMode::Scan, 500), None);
        // Nota legible usada por el menú y el panel de estadísticas.
        assert_eq!(
            resume_note(&cp),
            "ESCANEO interrumpida: 500 sectores completados, reanuda en el sector 500 (rango 0..1000)"
        );
    }

    /// `load_pending` filtra por unidad y tolera la ausencia de archivo;
    /// `list_pending` barre el directorio (panel de estadísticas).
    #[test]
    fn pendientes_por_unidad_y_directorio() {
        let dir = std::env::temp_dir().join(format!("regrow_state_pend_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_pending(&dir, 3).unwrap(), None);
        cp_scan(0, 1_000, 499).save(&state_path(&dir, 3)).unwrap();
        let cp_scan2 = Checkpoint {
            unit: 9,
            ..cp_scan(0, 1_000, 499)
        };
        cp_scan2.save(&state_path(&dir, 9)).unwrap();
        // Un checkpoint de otra unidad no se ofrece para esta.
        let got = load_pending(&dir, 3).unwrap().unwrap();
        assert_eq!((got.unit, got.last_sector), (3, 499));
        assert_eq!(load_pending(&dir, 4).unwrap(), None);
        let all = list_pending(&dir);
        assert_eq!(all.iter().map(|(u, _)| *u).collect::<Vec<_>>(), vec![3, 9]);
        // Archivo dañado: load_pending reporta Err (el menú avisa), list_pending
        // lo ignora (el panel no puede fallar por un estado corrupto).
        fs::write(state_path(&dir, 5), "regrow.state.v1\r\nmode=scan\r\n").unwrap();
        assert!(load_pending(&dir, 5).unwrap_err().to_string().contains("ausente"));
        assert_eq!(list_pending(&dir).len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    /// Un checkpoint al que le faltan contadores opcionales (`healthy_skipped`
    /// y las bandas) sigue siendo legible: se leen como 0.
    #[test]
    fn checkpoint_con_campos_ausentes_se_leen() {
        let dir = std::env::temp_dir().join(format!("regrow_state_old_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = state_path(&dir, 2);
        fs::write(
            &p,
            "regrow.state.v1\r\nmode=repair\r\nunit=2\r\nstart=0\r\nend=100\r\n\
             last_sector=39\r\nscanned=40\r\nbad=1\r\nrecovered=0\r\ndelays=0\r\n\
             regenerated_ok=0\r\ndata_lost=0\r\nbacked_up=0\r\n",
        )
        .unwrap();
        let cp = Checkpoint::load(&p).unwrap();
        assert_eq!((cp.mode, cp.last_sector, cp.stats.scanned), (RunMode::Repair, 39, 40));
        assert_eq!(cp.stats.healthy_skipped, 0);
        // Bandas ausentes en el archivo: leídas como 0.
        assert_eq!((cp.stats.slow1, cp.stats.slow2, cp.stats.slow3), (0, 0, 0));
        fs::remove_dir_all(&dir).ok();
    }

    /// Refresh: viaja en el checkpoint, solo reanuda su propio modo y
    /// las bandas acumuladas sobreviven al round-trip.
    #[test]
    fn checkpoint_refresh_roundtrip_y_resume() {
        let dir = std::env::temp_dir().join(format!("regrow_state_refresh_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cp = Checkpoint {
            mode: RunMode::Refresh,
            start: 0,
            end: 1_000,
            last_sector: 39,
            stats: EngineStats {
                scanned: 40,
                slow1: 7,
                slow2: 2,
                slow3: 1,
                ..Default::default()
            },
            unit: 8,
        };
        let p = state_path(&dir, 8);
        cp.save(&p).unwrap();
        let back = Checkpoint::load(&p).unwrap();
        assert_eq!(back.mode, RunMode::Refresh);
        assert_eq!((back.stats.slow1, back.stats.slow2, back.stats.slow3), (7, 2, 1));
        assert_eq!(resume_plan(&back, RunMode::Scan, 1_000), None);
        assert_eq!(resume_plan(&back, RunMode::Refresh, 1_000).unwrap().start, 40);
        Checkpoint::remove(&p);
        fs::remove_dir_all(&dir).ok();
    }

    /// El plan de tramos de una pasada quirúrgica sobrevive a un corte y
    /// permite reanudar sin volver a dar el mapfile original.
    #[test]
    fn plan_de_tramos_roundtrip_y_recorte() {
        let dir = std::env::temp_dir().join(format!("regrow_ranges_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = ranges_path(&dir, 4);
        assert_eq!(
            RangePlan::load(&p).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let plan = RangePlan {
            mode: RunMode::Repair,
            unit: 4,
            window_start: 100,
            window_end: 1000,
            source: "/ruta/sdb5_rescate.log".into(),
            result_map: "/ruta/sdb5_rescate.log.regrow.map".into(),
            ranges: vec![(100, 120), (200, 210), (500, 501)],
        };
        plan.save(&p).unwrap();
        let back = RangePlan::load(&p).unwrap();
        assert_eq!(back, plan);
        assert_eq!(back.total(), 31);
        // Reanudando en 205: del tramo (200,210) queda su parte restante.
        assert_eq!(back.clamped(205, 1000), vec![(205, 210), (500, 501)]);
        // Ventana agotada: nada que reanudar.
        assert_eq!(back.clamped(501, 1000), vec![]);
        // Tramos solapados (archivo corrupto o tocado a mano): no se tragan.
        fs::write(
            &p,
            "regrow.ranges.v1\r\nmode=repair\r\nunit=4\r\nwindow=100 1000\r\nsource=x\r\nresult_map=y\r\n100 300\r\n200 210\r\n",
        )
        .unwrap();
        assert!(RangePlan::load(&p).is_err());
        RangePlan::remove(&p);
        fs::remove_dir_all(&dir).ok();
    }
}
