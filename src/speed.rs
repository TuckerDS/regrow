// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! `speedtest`: lectura bulk cronometrada + zonas más lentas.
//!
//! Sirve para caracterizar el medio antes de una reparación: un disco con
//! zonas crónicamente lentas suele indicar degradación mecánica inminente.
//! No escribe nada.

use std::time::{Duration, Instant};

use crate::source::{SectorSource, SECTOR_SIZE};

pub const SPEED_CHUNK: usize = 256; // sectores (128 KiB) por bloque

/// Sondeos por defecto del modo `--seek` (256 posiciones aleatorias: bastante
/// para estimar media/mediana/p95 sin convertir el benchmark en una odisea).
pub const SEEK_SAMPLES: usize = 256;

#[derive(Clone, Debug)]
pub struct ZoneTime {
    pub start: u32, // primer sector del bloque
    pub secs: f64,
}

#[derive(Clone, Debug)]
pub struct SpeedReport {
    pub scanned: u64, // sectores leídos
    pub elapsed: Duration,
    pub chunk_secs: Vec<ZoneTime>,
    pub errors: u64, // bloques que fallaron (se reintentan sector a sector)
}

impl SpeedReport {
    /// Media en MB/s (1 MB = 1048576 B).
    pub fn avg_mb_per_s(&self) -> f64 {
        let bytes = self.scanned as f64 * SECTOR_SIZE as f64;
        let secs = self.elapsed.as_secs_f64().max(1e-9);
        bytes / secs / 1_048_576.0
    }

    /// Bloques más lentos (orden descendente por tiempo), top `n`.
    pub fn slowest(&self, n: usize) -> Vec<&ZoneTime> {
        let mut v: Vec<&ZoneTime> = self.chunk_secs.iter().collect();
        v.sort_by(|a, b| b.secs.partial_cmp(&a.secs).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(n);
        v
    }
}

/// Mide la lectura bulk de [start, end) en bloques de `SPEED_CHUNK` sectores.
/// Con `verify` los bloques se comprueban con la verificación directa del
/// firmware (sin transferir contenido por el bus; degrada a lecturas normales
/// donde el medio no la expone).
pub fn run_speedtest(
    src: &mut dyn SectorSource,
    start: u32,
    end: u32,
    verify: bool,
    op_timeout: Duration,
    mut on_progress: impl FnMut(&[ZoneTime]),
) -> SpeedReport {
    assert!(start < end, "rango vacío");
    let mut buf = vec![0u8; SPEED_CHUNK * SECTOR_SIZE];
    let mut chunk_secs = Vec::new();
    let mut scanned = 0u64;
    let mut errors = 0u64;
    let t_all = Instant::now();

    let mut cur = start;
    while cur < end {
        let n = SPEED_CHUNK.min((end - cur) as usize);
        let t0 = Instant::now();
        let res = if verify {
            src.verify_chunk(cur as u64, n, op_timeout)
        } else {
            src.read_chunk(cur as u64, n, &mut buf[..n * SECTOR_SIZE])
        };
        let secs = t0.elapsed().as_secs_f64();
        if res.is_err() {
            errors += 1;
            // Reintentar sector a sector para no perder el mapeo de la zona.
            let mut ok = 0u64;
            for s in cur..cur + n as u32 {
                let ok_one = if verify {
                    src.verify_chunk(s as u64, 1, op_timeout).is_ok()
                } else {
                    let mut one = [0u8; SECTOR_SIZE];
                    src.read_sector(s as u64, &mut one).is_ok()
                };
                if ok_one {
                    ok += 1;
                }
            }
            scanned += ok;
        } else {
            scanned += n as u64;
        }
        chunk_secs.push(ZoneTime { start: cur, secs });
        if chunk_secs.len() % 16 == 0 {
            on_progress(&chunk_secs);
        }
        cur += n as u32;
    }

    SpeedReport {
        scanned,
        elapsed: t_all.elapsed(),
        chunk_secs,
        errors,
    }
}

// --------------------------- modo seek (--seek) -----------------------------

/// Un sondeo individual: lectura de UN sector en una posición alejada de la
/// anterior — lo que se tarda en posicionar el cabezal y leer (el «seek time»
/// que Victoria mide para comparar medios).
#[derive(Clone, Debug)]
pub struct SeekSample {
    pub lba: u64,
    pub secs: f64,
}

#[derive(Clone, Debug)]
pub struct SeekReport {
    pub samples: Vec<SeekSample>, // solo los completados (los fallidos no enturbian la estadística)
    pub errors: u64,              // lecturas fallidas (se contabilizan aparte)
    pub elapsed: Duration,
}

impl SeekReport {
    pub fn avg_ms(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        self.samples.iter().map(|s| s.secs).sum::<f64>() / self.samples.len() as f64 * 1000.0
    }
    pub fn median_ms(&self) -> f64 {
        self.quantile_ms(0.5)
    }
    pub fn p95_ms(&self) -> f64 {
        self.quantile_ms(0.95)
    }
    fn quantile_ms(&self, q: f64) -> f64 {
        let mut v: Vec<f64> = self.samples.iter().map(|s| s.secs * 1000.0).collect();
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let i = ((v.len() as f64 - 1.0) * q).round() as usize;
        v[i]
    }
    pub fn max_ms(&self) -> f64 {
        self.samples
            .iter()
            .map(|s| s.secs * 1000.0)
            .fold(0.0, f64::max)
    }
    /// Los `n` sondeos más lentos (posición y tiempo).
    pub fn slowest(&self, n: usize) -> Vec<&SeekSample> {
        let mut v: Vec<&SeekSample> = self.samples.iter().collect();
        v.sort_by(|a, b| b.secs.partial_cmp(&a.secs).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(n);
        v
    }
}

/// Generador determinista (xorshift64*): el mapa de sondeos de una pasada es
/// reproducible — dos `--seek` sobre el mismo rango y muestra comparan el
/// mismo conjunto de posiciones, algo que para un benchmark importa.
struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6F_DD1D)
    }
}

