// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Relleno aditivo de huecos de una imagen desde una segunda copia del
//! origen (un «donante»: otra imagen del mismo rango).
//!
//! ddrescue 1.29 resuelve esto con `--fill-mode`, pero su relleno es un
//! relleno de PATRÓN: un solo búfer leído una vez del infile se escribe en
//! todos los huecos seleccionados (sirve para borrar o marcar posiciones, no
//! para recuperar datos). Aquí el relleno es posicional y conservador:
//!
//! - Solo se tocan sectores pendientes en el mapa DESTINO; los `+` de la
//!   imagen jamás se reescriben (la misma regla dura que en `rescue`).
//! - Cada sector se respalda en el sidecar ANTES de escribirse, igual que en
//!   `rescue`.
//! - Si el donante aporta su propio mapa (`donor_map`), solo se copia lo que
//!   ese mapa marca `+`; lo demás queda pendiente.
//! - Sin mapa del donante el donante se trata como legible, pero NO se
//!   escriben sectores todo-cero: un hueco de un archivo disperso lee como
//!   ceros y podría disfrazarse de dato recuperado. `allow_zeros` lo permite
//!   explícitamente.
//! - Lo que el donante no puede leer queda pendiente exactamente igual que
//!   antes: el relleno jamás empeora el estado.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::mapfile::{MapFile, MapStatus};
use crate::rescue::ImageBackup;
use crate::source::{FileImage, SectorSource, SECTOR_SIZE};

pub const FILL_SAVE_EVERY_OPS: usize = 256;

#[derive(Clone, Debug)]
pub struct FillOptions {
    pub start: u64,
    pub end: u64, // exclusivo
    pub block: usize,
    /// Escribir también sectores del donante que son todo-cero. Sin mapa del
    /// donante esto es arriesgado (hueco ↔ cero son indistinguibles en un
    /// archivo); con mapa del donante ya se sabe que son datos reales.
    pub allow_zeros: bool,
    /// Sidecar disperso donde respaldar cada sector ANTES de escribirlo
    /// (None = sin respaldo; la regla de campo pide uno).
    pub backup: Option<PathBuf>,
    pub save_every_ops: usize,
}

impl Default for FillOptions {
    fn default() -> Self {
        Self {
            start: 0,
            end: 0,
            block: crate::rescue::RESCUE_BLOCK_DEFAULT,
            allow_zeros: false,
            backup: None,
            save_every_ops: FILL_SAVE_EVERY_OPS,
        }
    }
}

#[derive(Debug)]
pub struct FillReport {
    pub filled: u64,        // sectores escritos desde el donante
    pub zero_skipped: u64,  // sectores no escritos por ser todo-cero sin mapa del donante
    pub donor_missing: u64, // sectores pendientes que el donante no cubre (no-buenos o fuera de su tamaño)
    pub donor_errors: u64,  // sectores cuyo intento de lectura del donante falló
    pub seconds: u64,
}

#[derive(Debug)]
pub struct FillProgress {
    pub cur: u64,
    pub filled: u64,
    pub zero_skipped: u64,
    pub donor_missing: u64,
    pub pending: u64,
}

/// Rangos `+` del mapa del donante, recortados al tamaño real del archivo
/// donante y a la ventana [from, to). Un mapa puede anunciar buen dato más
/// allá del EOF del archivo: eso no existe y no se copia.
pub fn donor_good_ranges(donor_map: &MapFile, donor_sectors: u64, from: u64, to: u64) -> Vec<(u64, u64)> {
    let limit = donor_sectors.min(to);
    let mut out = Vec::new();
    for e in donor_map.entries() {
        if e.status != MapStatus::Good {
            continue;
        }
        let a = e.start.max(from);
        let b = (e.start + e.len).min(limit);
        if a < b {
            out.push((a, b - a));
        }
    }
    out
}

fn sector_is_zero(buf: &[u8]) -> bool {
    buf.iter().all(|b| *b == 0)
}

/// Fallo de un bloque de relleno: `Donor` es normal (el donante no lee ese
/// bloque; queda pendiente y se sigue). `Fatal` es respaldo o escritura en
/// la imagen: aborta el relleno — la imagen jamás se toca sin su respaldo
/// delante (misma semántica que `rescue`).
enum ChunkErr {
    Donor(io::Error),
    Fatal(io::Error),
}

