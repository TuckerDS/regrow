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
pub fn run_speedtest(
    src: &mut dyn SectorSource,
    start: u32,
    end: u32,
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
        let res = src.read_chunk(cur as u64, n, &mut buf[..n * SECTOR_SIZE]);
        let secs = t0.elapsed().as_secs_f64();
        if res.is_err() {
            errors += 1;
            // Reintentar sector a sector para no perder el mapeo de la zona.
            let mut ok = 0u64;
            for s in cur..cur + n as u32 {
                let mut one = [0u8; SECTOR_SIZE];
                if src.read_sector(s as u64, &mut one).is_ok() {
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
        let rep = run_speedtest(&mut m, 0, 4 * SPEED_CHUNK as u32, |_| {});
        assert_eq!(rep.scanned, 4 * SPEED_CHUNK as u64);
        assert_eq!(rep.chunk_secs.len(), 4);
        assert_eq!(rep.errors, 0);
        let slowest = rep.slowest(1);
        assert_eq!(slowest[0].start, 2 * SPEED_CHUNK as u32);
        assert!(slowest[0].secs > 0.05);
        assert!(rep.avg_mb_per_s() > 0.0);
    }
}
