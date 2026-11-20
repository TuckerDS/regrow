// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Rescate disco→imagen con fases encadenadas y mapfile propio.
//!
//! Fases (cada una opera solo sobre lo aún pendiente, según el mapa):
//! 1. DIRECTA — lectura bulk por bloques; un bloque fallido se marca y se
//!    SALTA (no se frena el avance). Los bloques buenos se copian enteros.
//! 2. DIVISIÓN BINARIA — cada rango fallido se parte por la mitad
//!    recursivamente hasta el sector individual, copiando todo sub-bloque
//!    que ahora lee bien. Aísla el daño sin releer lo sano.
//! 3. RASCADO — los sectores fallidos individuales se reintentan `retries`
//!    veces; los que agotan los reintentos quedan `?` (rascados).
//! 4. PASADA INVERSA — los pendientes se releen de mayor a menor LBA: en
//!    muchos USB/multisegmento ciertos sectores ceden solo al cambiar el
//!    orden y la dirección de acceso.
//!
//! Seguridad: la imagen se crea SPARSE del tamaño exacto del origen; un
//! sector que nunca se pudo leer queda como hueco honesto (no se inventan
//! ceros como "datos"). El mapa se guarda de forma atómica y periódica:
//! cualquier pasada siguiente con `--resume` retoma sin releer un solo byte
//! de lo ya bueno.

use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::mapfile::{MapFile, MapStatus};
use crate::source::{FileImage, SectorSource, SECTOR_SIZE};

pub const RESCUE_BLOCK_DEFAULT: usize = 128; // sectores (64 KiB por bloque)
pub const RESCUE_RETRIES_DEFAULT: u8 = 3;
pub const RESCUE_SAVE_EVERY_OPS: usize = 128; // guardado del mapa cada N operaciones

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Direct,
    Split,
    Scrape,
    Reverse,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Direct => "directa",
            Phase::Split => "división",
            Phase::Scrape => "rascado",
            Phase::Reverse => "inversa",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RescueOptions {
    pub start: u64,
    pub end: u64, // exclusivo
    pub block: usize,
    pub retries: u8,
    pub reverse: bool,
    pub save_every_ops: usize,
    /// Descanso entre reintentos de un sector y tras bloque fallido: en un
    /// clúster malo el cabezal agradece pausar (campo: mejor horas de más que
    /// un daño adicional). 0 = sin pausas.
    pub retry_wait: Duration,
    /// Umbral °C: si el dispositivo supera la temperatura, el rescate PAUSA
    /// (espera a enfriar; aborta tras 10 min sin bajar). Requiere `device`.
    pub max_temp: Option<u64>,
    /// Dispositivo origen para el guard de temperatura (None = sin guard).
    pub device: Option<PathBuf>,
}

impl Default for RescueOptions {
    fn default() -> Self {
        Self {
            start: 0,
            end: 0,
            block: RESCUE_BLOCK_DEFAULT,
            retries: RESCUE_RETRIES_DEFAULT,
            reverse: true,
            save_every_ops: RESCUE_SAVE_EVERY_OPS,
            retry_wait: Duration::from_secs(2),
            max_temp: None,
            device: None,
        }
    }
}

#[derive(Debug)]
pub struct RescueReport {
    pub good: u64,
    pub failed: u64,
    pub scraped: u64,
    pub untried: u64,
    #[allow(dead_code)] // diagnóstico: usados por el panel/futuro JSON
    pub blocks_ok: u64,
    #[allow(dead_code)]
    pub block_errors: u64,
    pub seconds: u64,
}

#[derive(Debug)]
pub struct Progress {
    pub phase: Phase,
    pub cur: u64,
    pub good: u64,
    pub pending: u64,
    pub blocks_ok: u64,
    pub block_errors: u64,
}