/// Copia un bloque del donante a la imagen aplicando el filtro de ceros por
/// sector cuando procede. Devuelve los sectores efectivamente escritos.
fn copy_block(
    donor: &mut FileImage,
    img: &mut FileImage,
    mut bak: Option<&mut ImageBackup>,
    map: &mut MapFile,
    lba: u64,
    n: usize,
    filter_zeros: bool,
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Result<(u64, u64), ChunkErr> {
    buf.resize(n * SECTOR_SIZE, 0);
    match donor.read_chunk(lba, n, buf) {
        Ok(()) => {}
        Err(e) => return Err(ChunkErr::Donor(e)),
    }
    let mut runs: Vec<(u64, usize)> = Vec::new();
    let mut zeros = 0u64;
    if filter_zeros {
        let mut k = 0usize;
        while k < n {
            if sector_is_zero(&buf[k * SECTOR_SIZE..(k + 1) * SECTOR_SIZE]) {
                zeros += 1;
                k += 1;
            } else {
                let s = k;
                while k < n && !sector_is_zero(&buf[k * SECTOR_SIZE..(k + 1) * SECTOR_SIZE]) {
                    k += 1;
                }
                runs.push((lba + s as u64, k - s));
            }
        }
    } else {
        runs.push((lba, n));
    }
    let mut written = 0u64;
    for (ra, rn) in runs {
        // Respaldo ANTES de sobrescribir: contenido previo de la imagen.
        scratch.resize(rn * SECTOR_SIZE, 0);
        match img.read_chunk(ra, rn, scratch) {
            Ok(()) => {}
            Err(e) => return Err(ChunkErr::Fatal(e)),
        }
        if let Some(b) = bak.as_mut() {
            match b.put_chunk(ra, &scratch[..rn * SECTOR_SIZE]) {
                Ok(()) => {}
                Err(e) => return Err(ChunkErr::Fatal(e)),
            }
        }
        match img.write_chunk(
            ra,
            rn,
            &buf[((ra - lba) as usize) * SECTOR_SIZE..((ra - lba) as usize + rn) * SECTOR_SIZE],
        ) {
            Ok(()) => {}
            Err(e) => return Err(ChunkErr::Fatal(e)),
        }
        map.set(ra, rn as u64, MapStatus::Good, 0);
        written += rn as u64;
    }
    Ok((written, zeros))
}

pub fn run_fill(
    donor: &mut FileImage,
    img: &mut FileImage,
    map: &mut MapFile,
    donor_map: Option<&MapFile>,
    opts: &FillOptions,
    map_path: &Path,
    mut on_progress: impl FnMut(&FillProgress),
) -> io::Result<FillReport> {
    let t0 = Instant::now();
    let end = if opts.end == 0 { map.sector_count } else { opts.end.min(map.sector_count) };
    let block = opts.block.max(1);
    // Sin mapa del donante: todo el archivo donante es candidato, y los ceros
    // se filtran por sector salvo orden explícita. Con mapa: manda el mapa.
    let donor_sectors = donor.sector_count();
    let good = match donor_map {
        Some(dm) => donor_good_ranges(dm, donor_sectors, opts.start, end),
        None => vec![(opts.start, donor_sectors.min(end).saturating_sub(opts.start))],
    };
    let filter_zeros = donor_map.is_none() && !opts.allow_zeros;

    let mut buf: Vec<u8> = Vec::with_capacity(block * SECTOR_SIZE);
    let mut scratch: Vec<u8> = Vec::new();
    let mut bak = match &opts.backup {
        Some(p) => Some(ImageBackup::open(p)?),
        None => None,
    };

    let pend = map.pending_ranges(opts.start, end, &[MapStatus::Failed, MapStatus::Scraped]);
    let mut filled = 0u64;
    let mut zero_skipped = 0u64;
    let mut donor_missing = 0u64;
    let mut donor_errors = 0u64;
    let mut ops = 0usize;
    let mut last_save = Instant::now();
    let mut last_line = Instant::now();

    let maybe_save = |map: &mut MapFile, ops: &mut usize, last_save: &mut Instant| -> io::Result<()> {
        if crate::tui::interrupted() {
            map.save(map_path)?;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "relleno interrumpido — mapfile guardado",
            ));
        }
        *ops += 1;
        if *ops >= opts.save_every_ops.max(1) || last_save.elapsed().as_secs() >= 60 {
            map.save(map_path)?;
            *ops = 0;
            *last_save = Instant::now();
        }
        Ok(())
    };

    fn emit<F: FnMut(&FillProgress)>(
        map: &MapFile,
        on_progress: &mut F,
        cur: u64,
        filled: u64,
        zero_skipped: u64,
        donor_missing: u64,
    ) {
        let (_, f, s, u) = map.stats();
        on_progress(&FillProgress {
            cur,
            filled,
            zero_skipped,
            donor_missing,
            pending: f + s + u,
        });
    }

    for (a, len) in &pend {
        let a_end = a + len;
        let mut pos = *a;
        // Recorre los rangos buenos del donante solapados con este pendiente.
        let mut j = 0usize;
        while j < good.len() && good[j].0 + good[j].1 <= pos {
            j += 1;
        }
        while pos < a_end {
            if j >= good.len() || good[j].0 >= a_end {
                // El donante no cubre el resto de este pendiente.
                donor_missing += a_end - pos;
                break;
            }
            let g0 = good[j].0.max(pos);
            if g0 > pos {
                donor_missing += g0 - pos;
                pos = g0;
            }
            let g1 = (good[j].0 + good[j].1).min(a_end);
            while pos < g1 {
                let n = block.min((g1 - pos) as usize).max(1);
                match copy_block(donor, img, bak.as_mut(), map, pos, n, filter_zeros, &mut buf, &mut scratch) {
                    Ok((w, z)) => {
                        filled += w;
                        zero_skipped += z;
                    }
                    Err(ChunkErr::Donor(e)) => {
                        // El donante no lee este bloque: queda pendiente.
                        donor_errors += n as u64;
                        eprintln!("aviso: donante ilegible en sector {pos}: {e}");
                    }
                    Err(ChunkErr::Fatal(e)) => {
                        // Respaldo o escritura fallida: abortar con el mapa
                        // guardado (la imagen no se toca sin respaldo delante).
                        map.save(map_path)?;
                        return Err(e);
                    }
                }
                pos += n as u64;
                ops += 1;
                maybe_save(map, &mut ops, &mut last_save)?;
                if last_line.elapsed().as_secs() >= 30 {
                    last_line = Instant::now();
                    emit(map, &mut on_progress, pos, filled, zero_skipped, donor_missing);
                }
            }
            j += 1;
        }
        emit(map, &mut on_progress, *a, filled, zero_skipped, donor_missing);
    }

    map.save(map_path)?;
    emit(map, &mut on_progress, end, filled, zero_skipped, donor_missing);
    Ok(FillReport {
        filled,
        zero_skipped,
        donor_missing,
        donor_errors,
        seconds: t0.elapsed().as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileImage;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("regrow-test-fill-{}-{}", std::process::id(), name));
        p
    }

    fn write_sectors(path: &Path, sectors: u64) -> FileImage {
        let f = FileImage::create(path, sectors).unwrap();
        f
    }

    fn pat(seed: u8, n: usize) -> Vec<u8> {
        (0..n).map(|i| (seed + (i as u8 % 7)) | 1).collect() // nunca todo-cero
    }

    #[test]
    fn fill_rellena_huecos_desde_donante_y_respalda() {
        let imgp = tmp("img1");
        let donp = tmp("don1");
        let bakp = tmp("bak1");
        let mapp = tmp("map1");
        let _ = fs::remove_file(&imgp);
        let _ = fs::remove_file(&donp);
        let _ = fs::remove_file(&bakp);

        // Imagen: 100 sectores; 0..10 ya buenos (contenido A), 10..20 hueco.
        let mut img = write_sectors(&imgp, 100);
        img.write_chunk(0, 10, &pat(7, 10 * SECTOR_SIZE)).unwrap();
        // Donante: tiene datos REALDES en 10..20; el resto es hueco (ceros).
        let mut don = write_sectors(&donp, 100);
        let data = pat(5, 10 * SECTOR_SIZE);
        don.write_chunk(10, 10, &data).unwrap();

        let mut map = MapFile::new(100);
        map.set(0, 10, MapStatus::Good, 0);
        // 10..100 quedan pendientes (hueco = Failed).

        let opts = FillOptions {
            start: 0,
            end: 100,
            block: 4,
            allow_zeros: false,
            backup: Some(bakp.clone()),
            save_every_ops: 4,
        };
        let rep = run_fill(&mut don, &mut img, &mut map, None, &opts, &map_path(&mapp), |_| {}).unwrap();

        assert_eq!(rep.filled, 10, "los 10 sectores con datos del donante se copian");
        assert_eq!(rep.zero_skipped, 80, "80 huecos-cero del donante NO se disfrazan de dato");
        assert_eq!(rep.donor_missing, 0);
        // El mapa marca bueno 0..20 y deja 20..100 pendiente.
        let (g, f, s, u) = map.stats();
        assert_eq!(g, 20);
        assert_eq!(f + s + u, 80);
        // La imagen contiene los datos del donante en 10..20.
        let mut got = vec![0u8; 10 * SECTOR_SIZE];
        img.read_chunk(10, 10, &mut got).unwrap();
        assert_eq!(got, data);
        // Lo bueno previo jamás se tocó: sigue siendo contenido A.
        img.read_chunk(0, 10, &mut got).unwrap();
        assert_eq!(got, pat(7, 10 * SECTOR_SIZE));

        for p in [&imgp, &donp, &bakp, &mapp] {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn fill_respeta_el_mapa_del_donante() {
        let imgp = tmp("img2");
        let donp = tmp("don2");
        let mapp = tmp("map2");
        let _ = fs::remove_file(&imgp);
        let _ = fs::remove_file(&donp);

        let mut img = write_sectors(&imgp, 100);
        let mut don = write_sectors(&donp, 100);
        don.write_chunk(10, 10, &pat(3, 10 * SECTOR_SIZE)).unwrap(); // datos en 10..20

        let mut map = MapFile::new(100);
        map.set(0, 10, MapStatus::Good, 0);

        // Mapa del donante: SOLO 12..16 son `+` (el resto del donante, aunque
        // tenga bytes, no está certificado).
        let mut dmap = MapFile::new(100);
        dmap.set(12, 4, MapStatus::Good, 0);

        let opts = FillOptions {
            start: 0,
            end: 100,
            block: 8,
            allow_zeros: false,
            backup: None,
            save_every_ops: 8,
        };
        let rep = run_fill(&mut don, &mut img, &mut map, Some(&dmap), &opts, &map_path(&mapp), |_| {}).unwrap();

        assert_eq!(rep.filled, 4, "solo los 4 sectores que el mapa del donante certifica");
        assert_eq!(rep.zero_skipped, 0, "con mapa del donante no hace falta filtrar ceros");
        assert_eq!(rep.donor_missing, 86, "10..12 y 16..100 sin certificar quedan pendientes");
        let (g, _, _, _) = map.stats();
        assert_eq!(g, 14); // 0..10 previos + 12..16 nuevos

        for p in [&imgp, &donp, &mapp] {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn fill_allow_zeros_escribe_ceros_certificados_por_ausencia_de_mapa() {
        let imgp = tmp("img3");
        let donp = tmp("don3");
        let mapp = tmp("map3");
        let _ = fs::remove_file(&imgp);
        let _ = fs::remove_file(&donp);

        let mut img = write_sectors(&imgp, 40);
        let mut don = write_sectors(&donp, 40); // donante todo hueco (ceros)

        let mut map = MapFile::new(40);
        let opts = FillOptions {
            start: 0,
            end: 40,
            block: 8,
            allow_zeros: true,
            backup: None,
            save_every_ops: 8,
        };
        let rep = run_fill(&mut don, &mut img, &mut map, None, &opts, &map_path(&mapp), |_| {}).unwrap();
        assert_eq!(rep.filled, 40, "--allow-zeros escribe incluso sectores cero");
        assert_eq!(rep.zero_skipped, 0);

        for p in [&imgp, &donp, &mapp] {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn fill_no_copia_mas_alla_del_tamanio_real_del_donante() {
        let imgp = tmp("img4");
        let donp = tmp("don4");
        let mapp = tmp("map4");
        let _ = fs::remove_file(&imgp);
        let _ = fs::remove_file(&donp);

        let mut img = write_sectors(&imgp, 100);
        img.write_chunk(0, 10, &pat(9, 10 * SECTOR_SIZE)).unwrap();
        // Donante truncado: solo 30 sectores existen (como la copia de campo).
        let mut don = write_sectors(&donp, 30);
        don.write_chunk(10, 20, &pat(4, 20 * SECTOR_SIZE)).unwrap();

        let mut map = MapFile::new(100);
        map.set(0, 10, MapStatus::Good, 0);
        // Mapa del donante optimista: certifica hasta el sector 60, pero el
        // archivo solo llega a 30: lo de 30..60 no existe y no se copia.
        let mut dmap = MapFile::new(100);
        dmap.set(10, 50, MapStatus::Good, 0);

        let opts = FillOptions {
            start: 0,
            end: 100,
            block: 16,
            allow_zeros: true,
            backup: None,
            save_every_ops: 16,
        };
        let rep = run_fill(&mut don, &mut img, &mut map, Some(&dmap), &opts, &map_path(&mapp), |_| {}).unwrap();

        assert_eq!(rep.filled, 20, "solo existe donante hasta el sector 30");
        assert_eq!(rep.donor_missing, 70, "30..100 sin donante real");

        for p in [&imgp, &donp, &mapp] {
            let _ = fs::remove_file(p);
        }
    }

    // small helper so the tests read naturally
    fn map_path(p: &Path) -> PathBuf {
        p.to_path_buf()
    }
}