/// Sondeo de tiempos de posicionamiento: `samples` lecturas de un sector en
/// posiciones aleatorias SIN REPETICIÓN sobre [start, end), cronometradas una
/// a una. Semilla fija ⇒ posiciones reproducibles. Los fallos no entran en la
/// estadística de tiempos pero sí en `errors` (un seek que además no lee es
/// información, no ruido: el llamador decide).
pub fn run_seektest(
    src: &mut dyn SectorSource,
    start: u32,
    end: u32,
    samples: usize,
    seed: u64,
    mut on_progress: impl FnMut(&[SeekSample]),
) -> SeekReport {
    assert!(start < end, "rango vacío");
    let span = (end - start) as u64;
    let n = (samples as u64).min(span) as usize;
    let mut rng = XorShift64(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed });
    // Posiciones sin repetición: rechazo simple mientras la muestra es pequeña
    // frente al rango; si el rango es corto, barajado completo de las posiciones.
    let positions: Vec<u64> = if span >= (n as u64) * 4 {
        let mut chosen = std::collections::HashSet::with_capacity(n);
        let mut out: Vec<u64> = Vec::with_capacity(n);
        while out.len() < n {
            let p = start as u64 + rng.next() % span;
            if chosen.insert(p) {
                out.push(p);
            }
        }
        out
    } else {
        let mut out: Vec<u64> = (start as u64..end as u64).collect();
        for i in (1..out.len()).rev() {
            out.swap(i, (rng.next() % (i as u64 + 1)) as usize);
        }
        out.truncate(n);
        out
    };

    let mut samples_done: Vec<SeekSample> = Vec::with_capacity(n);
    let mut errors = 0u64;
    let mut one = [0u8; SECTOR_SIZE];
    let t_all = Instant::now();
    for p in positions {
        let t0 = Instant::now();
        let res = src.read_sector(p, &mut one);
        let secs = t0.elapsed().as_secs_f64();
        match res {
            Ok(()) => {
                samples_done.push(SeekSample { lba: p, secs });
                if samples_done.len() % 16 == 0 {
                    on_progress(&samples_done);
                }
            }
            Err(_) => errors += 1,
        }
    }
    SeekReport {
        samples: samples_done,
        errors,
        elapsed: t_all.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct Mock {
        sectors: u64,
        slow_chunk: u64, // LBA inicial del bloque lento
    }

    impl SectorSource for Mock {
        fn sector_count(&self) -> u64 {
            self.sectors
        }
        fn read_sector(&mut self, lba: u64, _buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
            if lba >= self.sectors {
                return Err(io::Error::other("fuera de rango"));
            }
            Ok(())
        }
        fn write_sector(&mut self, _lba: u64, _buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
            Err(io::Error::other("no escribe"))
        }
        fn read_chunk(&mut self, lba: u64, count: usize, buf: &mut [u8]) -> io::Result<()> {
            if lba + count as u64 > self.sectors {
                return Err(io::Error::other("fuera de rango"));
            }
            for b in buf.iter_mut() {
                *b = 0x5A;
            }
            if lba == self.slow_chunk {
                std::thread::sleep(Duration::from_millis(60));
            }
            Ok(())
        }
    }

    #[test]
    fn speedtest_identifica_zona_lenta() {
        let mut m = Mock {
            sectors: 4 * SPEED_CHUNK as u64,
            slow_chunk: 2 * SPEED_CHUNK as u64,
        };
        let rep = run_speedtest(&mut m, 0, 4 * SPEED_CHUNK as u32, false, Duration::from_secs(5), |_| {});
        assert_eq!(rep.scanned, 4 * SPEED_CHUNK as u64);
        assert_eq!(rep.chunk_secs.len(), 4);
        assert_eq!(rep.errors, 0);
        let slowest = rep.slowest(1);
        assert_eq!(slowest[0].start, 2 * SPEED_CHUNK as u32);
        assert!(slowest[0].secs > 0.05);
        assert!(rep.avg_mb_per_s() > 0.0);
    }

    /// `--verify` en speedtest: los bloques se comprueban con verify_chunk
    /// (aquí delega en lecturas) y el resultado es equivalente.
    #[test]
    fn speedtest_con_verify_equivalente() {
        let mut m = Mock {
            sectors: 2 * SPEED_CHUNK as u64,
            slow_chunk: u32::MAX as u64, // ninguno lento
        };
        let rep = run_speedtest(&mut m, 0, 2 * SPEED_CHUNK as u32, true, Duration::from_secs(5), |_| {});
        assert_eq!(rep.scanned, 2 * SPEED_CHUNK as u64);
        assert_eq!(rep.errors, 0);
        assert_eq!(rep.chunk_secs.len(), 2);
    }

    /// Fuente para los sondeos: lee todo salvo una lista de sectores fallidos.
    struct MockSeek {
        sectors: u64,
        bad: std::collections::HashSet<u64>,
    }
    impl SectorSource for MockSeek {
        fn sector_count(&self) -> u64 {
            self.sectors
        }
        fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
            if lba >= self.sectors || self.bad.contains(&lba) {
                return Err(io::Error::other("sin leer"));
            }
            buf.fill(0x5A);
            Ok(())
        }
        fn write_sector(&mut self, _lba: u64, _buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
            Err(io::Error::other("no escribe"))
        }
    }

    #[test]
    fn seektest_sondeos_distintos_dentro_del_rango_y_reproducibles() {
        let mut m = MockSeek {
            sectors: 100_000,
            bad: std::collections::HashSet::new(),
        };
        let rep = run_seektest(&mut m, 1_000, 90_000, 64, 7, |_| {});
        assert_eq!(rep.samples.len(), 64);
        assert_eq!(rep.errors, 0);
        let uniq: std::collections::HashSet<u64> = rep.samples.iter().map(|s| s.lba).collect();
        assert_eq!(uniq.len(), 64, "sin posiciones repetidas");
        assert!(
            rep.samples.iter().all(|s| s.lba >= 1_000 && s.lba < 90_000),
            "fuera del rango pedido"
        );
        assert!(rep.avg_ms() >= 0.0 && rep.median_ms() <= rep.p95_ms() && rep.p95_ms() <= rep.max_ms());
        // Misma semilla ⇒ mismas posiciones (un benchmark debe compararse
        // consigo mismo); otra semilla, mapa distinto.
        let rep2 = run_seektest(&mut m, 1_000, 90_000, 64, 7, |_| {});
        let lbs: Vec<u64> = rep.samples.iter().map(|s| s.lba).collect();
        let lbs2: Vec<u64> = rep2.samples.iter().map(|s| s.lba).collect();
        assert_eq!(lbs, lbs2);
        let rep3 = run_seektest(&mut m, 1_000, 90_000, 64, 8, |_| {});
        let lbs3: Vec<u64> = rep3.samples.iter().map(|s| s.lba).collect();
        assert_ne!(lbs, lbs3);
    }

    #[test]
    fn seektest_contabiliza_fallos_sin_ensuciar_la_estadistica() {
        let bad: std::collections::HashSet<u64> = (0..50_000u64).step_by(13).collect();
        let n_bad = bad.len();
        let mut m = MockSeek {
            sectors: 100_000,
            bad,
        };
        // Con la semilla y el rango fijos forzamos a caer en terreno malo:
        // mitad del rango está enhebrado de sectores ilegibles.
        let rep = run_seektest(&mut m, 0, 100_000, 128, 7, |_| {});
        assert_eq!(rep.samples.len() + rep.errors as usize, 128);
        assert!(rep.errors > 0, "con la mitad del rango podada debe fallar algo");
        assert!(n_bad > 0);
        assert!(
            rep.samples.iter().all(|s| !m.bad.contains(&s.lba)),
            "ningún fallido entra en la estadística"
        );
    }

    #[test]
    fn seektest_rango_corto_barra_todas_las_posiciones() {
        let mut m = MockSeek {
            sectors: 100,
            bad: std::collections::HashSet::new(),
        };
        let rep = run_seektest(&mut m, 10, 20, 256, 7, |_| {});
        assert_eq!(rep.samples.len(), 10, "un rango corto sondea todas sus posiciones");
        let uniq: std::collections::HashSet<u64> = rep.samples.iter().map(|s| s.lba).collect();
        assert_eq!(uniq, (10..20u64).collect::<std::collections::HashSet<u64>>());
    }
}