/// Crea (o valida) la imagen sparse: tamaño exacto del origen para que el
/// offset del archivo sea el LBA 1:1. Una imagen existente de otro tamaño
/// se rechaza: jamás se trunca un rescate previo.
pub fn prepare_image(path: &Path, sectors: u64) -> io::Result<FileImage> {
    if path.exists() {
        let len = std::fs::metadata(path)?.len();
        let want = sectors * SECTOR_SIZE as u64;
        if len != want {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "la imagen existente tiene {len} bytes y se requieren {want} (no se toca una imagen de otro tamaño: usa otra ruta o bórra a conciencia)"
                ),
            ));
        }
        FileImage::open_rw(path)
    } else {
        FileImage::create(path, sectors)
    }
}

fn try_block(src: &mut dyn SectorSource, img: &mut FileImage, lba: u64, n: usize, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.resize(n * SECTOR_SIZE, 0);
    src.read_chunk(lba, n, buf)?;
    img.write_chunk(lba, n, buf)
}

fn emit_progress<F: FnMut(&Progress)>(
    map: &MapFile,
    on_progress: &mut F,
    phase: Phase,
    cur: u64,
    blocks_ok: u64,
    block_errors: u64,
) {
    let (g, f, s, u) = map.stats();
    on_progress(&Progress {
        phase,
        cur,
        good: g,
        pending: f + s + u,
        blocks_ok,
        block_errors,
    });
}

