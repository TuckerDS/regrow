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

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
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

/// Política de «zona lenta» de la fase directa (al estilo ddrescue `-a`).
/// Un bloque que se lee más despacio del umbral NO se copia en ese momento:
/// queda pendiente y lo retrabajan las fases lentas (división y rascado),
/// que sí aceptan lecturas lentas — así la pasada rápida no acampa sobre la
/// zona mala, pero ningún dato leído se pierde jamás.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinRate {
    /// Desactivado (comportamiento por defecto).
    Off,
    /// Umbral = un décimo del ritmo medio de copia alcanzado hasta ahora
    /// (suelo 1 KiB/s, como el `auto` de ddrescue).
    Auto,
    /// Umbral explícito, en bytes/s.
    Bytes(u64),
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
    /// Sidecar disperso donde respaldar cada sector ANTES de sobrescribirlo
    /// en la imagen (None = sin respaldo; la regla de campo pide uno).
    pub backup: Option<PathBuf>,
    /// Techo de bytes/s copiados a la imagen (estilo ddrescue `-Z`): al
    /// superarlo en una ventana de 1 s, el rescate duerme hasta cerrarla.
    /// Sirve para convivir con una máquina en producción. None = sin techo.
    pub rate_limit: Option<u64>,
    /// Política de zona lenta en la FASE DIRECTA (estilo ddrescue `-a`): ver
    /// `MinRate`. Las fases lentas (división/rascado) siempre aceptan lo que
    /// se pueda leer, por lento que sea — nunca se descarta un dato leído.
    pub min_read_rate: MinRate,
    /// Aborto protector: si los INTENTOS de lectura fallidos superan esta
    /// tasa (bytes/s en ventana de 1 s), el rescate se detiene con el
    /// mapfile guardado. None = desactivado.
    pub max_error_rate: Option<u64>,
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
            backup: None,
            rate_limit: None,
            min_read_rate: MinRate::Off,
            max_error_rate: None,
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
    /// Bloques que la fase directa declaró lentos y difirió a las fases lentas.
    pub slow_deferred: u64,
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
    /// Bytes efectivamente copiados a la imagen en esta corrida (para el ritmo).
    pub copied_bytes: u64,
    pub slow_deferred: u64,
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