pub fn run_rescue(
    src: &mut dyn SectorSource,
    img: &mut FileImage,
    map: &mut MapFile,
    opts: &RescueOptions,
    map_path: &Path,
    mut on_progress: impl FnMut(&Progress),
) -> io::Result<RescueReport> {
    let t0 = Instant::now();
    let mut buf: Vec<u8> = Vec::with_capacity(opts.block.max(1) * SECTOR_SIZE);
    let mut blocks_ok = 0u64;
    let mut block_errors = 0u64;
    let mut ops = 0usize;
    let mut last_save = Instant::now();
    let mut last_temp = Instant::now();

    // Descanso tras cada fallo: en un clúster malo el cabezal agradece pausar.
    let rest = |opts: &RescueOptions| {
        if opts.retry_wait > Duration::ZERO {
            thread::sleep(opts.retry_wait.min(Duration::from_secs(30)));
        }
    };

    let mut maybe_save = |map: &mut MapFile, ops: &mut usize, last_save: &mut Instant| -> io::Result<()> {
        if crate::tui::interrupted() {
            // Ctrl+C/kill: guardar el mapa y salir; --resume retoma exacto.
            map.save(map_path)?;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "rescate interrumpido — mapfile guardado",
            ));
        }
        *ops += 1;
        if *ops >= opts.save_every_ops.max(1) || last_save.elapsed().as_secs() >= 60 {
            map.save(map_path)?;
            *ops = 0;
            *last_save = Instant::now();
        }
        // Guard de temperatura (sondeo como mucho cada 45 s): el rescate PAUSA
        // mientras el disco enfría; si no baja del umbral en 10 min, se detiene
        // con el mapa guardado. La duración no importa; el disco, sí.
        if let (Some(dev), Some(max)) = (opts.device.as_deref(), opts.max_temp) {
            if last_temp.elapsed() >= Duration::from_secs(45) {
                last_temp = Instant::now();
                if let Ok(si) = crate::smart::query(dev) {
                    if let Some(t0) = si.temperature_c {
                        let mut t = t0;
                        if t > max {
                            eprintln!("aviso: {t} °C (umbral {max} °C) — enfriando antes de seguir…");
                            let tc = Instant::now();
                            while t > max.saturating_sub(2) {
                                thread::sleep(Duration::from_secs(10));
                                if tc.elapsed() >= Duration::from_secs(600) {
                                    map.save(map_path)?;
                                    return Err(io::Error::new(
                                        io::ErrorKind::TimedOut,
                                        format!(
                                            "el disco sigue a {t} °C tras 10 min — rescate detenido (mapfile guardado)"
                                        ),
                                    ));
                                }
                                if let Ok(si2) = crate::smart::query(dev) {
                                    if let Some(t2) = si2.temperature_c {
                                        t = t2;
                                    }
                                }
                            }
                            println!("temperatura recuperada: {t} °C — reanudando");
                        }
                    }
                }
            }
        }
        Ok(())
    };

    // ---------------------------------------------------- fase 1: directa ----
    let ranges = map.pending_ranges(opts.start, opts.end, &[MapStatus::Failed]);
    for (a, len) in ranges {
        let mut pos = a;
        while pos < a + len {
            let n = opts.block.min((a + len - pos) as usize).max(1);
            match try_block(src, img, pos, n, &mut buf) {
                Ok(()) => {
                    map.set(pos, n as u64, MapStatus::Good, 0);
                    blocks_ok += 1;
                }
                Err(_) => {
                    map.set(pos, n as u64, MapStatus::Failed, 0);
                    block_errors += 1;
                    rest(&opts);
                }
            }
            pos += n as u64;
            maybe_save(map, &mut ops, &mut last_save)?;
            emit_progress(map, &mut on_progress, Phase::Direct, pos, blocks_ok, block_errors);
        }
    }
    map.save(map_path)?;

    // -------------------------------------------------- fase 2: división ----
    loop {
        let work: Vec<(u64, u64)> = map
            .entries()
            .iter()
            .filter(|e| e.status == MapStatus::Failed && e.len > 1 && e.start >= opts.start && e.start < opts.end)
            .map(|e| (e.start, e.len))
            .collect();
        if work.is_empty() {
            break;
        }
        for (a, len) in work {
            let mid = (len / 2).max(1);
            for (sa, sl) in [(a, mid), (a + mid, len - mid)] {
                if sl == 0 {
                    continue;
                }
                match try_block(src, img, sa, sl as usize, &mut buf) {
                    Ok(()) => {
                        map.set(sa, sl, MapStatus::Good, 0);
                        blocks_ok += 1;
                    }
                    Err(_) => {
                        map.set(sa, sl, MapStatus::Failed, 0);
                        block_errors += 1;
                        rest(&opts);
                    }
                }
                maybe_save(map, &mut ops, &mut last_save)?;
                emit_progress(map, &mut on_progress, Phase::Split, sa + sl, blocks_ok, block_errors);
            }
        }
    }
    map.save(map_path)?;

    // --------------------------------------------------- fase 3: rascado ----
    // Cada candidato se trabaja UNA vez por pasada (los que agoten los
    // reintentos quedan `?`; los reintentará de nuevo una próxima pasada).
    let singles: Vec<u64> = map
        .entries()
        .iter()
        .filter(|e| e.len == 1 && matches!(e.status, MapStatus::Failed | MapStatus::Scraped) && e.start >= opts.start && e.start < opts.end)
        .map(|e| e.start)
        .collect();
    for lba in singles {
            let mut recovered = false;
            for attempt in 1..=opts.retries.max(1) {
                match try_block(src, img, lba, 1, &mut buf) {
                    Ok(()) => {
                        map.set(lba, 1, MapStatus::Good, attempt - 1);
                        blocks_ok += 1;
                        recovered = true;
                        break;
                    }
                    Err(_) => {
                        map.set(lba, 1, MapStatus::Failed, attempt as u8);
                        block_errors += 1;
                        // pausa entre tentativas del mismo sector (no tras la última)
                        if attempt < opts.retries.max(1) {
                            rest(&opts);
                        }
                    }
                }
            }
            if !recovered {
                map.set(lba, 1, MapStatus::Scraped, opts.retries.max(1));
            }
            maybe_save(map, &mut ops, &mut last_save)?;
            emit_progress(map, &mut on_progress, Phase::Scrape, lba, blocks_ok, block_errors);
        }
    map.save(map_path)?;

    // ------------------------------------------------ fase 4: pasada inversa --
    if opts.reverse {
        let ranges = map.pending_ranges(opts.start, opts.end, &[MapStatus::Failed, MapStatus::Scraped]);
        for (a, len) in ranges.into_iter().rev() {
            let mut pos_ex = a + len;
            while pos_ex > a {
                let n = opts.block.min((pos_ex - a) as usize).max(1);
                pos_ex -= n as u64;
                if let Ok(()) = try_block(src, img, pos_ex, n, &mut buf) {
                    map.set(pos_ex, n as u64, MapStatus::Good, 0);
                    blocks_ok += 1;
                } else {
                    block_errors += 1;
                    rest(&opts);
                }
                maybe_save(map, &mut ops, &mut last_save)?;
                emit_progress(map, &mut on_progress, Phase::Reverse, pos_ex, blocks_ok, block_errors);
            }
        }
    }

    map.save(map_path)?;
    let (good, failed, scraped, untried) = map.stats();
    Ok(RescueReport {
        good,
        failed,
        scraped,
        untried,
        blocks_ok,
        block_errors,
        seconds: t0.elapsed().as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SECTOR_SIZE;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;

    /// Fuente sintética: contenido de sector = byte constante derivada del
    /// LBA. `never_ok` nunca deja leer; `fail_times[l]` falla las primeras
    /// N tentativas (cuentan TODAS las lecturas, incluidas las de bloques).
    struct MockRescue {
        sectors: u64,
        never_ok: HashSet<u64>,
        fail_times: HashMap<u64, usize>,
        attempts: RefCell<HashMap<u64, usize>>,
        reads_other: RefCell<u64>, // lecturas de sectores distintos de los marcados
    }

    impl MockRescue {
        fn new(sectors: u64) -> Self {
            Self {
                sectors,
                never_ok: HashSet::new(),
                fail_times: HashMap::new(),
                attempts: RefCell::new(HashMap::new()),
                reads_other: RefCell::new(0),
            }
        }
        fn mark(&mut self, lba: u64, times: usize) {
            self.fail_times.insert(lba, times);
        }
        fn poison(&mut self, lba: u64) {
            self.never_ok.insert(lba);
        }
        fn byte_for(lba: u64) -> u8 {
            ((lba % 251) + 7) as u8
        }
    }

    impl SectorSource for MockRescue {
        fn sector_count(&self) -> u64 {
            self.sectors
        }
        fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
            let mut att = self.attempts.borrow_mut();
            let n = att.entry(lba).or_insert(0);
            *n += 1;
            if self.never_ok.contains(&lba) {
                return Err(io::Error::new(io::ErrorKind::Other, "ilegible"));
            }
            if let Some(limit) = self.fail_times.get(&lba) {
                if *n <= *limit {
                    return Err(io::Error::new(io::ErrorKind::Other, "retardo/excedido"));
                }
            }
            if !self.fail_times.contains_key(&lba) && !self.never_ok.contains(&lba) {
                *self.reads_other.borrow_mut() += 1;
            }
            buf.fill(Self::byte_for(lba));
            Ok(())
        }
        fn write_sector(&mut self, _lba: u64, _buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
            Ok(())
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("regrow_rescue_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn opts(sectors: u64, block: usize, retries: u8, reverse: bool) -> RescueOptions {
        RescueOptions {
            start: 0,
            end: sectors,
            block,
            retries,
            reverse,
            save_every_ops: 16,
            retry_wait: Duration::ZERO, // tests sin pausas
            max_temp: None,
            device: None,
        }
    }

    #[test]
    fn rescate_aisla_ilegible_permanente_sin_tocar_lo_sano() {
        let dir = tmpdir("isla");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 1024u64;
        let mut src = MockRescue::new(sectors);
        src.poison(100);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        let rep = run_rescue(&mut src, &mut img, &mut map, &opts(sectors, 64, 3, true), &map_path, |_| {}).unwrap();
        assert_eq!(rep.good, 1023);
        assert_eq!(rep.scraped, 1);
        assert_eq!(rep.failed, 0);
        assert_eq!(rep.untried, 0);

        // Contenido copiado sector a sector; el ilegible queda como hueco cero.
        let bytes = fs::read(&img_path).unwrap();
        for lba in [0u64, 99, 101, 1023] {
            let off = (lba as usize) * SECTOR_SIZE;
            assert_eq!(bytes[off], MockRescue::byte_for(lba), "lba {lba}");
        }
        let off = 100 * SECTOR_SIZE;
        assert!(bytes[off..off + SECTOR_SIZE].iter().all(|&b| b == 0));

        // Mapa persistido y releíble: el rascado queda marcado con reintentos.
        let loaded = MapFile::load(&map_path).unwrap();
        let e = loaded.entries().iter().find(|e| e.start == 100).unwrap();
        assert_eq!((e.len, e.status), (1, MapStatus::Scraped));
        assert!(e.retries >= 3);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_no_vuelve_a_leer_lo_bueno() {
        let dir = tmpdir("resume");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 1024u64;
        let mut src = MockRescue::new(sectors);
        src.poison(100);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        run_rescue(&mut src, &mut img, &mut map, &opts(sectors, 64, 3, true), &map_path, |_| {}).unwrap();
        *src.reads_other.borrow_mut() = 0; // reinicio el contador de lo sano

        // Segunda pasada con --resume: el mapa ya conoce lo bueno.
        let mut map2 = MapFile::load(&map_path).unwrap();
        let rep2 = run_rescue(&mut src, &mut img, &mut map2, &opts(sectors, 64, 3, true), &map_path, |_| {}).unwrap();
        assert_eq!(*src.reads_other.borrow(), 0, "resume no debe releer un sector sano");
        assert_eq!(rep2.good, 1023);
        assert_eq!(rep2.scraped, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rascado_recupera_sector_flaky() {
        let dir = tmpdir("flaky");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 1024u64;
        let mut src = MockRescue::new(sectors);
        src.mark(300, 2); // falla 2 veces, la 3ª lee
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        // Con block=1 no hay división previa: las lecturas previas son directas.
        let rep = run_rescue(&mut src, &mut img, &mut map, &opts(sectors, 1, 3, false), &map_path, |_| {}).unwrap();
        assert_eq!(rep.good, 1024);
        assert_eq!(rep.scraped, 0);
        let bytes = fs::read(&img_path).unwrap();
        let off = 300 * SECTOR_SIZE;
        assert_eq!(bytes[off], MockRescue::byte_for(300));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pasada_inversa_recupera_lo_que_el_rascado_no_pudo() {
        let dir = tmpdir("rev");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 1024u64;
        let mut src = MockRescue::new(sectors);
        src.mark(500, 2); // directa(1) + rascado(1) fallan; la inversa es la 3ª lectura
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        let rep = run_rescue(&mut src, &mut img, &mut map, &opts(sectors, 1, 1, true), &map_path, |_| {}).unwrap();
        assert_eq!(rep.good, 1024, "la inversa debe rescatar el sector");
        assert_eq!(rep.scraped, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rechaza_imagen_existente_de_otro_tamano() {
        let dir = tmpdir("size");
        let img_path = dir.join("d.raw");
        fs::write(&img_path, vec![0u8; 4096]).unwrap();
        let err = prepare_image(&img_path, 1024).unwrap_err();
        assert!(err.to_string().contains("no se toca"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn continua_un_mapfile_de_texto_ajeno() {
        // Escenario real: continuar un rescate ajeno descrito en un mapfile
        // de texto (estilo ddrescue) — solo lo pendiente se relee.
        let dir = tmpdir("extmap");
        let txt = dir.join("ajeno.map");
        fs::write(
            &txt,
            concat!(
                "# Mapfile. Created by herramienta ajena\n",
                "#      pos        size  status\n",
                "0x00000000  0x00040000  +\n",
                "0x00040000  0x00000200  ?\n",
                "0x00040200  0x0003FE00  -\n",
            ),
        )
        .unwrap();
        let sectors = 1024u64;
        let mut map = MapFile::import_text(&txt, sectors).unwrap();
        let mut src = MockRescue::new(sectors);
        src.poison(0x200);
        let mut img = prepare_image(&dir.join("continua.raw"), sectors).unwrap();
        let rep = run_rescue(&mut src, &mut img, &mut map, &opts(sectors, 16, 2, false), &dir.join("own.map"), |_| {}).unwrap();
        assert_eq!(rep.good, 1023);
        assert_eq!(rep.scraped, 1);
        assert_eq!(*src.reads_other.borrow(), 0x1FF, "el mapa ajeno evita releer el dominio ya bueno (511 sectores relectos)");
        let _ = fs::remove_dir_all(&dir);
    }
}