/// Fallo de un intento de rescate. `Read` es un fallo normal del origen (el
/// sector se reintentará con la lógica de su fase). `Fatal` es un fallo del
/// respaldo o de la escritura en la imagen: aborta el rescate entero — la
/// imagen jamás se toca sin su respaldo sector a sector delante.
enum TryErr {
    /// Fallo de lectura del origen: normal en el rescate (lo reintenta la
    /// fase correspondiente); se conserva para diagnósticos futuros.
    Read(#[allow(dead_code)] io::Error),
    /// La lectura terminó bien pero tardó más que el presupuesto de la fase
    /// directa (`--min-read-rate`): el bloque NO se copia ahora; lo retrabajan
    /// las fases lentas. Lleva la tardanza real para el aviso.
    Slow(Duration),
    /// Fallo del respaldo o de la escritura en la imagen: aborta el rescate.
    Fatal(io::Error),
}

/// Ventana de 1 s para el techo de tasa (`--rate-limit`): al pasarse del
/// límite, devuelve cuánto hay que dormir para cerrar la ventana.
struct Throttle {
    limit: u64,
    win_start: Option<Instant>,
    bytes: u64,
}

impl Throttle {
    fn feed(&mut self, now: Instant, b: u64) -> Option<Duration> {
        match self.win_start {
            None => {
                self.win_start = Some(now);
                self.bytes = b;
                None
            }
            Some(t) => {
                let el = now.duration_since(t);
                if el >= Duration::from_secs(1) {
                    self.win_start = Some(now);
                    self.bytes = b;
                    return None;
                }
                self.bytes = self.bytes.saturating_add(b);
                if self.bytes > self.limit {
                    let rest_ns = 1_000_000_000u128.saturating_sub(el.as_nanos());
                    Some(Duration::from_millis((rest_ns / 1_000_000).max(1).min(5000) as u64))
                } else {
                    None
                }
            }
        }
    }
}

/// Ventana de 1 s para la tasa máxima de intentos fallidos
/// (`--max-error-rate`): `true` = se superó el umbral (abortar).
struct ErrMeter {
    limit: u64,
    win_start: Option<Instant>,
    bytes: u64,
}

impl ErrMeter {
    fn feed(&mut self, now: Instant, b: u64) -> bool {
        match self.win_start {
            None => {
                self.win_start = Some(now);
                self.bytes = b;
            }
            Some(t) => {
                if now.duration_since(t) >= Duration::from_secs(1) {
                    self.win_start = Some(now);
                    self.bytes = b;
                } else {
                    self.bytes = self.bytes.saturating_add(b);
                }
            }
        }
        self.bytes > self.limit
    }
}

/// Respaldo sector a sector ANTES de sobrescribir: un sidecar disperso
/// (`<imagen>.respaldo` por defecto) guarda, en el offset propio de cada
/// sector, el contenido que tenía la IMAGEN justo antes de esa escritura.
/// Un sector cuya lectura falla no se escribe y por tanto no se respalda: el
/// sidecar solo contiene contenido previo real. Con `--resume` se reabre sin
/// truncar: los respaldos de pasadas anteriores se conservan.
pub struct ImageBackup {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl ImageBackup {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub(crate) fn put_chunk(&mut self, lba: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
        self.file.write_all(data)?;
        Ok(())
    }
}

impl Drop for ImageBackup {
    fn drop(&mut self) {
        // El respaldo debe sobrevivir incluso a un abort del proceso.
        let _ = self.file.sync_all();
    }
}

/// Copia el bloque `src → img` solo después de respaldar en `bak` el contenido
/// que la imagen tiene ahora en ese rango. Orden deliberado: primero se LEE
/// el origen (si falla, no hay nada que escribir ni respaldar).
/// `slow_over`: presupuesto de lectura de la fase directa; si la lectura
/// termina bien pero tardó más que el presupuesto, el bloque se declara
/// `Slow` ANTES de respaldar o escribir (la imagen no se toca). Devuelve la
/// duración real de la lectura.
fn try_block(
    src: &mut dyn SectorSource,
    img: &mut FileImage,
    bak: Option<&mut ImageBackup>,
    lba: u64,
    n: usize,
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    slow_over: Option<Duration>,
) -> Result<Duration, TryErr> {
    buf.resize(n * SECTOR_SIZE, 0);
    let t = Instant::now();
    src.read_chunk(lba, n, buf).map_err(TryErr::Read)?;
    let dur = t.elapsed();
    if let Some(bud) = slow_over {
        if dur > bud {
            return Err(TryErr::Slow(dur));
        }
    }
    if let Some(b) = bak {
        scratch.resize(n * SECTOR_SIZE, 0);
        img.read_chunk(lba, n, scratch).map_err(TryErr::Fatal)?;
        b.put_chunk(lba, &scratch[..n * SECTOR_SIZE])
            .map_err(TryErr::Fatal)?;
    }
    img.write_chunk(lba, n, buf).map_err(TryErr::Fatal)?;
    Ok(dur)
}

fn emit_progress<F: FnMut(&Progress)>(
    map: &MapFile,
    on_progress: &mut F,
    phase: Phase,
    cur: u64,
    blocks_ok: u64,
    block_errors: u64,
    copied_bytes: u64,
    slow_deferred: u64,
) {
    let (g, f, s, u) = map.stats();
    on_progress(&Progress {
        phase,
        cur,
        good: g,
        pending: f + s + u,
        blocks_ok,
        block_errors,
        copied_bytes,
        slow_deferred,
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
    let mut scratch: Vec<u8> = Vec::new();
    let mut blocks_ok = 0u64;
    let mut block_errors = 0u64;
    let mut ops = 0usize;
    let mut last_save = Instant::now();
    let mut last_temp = Instant::now();
    let mut copied_bytes = 0u64;
    let mut slow_deferred = 0u64;

    // El respaldo se abre ANTES de tocar nada: si no se puede crear el
    // sidecar, el rescate ni siquiera empieza.
    let mut bak = match &opts.backup {
        Some(p) => Some(ImageBackup::open(p)?),
        None => None,
    };

    // Descanso tras cada fallo: en un clúster malo el cabezal agradece pausar.
    let rest = |opts: &RescueOptions| {
        if opts.retry_wait > Duration::ZERO {
            thread::sleep(opts.retry_wait.min(Duration::from_secs(30)));
        }
    };

    // Techos de ritmo y de errores (ventanas de 1 s).
    let mut throttle = opts.rate_limit.map(|l| Throttle {
        limit: l,
        win_start: None,
        bytes: 0,
    });
    let mut errmeter = opts.max_error_rate.map(|l| ErrMeter {
        limit: l,
        win_start: None,
        bytes: 0,
    });
    // Un fallo que cruza la tasa máxima detiene el rescate con el mapa a salvo.
    let rate_abort = || {
        io::Error::new(
            io::ErrorKind::Other,
            "tasa máxima de errores superada — rescate detenido con el mapfile guardado (sube --max-error-rate u omítelo para seguir insistiendo)",
        )
    };
    // Presupuesto de lectura de la fase directa según --min-read-rate.
    let slow_budget = |n: usize, copied: u64, elapsed: Duration| -> Option<Duration> {
        let rate: u64 = match opts.min_read_rate {
            MinRate::Off => return None,
            MinRate::Bytes(r) => r.max(1),
            MinRate::Auto => {
                let avg = copied / elapsed.as_secs().max(1);
                (avg / 10).max(1024)
            }
        };
        Some(Duration::from_millis(
            ((n as u64 * SECTOR_SIZE as u64 * 1000) / rate).max(1),
        ))
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
            let budget = slow_budget(n, copied_bytes, t0.elapsed());
            match try_block(src, img, bak.as_mut(), pos, n, &mut buf, &mut scratch, budget) {
                Ok(_) => {
                    map.set(pos, n as u64, MapStatus::Good, 0);
                    blocks_ok += 1;
                    copied_bytes += n as u64 * SECTOR_SIZE as u64;
                    if let Some(t) = throttle.as_mut() {
                        if let Some(d) = t.feed(Instant::now(), n as u64 * SECTOR_SIZE as u64) {
                            thread::sleep(d);
                        }
                    }
                }
                Err(TryErr::Slow(dur)) => {
                    slow_deferred += 1;
                    if let Some(bud) = budget {
                        eprintln!(
                            "aviso: bloque de {n} sectores en {pos} leído en {} ms (umbral {} ms): diferido a las fases lentas",
                            dur.as_millis(),
                            bud.as_millis()
                        );
                    }
                }
                Err(TryErr::Read(_)) => {
                    map.set(pos, n as u64, MapStatus::Failed, 0);
                    block_errors += 1;
                    if let Some(m) = errmeter.as_mut() {
                        if m.feed(Instant::now(), n as u64 * SECTOR_SIZE as u64) {
                            map.save(map_path)?;
                            return Err(rate_abort());
                        }
                    }
                    rest(&opts);
                }
                Err(TryErr::Fatal(e)) => {
                    map.save(map_path)?;
                    return Err(e);
                }
            }
            pos += n as u64;
            maybe_save(map, &mut ops, &mut last_save)?;
            emit_progress(map, &mut on_progress, Phase::Direct, pos, blocks_ok, block_errors, copied_bytes, slow_deferred);
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
                match try_block(src, img, bak.as_mut(), sa, sl as usize, &mut buf, &mut scratch, None) {
                    Ok(_) => {
                        map.set(sa, sl, MapStatus::Good, 0);
                        blocks_ok += 1;
                        copied_bytes += sl * SECTOR_SIZE as u64;
                        if let Some(t) = throttle.as_mut() {
                            if let Some(d) = t.feed(Instant::now(), sl * SECTOR_SIZE as u64) {
                                thread::sleep(d);
                            }
                        }
                    }
                    Err(TryErr::Slow(_)) => unreachable!(), // sin presupuesto en las fases lentas
                    Err(TryErr::Read(_)) => {
                        map.set(sa, sl, MapStatus::Failed, 0);
                        block_errors += 1;
                        if let Some(m) = errmeter.as_mut() {
                            if m.feed(Instant::now(), sl * SECTOR_SIZE as u64) {
                                map.save(map_path)?;
                                return Err(rate_abort());
                            }
                        }
                        rest(&opts);
                    }
                    Err(TryErr::Fatal(e)) => {
                        map.save(map_path)?;
                        return Err(e);
                    }
                }
                maybe_save(map, &mut ops, &mut last_save)?;
                emit_progress(map, &mut on_progress, Phase::Split, sa + sl, blocks_ok, block_errors, copied_bytes, slow_deferred);
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
                match try_block(src, img, bak.as_mut(), lba, 1, &mut buf, &mut scratch, None) {
                    Ok(_) => {
                        map.set(lba, 1, MapStatus::Good, attempt - 1);
                        blocks_ok += 1;
                        copied_bytes += SECTOR_SIZE as u64;
                        if let Some(t) = throttle.as_mut() {
                            if let Some(d) = t.feed(Instant::now(), SECTOR_SIZE as u64) {
                                thread::sleep(d);
                            }
                        }
                        recovered = true;
                        break;
                    }
                    Err(TryErr::Slow(_)) => unreachable!(), // sin presupuesto en las fases lentas
                    Err(TryErr::Read(_)) => {
                        map.set(lba, 1, MapStatus::Failed, attempt as u8);
                        block_errors += 1;
                        if let Some(m) = errmeter.as_mut() {
                            if m.feed(Instant::now(), SECTOR_SIZE as u64) {
                                map.save(map_path)?;
                                return Err(rate_abort());
                            }
                        }
                        // pausa entre tentativas del mismo sector (no tras la última)
                        if attempt < opts.retries.max(1) {
                            rest(&opts);
                        }
                    }
                    Err(TryErr::Fatal(e)) => {
                        map.save(map_path)?;
                        return Err(e);
                    }
                }
            }
            if !recovered {
                map.set(lba, 1, MapStatus::Scraped, opts.retries.max(1));
            }
            maybe_save(map, &mut ops, &mut last_save)?;
            emit_progress(map, &mut on_progress, Phase::Scrape, lba, blocks_ok, block_errors, copied_bytes, slow_deferred);
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
                match try_block(src, img, bak.as_mut(), pos_ex, n, &mut buf, &mut scratch, None) {
                    Ok(_) => {
                        map.set(pos_ex, n as u64, MapStatus::Good, 0);
                        blocks_ok += 1;
                        copied_bytes += n as u64 * SECTOR_SIZE as u64;
                        if let Some(t) = throttle.as_mut() {
                            if let Some(d) = t.feed(Instant::now(), n as u64 * SECTOR_SIZE as u64) {
                                thread::sleep(d);
                            }
                        }
                    }
                    Err(TryErr::Slow(_)) => unreachable!(), // sin presupuesto en las fases lentas
                    Err(TryErr::Read(_)) => {
                        block_errors += 1;
                        if let Some(m) = errmeter.as_mut() {
                            if m.feed(Instant::now(), n as u64 * SECTOR_SIZE as u64) {
                                map.save(map_path)?;
                                return Err(rate_abort());
                            }
                        }
                        rest(&opts);
                    }
                    Err(TryErr::Fatal(e)) => {
                        map.save(map_path)?;
                        return Err(e);
                    }
                }
                maybe_save(map, &mut ops, &mut last_save)?;
                emit_progress(map, &mut on_progress, Phase::Reverse, pos_ex, blocks_ok, block_errors, copied_bytes, slow_deferred);
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
        slow_deferred,
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
        slow_ms: HashMap<u64, u64>, // lectura correcta pero lenta
        attempts: RefCell<HashMap<u64, usize>>,
        reads_other: RefCell<u64>, // lecturas de sectores distintos de los marcados
    }

    impl MockRescue {
        fn new(sectors: u64) -> Self {
            Self {
                sectors,
                never_ok: HashSet::new(),
                fail_times: HashMap::new(),
                slow_ms: HashMap::new(),
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
        fn unpoison(&mut self, lba: u64) {
            self.never_ok.remove(&lba);
        }
        fn slow(&mut self, lba: u64, ms: u64) {
            self.slow_ms.insert(lba, ms);
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
            if let Some(ms) = self.slow_ms.get(&lba) {
                thread::sleep(Duration::from_millis(*ms));
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
            backup: None,
            rate_limit: None,
            min_read_rate: MinRate::Off,
            max_error_rate: None,
        }
    }

    #[test]
    fn throttle_duerme_al_superar_el_techo_de_tasa() {
        let mut th = Throttle {
            limit: 1024,
            win_start: None,
            bytes: 0,
        };
        let t = Instant::now();
        assert!(th.feed(t, 1024).is_none(), "justo en el límite no duerme");
        let d = th.feed(t, 512).expect("superado el límite debe pedir sueño");
        assert!(
            d > Duration::ZERO && d <= Duration::from_millis(1000),
            "sueño fuera de la ventana: {d:?}"
        );
    }

    #[test]
    fn zona_lenta_diferida_en_directa_se_recupera_en_las_fases_lentas() {
        let dir = tmpdir("lenta");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 1024u64;
        let mut src = MockRescue::new(sectors);
        src.slow(100, 60); // sector 100 tarda 60 ms en leerse
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        let mut o = opts(sectors, 64, 3, true);
        o.min_read_rate = MinRate::Bytes(3_200_000); // bloque de 64: presupuesto 10 ms
        let rep = run_rescue(&mut src, &mut img, &mut map, &o, &map_path, |_| {}).unwrap();
        assert!(
            rep.slow_deferred >= 1,
            "la fase directa debió diferir el bloque lento"
        );
        assert_eq!(rep.good, 1024, "lo lento se recupera en división");
        assert_eq!(rep.failed + rep.untried, 0);
        let bytes = fs::read(&img_path).unwrap();
        let off = 100 * SECTOR_SIZE;
        assert_eq!(bytes[off], MockRescue::byte_for(100), "el sector lento quedó copiado");
        // Se releyó al menos una vez más que un sector normal (directa diferida + división).
        assert!(*src.attempts.borrow().get(&100).unwrap() >= 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tasa_maxima_de_errores_aborta_con_el_mapa_guardado() {
        let dir = tmpdir("errrate");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 1024u64;
        let mut src = MockRescue::new(sectors);
        for l in 64..192 {
            src.poison(l);
        }
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        let mut o = opts(sectors, 64, 3, true);
        o.max_error_rate = Some(20_000); // el primer bloque fallido (32 KiB) ya lo cruza
        let e = run_rescue(&mut src, &mut img, &mut map, &o, &map_path, |_| {})
            .expect_err("debe abortar al cruzar la tasa");
        assert!(e.to_string().contains("tasa máxima de errores"), "{e}");
        let loaded = MapFile::load(&map_path).expect("el mapfile debe quedar guardado");
        assert_eq!(loaded.stats().0, 64, "lo sano ya copiado queda Good");
        assert_eq!(loaded.stats().1, 64, "el bloque fallido queda Failed");
        assert_eq!(loaded.stats().3, 1024 - 128, "lo no visitado queda pendiente para --resume");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn requeue_reentra_lo_agotado_en_el_resume() {
        let dir = tmpdir("requeue");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 1024u64;
        let mut src = MockRescue::new(sectors);
        src.poison(300);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        let o = opts(sectors, 64, 2, false);
        run_rescue(&mut src, &mut img, &mut map, &o, &map_path, |_| {}).unwrap();
        let loaded = MapFile::load(&map_path).unwrap();
        assert_eq!(loaded.stats().2, 1, "el ilegible quedó agotado (Scraped)");

        // El medio "mejora" (como tras un remap): ese sector ya responde.
        src.unpoison(300);
        let mut map2 = MapFile::load(&map_path).unwrap();
        assert_eq!(map2.requeue_scraped(), 1);
        let rep2 = run_rescue(&mut src, &mut img, &mut map2, &o, &map_path, |_| {}).unwrap();
        assert_eq!(rep2.good, 1024, "reencolado, el sector se recuperó en la fase directa");
        assert_eq!(rep2.scraped, 0);
        let bytes = fs::read(&img_path).unwrap();
        assert_eq!(bytes[300 * SECTOR_SIZE], MockRescue::byte_for(300));
        let _ = fs::remove_dir_all(&dir);
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

    #[test]
    fn respaldo_guarda_el_contenido_previo_de_la_imagen() {
        // Regla dura: cada sector se guarda ANTES de sobrescribirlo, en el
        // offset propio del sidecar disperso.
        let dir = tmpdir("respaldo");
        let sectors = 64u64;
        let img_path = dir.join("im.raw");
        {
            let mut img = prepare_image(&img_path, sectors).unwrap();
            img.write_sector(3, &[0x5Au8; SECTOR_SIZE]).unwrap(); // contenido previo
        }
        let bk_path = dir.join("im.raw.respaldo");
        let mut map = MapFile::new(sectors);
        let mut src = MockRescue::new(sectors);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut o = opts(sectors, 8, 1, false);
        o.backup = Some(bk_path.clone());
        run_rescue(&mut src, &mut img, &mut map, &o, &dir.join("own.map"), |_| {}).unwrap();
        // Lo que había en el sector 3 está en el sidecar...
        let bk = fs::read(&bk_path).unwrap();
        assert_eq!(&bk[3 * SECTOR_SIZE..4 * SECTOR_SIZE], &[0x5Au8; SECTOR_SIZE]);
        // ...y el sector 3 de la imagen tiene ahora el contenido del origen.
        let mut one = [0u8; SECTOR_SIZE];
        FileImage::open_ro(&img_path)
            .unwrap()
            .read_sector(3, &mut one)
            .unwrap();
        assert_eq!(one, [MockRescue::byte_for(3); SECTOR_SIZE]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sector_no_leido_no_se_escribe_ni_se_respalda() {
        // Un sector que no se pudo leer deja hueco honesto en la imagen y no
        // genera respaldo (no se escribió nada).
        let dir = tmpdir("respaldo2");
        let sectors = 32u64;
        let img_path = dir.join("im.raw");
        let bk_path = dir.join("im.raw.respaldo");
        let mut map = MapFile::new(sectors);
        let mut src = MockRescue::new(sectors);
        src.poison(10);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut o = opts(sectors, 4, 1, false);
        o.backup = Some(bk_path.clone());
        let rep = run_rescue(&mut src, &mut img, &mut map, &o, &dir.join("own.map"), |_| {}).unwrap();
        assert_eq!(rep.scraped, 1);
        let bytes = fs::read(&img_path).unwrap();
        assert!(bytes[10 * SECTOR_SIZE..11 * SECTOR_SIZE].iter().all(|&b| b == 0));
        let bk = fs::read(&bk_path).unwrap();
        assert!(bk[10 * SECTOR_SIZE..11 * SECTOR_SIZE].iter().all(|&b| b == 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn respaldo_imposible_aborta_antes_de_tocar_la_imagen() {
        // Si el sidecar no se puede abrir, el rescate ni siquiera empieza.
        let dir = tmpdir("respaldo3");
        let sectors = 8u64;
        let img_path = dir.join("im.raw");
        let mut map = MapFile::new(sectors);
        let mut src = MockRescue::new(sectors);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut o = opts(sectors, 4, 1, false);
        o.backup = Some(dir.clone()); // un directorio no es un archivo
        let err = run_rescue(&mut src, &mut img, &mut map, &o, &dir.join("own.map"), |_| {});
        assert!(err.is_err(), "sin sidecar accesible el rescate debe negarse a empezar");
        let _ = fs::remove_dir_all(&dir);
    }
}
