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

/// Umbral a partir del cual un intento de lectura fallido se considera
/// «laborioso»: el medio tardó en devolver el error, señal de que el cabezal
/// estuvo aguantando el intento. Solo entonces conviene enfriar (`--retry-wait`).
/// Un error inmediato declarado por el firmware no necesita pausa: dormirla tras
/// cada bloque muerto añade horas de sueño puro sin proteger nada.
const REST_STRAINED: Duration = Duration::from_secs(1);

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
    /// Muestra un avance hex+ASCII del último sector leído bien (0 = off).
    pub preview_lines: usize,
    /// Tras escribir un bloque, relectura de contraste en el DESTINO: si el
    /// medio no devuelve lo escrito, el rescate/clonado aborta con el mapfile
    /// guardado (`--verify` en `clone`; el `compare` de ddrescue en su forma
    /// más simple). Coste: duplica las lecturas del destino.
    pub verify_dest: bool,
    /// Salto adaptativo en la fase directa (`--skip-adaptive`): tras una
    /// ristra de bloques fallidos, salta una distancia geométrica en vez de
    /// seguir tropezando; lo saltado queda pendiente para las fases lentas.
    pub skip_adaptive: bool,
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
            preview_lines: 0,
            verify_dest: false,
            skip_adaptive: false,
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
    /// Saltos adaptativos dados por la fase directa (`--skip-adaptive`).
    pub adaptive_leaps: u64,
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
    /// Primer sector del último bloque leído/copiado bien: (lba, 512 bytes).
    pub preview: Option<(u64, Vec<u8>)>,
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

/// `--generate-map`: mapa aproximado a partir de una imagen destino ya
/// existente (el `-G` de ddrescue, para herencias de `dd`): un sector
/// TODO-CERO se interpreta como «aún no rescatado» (queda pendiente y se
/// reintentará desde el origen); cualquier sector con al menos un byte
/// distinto de cero se da por BUENO y no se releerá del origen.
/// Advertencia honesta: si un cero era dato legítimo, se releerá del origen
/// (no se pierde nada); si un no-cero era basura de una imagen ajena, queda
/// preservado como bueno — usa esta opción solo con imágenes propias de un
/// rescate o copia fiable.
pub fn generate_map_from_image(img: &mut FileImage) -> io::Result<MapFile> {
    let total = img.sector_count();
    let mut map = MapFile::new(total);
    const GEN_CHUNK: usize = 1024; // sectores por lectura
    let mut buf = vec![0u8; GEN_CHUNK * SECTOR_SIZE];
    let mut run: Option<(u64, u64)> = None; // tramo no-cero en curso
    let mut pos = 0u64;
    while pos < total {
        let n = GEN_CHUNK.min((total - pos) as usize);
        img.read_chunk(pos, n, &mut buf[..n * SECTOR_SIZE])?;
        for i in 0..n {
            let sec = &buf[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE];
            let nonzero = sec.iter().any(|&b| b != 0);
            let lba = pos + i as u64;
            if nonzero {
                let continua = match &run {
                    Some((a, len)) => lba == *a + *len,
                    None => false,
                };
                if continua {
                    if let Some((_, len)) = &mut run {
                        *len += 1;
                    }
                } else {
                    if let Some((a, len)) = run.take() {
                        map.set(a, len, MapStatus::Good, 0);
                    }
                    run = Some((lba, 1));
                }
            } else if let Some((a, len)) = run.take() {
                map.set(a, len, MapStatus::Good, 0);
            }
        }
        pos += n as u64;
    }
    if let Some((a, len)) = run.take() {
        map.set(a, len, MapStatus::Good, 0);
    }
    Ok(map)
}

/// Fallo de un intento de rescate. `Read` es un fallo normal del origen (el
/// sector se reintentará con la lógica de su fase). `Fatal` es un fallo del
/// respaldo o de la escritura en la imagen: aborta el rescate entero — la
/// imagen jamás se toca sin su respaldo sector a sector delante.
enum TryErr {
    /// Fallo de lectura del origen: normal en el rescate (lo reintenta la
    /// fase correspondiente). Lleva el error para diagnósticos y la duración
    /// real del intento: una lectura que falló al instante (error de medio
    /// declarado por el firmware) no castiga el cabezal y no necesita pausa;
    /// una que tardó en fallar sí (el disco estaba aguantando el intento).
    Read(#[allow(dead_code)] io::Error, Duration),
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

/// Copia el bloque `src → dst` solo después de respaldar en `bak` el contenido
/// que el DESTINO tiene ahora en ese rango. Orden deliberado: primero se LEE
/// el origen (si falla, no hay nada que escribir ni respaldar).
/// `slow_over`: presupuesto de lectura de la fase directa; si la lectura
/// termina bien pero tardó más que el presupuesto, el bloque se declara
/// `Slow` ANTES de respaldar o escribir (el destino no se toca). Devuelve la
/// duración real de la lectura.
/// `verify_dest`: tras escribir, el bloque se relee del destino y se compara
/// con lo escrito — una discrepancia es `Fatal` (el mapfile queda guardado).
fn try_block(
    src: &mut dyn SectorSource,
    dst: &mut dyn SectorSource,
    bak: Option<&mut ImageBackup>,
    lba: u64,
    n: usize,
    buf: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    slow_over: Option<Duration>,
    verify_dest: bool,
) -> Result<Duration, TryErr> {
    buf.resize(n * SECTOR_SIZE, 0);
    let t = Instant::now();
    if let Err(e) = src.read_chunk(lba, n, buf) {
        return Err(TryErr::Read(e, t.elapsed()));
    }
    let dur = t.elapsed();
    if let Some(bud) = slow_over {
        if dur > bud {
            return Err(TryErr::Slow(dur));
        }
    }
    if let Some(b) = bak {
        scratch.resize(n * SECTOR_SIZE, 0);
        dst.read_chunk(lba, n, scratch).map_err(TryErr::Fatal)?;
        b.put_chunk(lba, &scratch[..n * SECTOR_SIZE])
            .map_err(TryErr::Fatal)?;
    }
    dst.write_chunk(lba, n, buf).map_err(TryErr::Fatal)?;
    if verify_dest {
        // Contraste de lo recién escrito (el `compare` de ddrescue en su
        // forma más simple): el medio destino miente ⇒ abortar con mapa.
        scratch.resize(n * SECTOR_SIZE, 0);
        dst.read_chunk(lba, n, scratch).map_err(TryErr::Fatal)?;
        if scratch[..n * SECTOR_SIZE] != buf[..n * SECTOR_SIZE] {
            return Err(TryErr::Fatal(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "el destino no devolvió lo que se le escribió (sectores {lba}..{})",
                    lba + n as u64
                ),
            )));
        }
    }
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
    preview: Option<&(u64, Vec<u8>)>,
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
        preview: preview.cloned(),
    });
}

pub fn run_rescue(
    src: &mut dyn SectorSource,
    dst: &mut dyn SectorSource,
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
    // Salto adaptativo (`--skip-adaptive`): fallos de lectura consecutivos
    // en la fase directa y la distancia de salto correspondiente.
    let mut consec_read_fail = 0u64;
    let mut adaptive_leaps = 0u64;
    let mut ops = 0usize;
    let mut last_save = Instant::now();
    let mut last_temp = Instant::now();
    let mut copied_bytes = 0u64;
    let mut slow_deferred = 0u64;
    let mut last_sample: Option<(u64, Vec<u8>)> = None;

    // El respaldo se abre ANTES de tocar nada: si no se puede crear el
    // sidecar, el rescate ni siquiera empieza.
    let mut bak = match &opts.backup {
        Some(p) => Some(ImageBackup::open(p)?),
        None => None,
    };

    // Descanso tras un fallo laborioso: en un clúster malo el cabezal agradece
    // pausar, pero solo cuando el medio tardó en fallar (ver REST_STRAINED).
    let rest = |opts: &RescueOptions| {
        if opts.retry_wait > Duration::ZERO {
            thread::sleep(opts.retry_wait.min(Duration::from_secs(30)));
        }
    };
    let rest_after = |opts: &RescueOptions, dur: Duration| {
        if dur >= REST_STRAINED {
            rest(opts);
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
            match try_block(src, dst, bak.as_mut(), pos, n, &mut buf, &mut scratch, budget, opts.verify_dest) {
                Ok(_) => {
                    consec_read_fail = 0;
                    map.set(pos, n as u64, MapStatus::Good, 0);
                    blocks_ok += 1;
                    copied_bytes += n as u64 * SECTOR_SIZE as u64;
                    if opts.preview_lines > 0 {
                        last_sample = Some((pos, buf[..SECTOR_SIZE].to_vec()));
                    }
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
                Err(TryErr::Read(_, dur)) => {
                    map.set(pos, n as u64, MapStatus::Failed, 0);
                    block_errors += 1;
                    consec_read_fail += 1;
                    if let Some(m) = errmeter.as_mut() {
                        if m.feed(Instant::now(), n as u64 * SECTOR_SIZE as u64) {
                            map.save(map_path)?;
                            return Err(rate_abort());
                        }
                    }
                    rest_after(&opts, dur);
                }
                Err(TryErr::Fatal(e)) => {
                    map.save(map_path)?;
                    return Err(e);
                }
            }
            pos += n as u64;
            // Salto adaptativo (`--skip-adaptive`, estilo -K): tras una ristra
            // de bloques fallidos la fase directa deja de martillar el mismo
            // clúster — salta una distancia geométrica y lo saltado queda
            // pendiente para las fases lentas, que sí trabajan sector a
            // sector. En un clúster malo grande esto ahorra minutos de
            // tropezar y, sobre todo, castigo adicional al cabezal.
            if opts.skip_adaptive && consec_read_fail >= 4 {
                let stride_blocks = 1u64 << (consec_read_fail - 2).min(12);
                let leap = (stride_blocks as usize).saturating_mul(opts.block.max(1));
                if pos + (leap as u64) < a + len {
                    // Lo saltado se declara explícitamente pendiente ( Failed
                    // cubre también «sin intentar»): así las fases lentas lo
                    // ven dentro de esta misma corrida, no solo al reanudar.
                    map.set(pos, leap as u64, MapStatus::Failed, 0);
                    pos += leap as u64;
                    adaptive_leaps += 1;
                    if leap >= 8 * opts.block.max(1) {
                        eprintln!(
                            "salto adaptativo: {leap} sectores pendientes se dejan para las fases lentas (el cabezal deja de martillar el clúster)"
                        );
                    }
                }
            }
            maybe_save(map, &mut ops, &mut last_save)?;
            emit_progress(map, &mut on_progress, Phase::Direct, pos, blocks_ok, block_errors, copied_bytes, slow_deferred, last_sample.as_ref());
        }
    }
    map.save(map_path)?;

    // -------------------------------------------------- fase 2: división ----
    // Bisección ACOTADA: cada tramo pendiente se parte por la mitad hasta medir
    // un bloque de la fase directa, y cada trozo se intenta UNA sola vez (la
    // cola guarda el árbol de bisección). Antes esta fase se re-derivaba del
    // mapa: en un tramo totalmente ilegible las dos mitades fallidas quedaban
    // `Failed` con los mismos reintentos, el mapa las fusionaba de vuelta al
    // tramo original y la división repetía la misma bisección INFINITAMENTE —
    // martilleo puro: horas tropezando el mismo clúster sin recuperar un sector.
    // Por debajo del tamaño de bloque una lectura bulk ya no ahorra nada frente
    // a intentar los sectores uno a uno: eso es trabajo del rascado.
    let floor = opts.block.max(1) as u64;
    let mut queue: std::collections::VecDeque<(u64, u64)> = map
        .pending_ranges(opts.start, opts.end, &[MapStatus::Failed])
        .into_iter()
        .filter(|(_, len)| *len > floor)
        .collect();
    while let Some((a, len)) = queue.pop_front() {
        let mid = (len / 2).max(1);
        for (sa, sl) in [(a, mid), (a + mid, len - mid)] {
            if sl == 0 {
                continue;
            }
            match try_block(src, dst, bak.as_mut(), sa, sl as usize, &mut buf, &mut scratch, None, opts.verify_dest) {
                Ok(_) => {
                    map.set(sa, sl, MapStatus::Good, 0);
                    blocks_ok += 1;
                    copied_bytes += sl * SECTOR_SIZE as u64;
                    if opts.preview_lines > 0 {
                        last_sample = Some((sa, buf[..SECTOR_SIZE].to_vec()));
                    }
                    if let Some(t) = throttle.as_mut() {
                        if let Some(d) = t.feed(Instant::now(), sl * SECTOR_SIZE as u64) {
                            thread::sleep(d);
                        }
                    }
                }
                Err(TryErr::Slow(_)) => unreachable!(), // sin presupuesto en las fases lentas
                Err(TryErr::Read(_, dur)) => {
                    map.set(sa, sl, MapStatus::Failed, 0);
                    block_errors += 1;
                    // El trozo sigue midiendo más que un bloque: su bisección
                    // queda en cola y se intentará una única vez.
                    if sl > floor {
                        queue.push_back((sa, sl));
                    }
                    if let Some(m) = errmeter.as_mut() {
                        if m.feed(Instant::now(), sl * SECTOR_SIZE as u64) {
                            map.save(map_path)?;
                            return Err(rate_abort());
                        }
                    }
                    rest_after(&opts, dur);
                }
                Err(TryErr::Fatal(e)) => {
                    map.save(map_path)?;
                    return Err(e);
                }
            }
            maybe_save(map, &mut ops, &mut last_save)?;
            emit_progress(map, &mut on_progress, Phase::Split, sa + sl, blocks_ok, block_errors, copied_bytes, slow_deferred, last_sample.as_ref());
        }
    }
    map.save(map_path)?;

    // --------------------------------------------------- fase 3: rascado ----
    // Candidatos: TODOS los sectores pendientes (`Failed`, incluidos los huecos
    // sin mapear) del rango. La división acotada ya buscó los tramos grandes
    // legibles; lo que queda es saber si hay algo recuperable sector a sector.
    // Los `Scraped` (agotados en pasadas anteriores) NO se reintentan aquí: para
    // eso está `--requeue`, que los devuelve explícitamente a pendientes. Cada
    // candidato se trabaja UNA vez por pasada (los que agoten los reintentos
    // quedan `?`). Es la fase cara del rescate: cuesta un intento por sector
    // pendiente, por eso se avisa cuántos son (`--retry` y `--max-error-rate`
    // son los mandos del coste).
    let spans: Vec<(u64, u64)> = map.pending_ranges(opts.start, opts.end, &[MapStatus::Failed]);
    let candidates: u64 = spans.iter().map(|(_, len)| *len).sum();
    if candidates > 0 {
        eprintln!(
            "rascado: {} sectores pendientes se intentan uno a uno ({} tentativa{} cada uno)",
            candidates,
            opts.retries.max(1),
            if opts.retries.max(1) > 1 { "s" } else { "" }
        );
    }
    for (a, len) in spans {
        for lba in a..a + len {
            let mut recovered = false;
            for attempt in 1..=opts.retries.max(1) {
                match try_block(src, dst, bak.as_mut(), lba, 1, &mut buf, &mut scratch, None, opts.verify_dest) {
                    Ok(_) => {
                        map.set(lba, 1, MapStatus::Good, attempt - 1);
                        blocks_ok += 1;
                        copied_bytes += SECTOR_SIZE as u64;
                        if opts.preview_lines > 0 {
                            last_sample = Some((lba, buf[..SECTOR_SIZE].to_vec()));
                        }
                        if let Some(t) = throttle.as_mut() {
                            if let Some(d) = t.feed(Instant::now(), SECTOR_SIZE as u64) {
                                thread::sleep(d);
                            }
                        }
                        recovered = true;
                        break;
                    }
                    Err(TryErr::Slow(_)) => unreachable!(), // sin presupuesto en las fases lentas
                    Err(TryErr::Read(_, dur)) => {
                        map.set(lba, 1, MapStatus::Failed, attempt as u8);
                        block_errors += 1;
                        if let Some(m) = errmeter.as_mut() {
                            if m.feed(Instant::now(), SECTOR_SIZE as u64) {
                                map.save(map_path)?;
                                return Err(rate_abort());
                            }
                        }
                        // Pausa entre tentativas del mismo sector (nunca tras
                        // la última) y solo si el fallo fue laborioso: un error
                        // inmediato del firmware no necesita enfriar el medio, y
                        // dormir tras cada sector muerto multiplicaba el coste
                        // de un dominio grande por el `--retry-wait`.
                        if attempt < opts.retries.max(1) {
                            rest_after(&opts, dur);
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
            emit_progress(map, &mut on_progress, Phase::Scrape, lba, blocks_ok, block_errors, copied_bytes, slow_deferred, last_sample.as_ref());
        }
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
                match try_block(src, dst, bak.as_mut(), pos_ex, n, &mut buf, &mut scratch, None, opts.verify_dest) {
                    Ok(_) => {
                        map.set(pos_ex, n as u64, MapStatus::Good, 0);
                        blocks_ok += 1;
                        copied_bytes += n as u64 * SECTOR_SIZE as u64;
                        if opts.preview_lines > 0 {
                            last_sample = Some((pos_ex, buf[..SECTOR_SIZE].to_vec()));
                        }
                        if let Some(t) = throttle.as_mut() {
                            if let Some(d) = t.feed(Instant::now(), n as u64 * SECTOR_SIZE as u64) {
                                thread::sleep(d);
                            }
                        }
                    }
                    Err(TryErr::Slow(_)) => unreachable!(), // sin presupuesto en las fases lentas
                    Err(TryErr::Read(_, dur)) => {
                        block_errors += 1;
                        if let Some(m) = errmeter.as_mut() {
                            if m.feed(Instant::now(), n as u64 * SECTOR_SIZE as u64) {
                                map.save(map_path)?;
                                return Err(rate_abort());
                            }
                        }
                        rest_after(&opts, dur);
                    }
                    Err(TryErr::Fatal(e)) => {
                        map.save(map_path)?;
                        return Err(e);
                    }
                }
                maybe_save(map, &mut ops, &mut last_save)?;
                emit_progress(map, &mut on_progress, Phase::Reverse, pos_ex, blocks_ok, block_errors, copied_bytes, slow_deferred, last_sample.as_ref());
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
        adaptive_leaps,
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
            preview_lines: 0,
            verify_dest: false,
            skip_adaptive: false,
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
    fn preview_lleva_el_ultimo_sector_leido() {
        let dir = tmpdir("preview");
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let sectors = 128u64;
        let mut src = MockRescue::new(sectors);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut map = MapFile::new(sectors);
        let mut o = opts(sectors, 8, 1, false);
        o.preview_lines = 2;
        let mut mostrados: Vec<Option<(u64, Vec<u8>)>> = Vec::new();
        run_rescue(&mut src, &mut img, &mut map, &o, &map_path, |p| {
            mostrados.push(p.preview.clone())
        })
        .unwrap();
        let last = mostrados.last().expect("progresos").as_ref().expect("muestra");
        assert_eq!(last.0, 120, "último bloque directo comenzaba en 120");
        assert_eq!(last.1.len(), SECTOR_SIZE);
        assert_eq!(last.1[0], MockRescue::byte_for(120));
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

    /// Destino de prueba que acepta escrituras pero al releer devuelve ceros:
    /// modela un medio que «miente» sobre lo que guardó.
    struct Amnesico {
        sectors: u64,
    }
    impl SectorSource for Amnesico {
        fn sector_count(&self) -> u64 {
            self.sectors
        }
        fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
            if lba >= self.sectors {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "fuera de rango"));
            }
            buf.fill(0);
            Ok(())
        }
        fn write_sector(&mut self, _lba: u64, _buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn verify_dest_aborta_si_el_destino_miente() {
        // Con el contraste activado, un destino que no devuelve lo escrito
        // aborta (Fatal) y deja el mapfile guardado para reanudar.
        let dir = tmpdir("vfy1");
        let sectors = 64u64;
        let map_path = dir.join("c.map");
        let mut map = MapFile::new(sectors);
        let mut src = MockRescue::new(sectors);
        let mut dst = Amnesico { sectors };
        let mut o = opts(sectors, 8, 1, false);
        o.verify_dest = true;
        let e = run_rescue(&mut src, &mut dst, &mut map, &o, &map_path, |_| {}).unwrap_err();
        assert!(
            e.to_string().contains("no devolvió lo que se le escribió"),
            "mensaje inesperado: {e}"
        );
        assert!(map_path.exists(), "el mapa debe quedar guardado en el abort");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_dest_con_destino_honesto_completa_igual() {
        // Contraste activado sobre un destino normal: resultado idéntico al
        // rescate sin contraste (solo más lecturas del destino).
        let dir = tmpdir("vfy2");
        let sectors = 256u64;
        let img_path = dir.join("d.raw");
        let map_path = dir.join("d.map");
        let mut map = MapFile::new(sectors);
        let mut src = MockRescue::new(sectors);
        src.poison(70);
        let mut img = prepare_image(&img_path, sectors).unwrap();
        let mut o = opts(sectors, 16, 2, true);
        o.verify_dest = true;
        let rep = run_rescue(&mut src, &mut img, &mut map, &o, &map_path, |_| {}).unwrap();
        assert_eq!(rep.good, sectors - 1);
        assert_eq!(rep.scraped, 1);
        let bytes = fs::read(&img_path).unwrap();
        let off = 71 * SECTOR_SIZE;
        assert_eq!(bytes[off], MockRescue::byte_for(71), "el contraste no altera lo copiado");
        let _ = fs::remove_dir_all(&dir);
    }

    // ----------------------- U8: --generate-map / --skip-adaptive ----------

    /// `--generate-map` (el `-G` de ddrescue): una imagen ya existente se
    /// convierte en mapa aproximado — no-cero = bueno, todo-cero = pendiente.
    #[test]
    fn generate_map_declara_buenos_los_no_cero_y_deja_los_huecos() {
        let dir = tmpdir("gen1");
        let img_path = dir.join("herencia.raw");
        let sectors = 2_100u64;
        let mut img = prepare_image(&img_path, sectors).unwrap();
        // Tramos con contenido: 10..30, 1000..1005, 1020..1028 (cruza la
        // frontera del bloque de lectura de 1024 sectores) y el final 2096..2100.
        for (a, b) in [(10u64, 30u64), (1000, 1005), (1020, 1028), (2096, 2100)] {
            let data = vec![0xA5u8; (b - a) as usize * SECTOR_SIZE];
            img.write_chunk(a, (b - a) as usize, &data).unwrap();
        }
        let mut img = FileImage::open_rw(&img_path).unwrap();
        let map = generate_map_from_image(&mut img).unwrap();
        let (g, f, s, u) = map.stats();
        assert_eq!(g, 20 + 5 + 8 + 4, "solo los sectores con contenido cuentan como buenos");
        assert_eq!(f + s + u, sectors - g, "lo todo-cero queda pendiente");
        // El tramo que cruza la frontera de lectura es UNA sola entrada: los
        // tramos no se parten por donde corta nuestro búfer.
        assert!(
            map.entries()
                .iter()
                .any(|e| e.status == MapStatus::Good && e.start == 1020 && e.len == 8),
            "el tramo cruzado quedó partido: {:?}",
            map.entries()
        );
        // Los huecos son de verdad huecos (sin entrada): los trabaja la fase directa.
        assert!(
            map.entries().iter().all(|e| e.status == MapStatus::Good),
            "generate_map solo escribe entradas buenas"
        );
        assert_eq!(map.pending_ranges(0, sectors, &[MapStatus::Failed]).len(), 4);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Un mapa generado desde la imagen hace que el rescate NO vuelva a leer
    /// del origen lo que la imagen ya tiene, y complete solo los huecos.
    #[test]
    fn generate_map_evita_releer_lo_que_la_imagen_ya_tiene() {
        let dir = tmpdir("gen2");
        let sectors = 1_000u64;
        let src_path = dir.join("origen.raw");
        let img_path = dir.join("destino.raw");
        // Origen: 0..500 = 0x11, 500..1000 = 0x22.
        let mut src_data = vec![0x11u8; 500 * SECTOR_SIZE];
        src_data.extend(vec![0x22u8; 500 * SECTOR_SIZE]);
        fs::write(&src_path, &src_data).unwrap();
        // Imagen heredada: 0..500 = 0xAA (contenido propio), 500..1000 = ceros.
        let mut img_data = vec![0xAAu8; 500 * SECTOR_SIZE];
        img_data.extend(vec![0u8; 500 * SECTOR_SIZE]);
        fs::write(&img_path, &img_data).unwrap();

        let mut src = FileImage::open_ro(&src_path).unwrap();
        let mut img = FileImage::open_rw(&img_path).unwrap();
        let mut map = generate_map_from_image(&mut img).unwrap();
        assert_eq!(map.stats().0, 500);
        let o = opts(sectors, 64, 1, true);
        let rep = run_rescue(&mut src, &mut img, &mut map, &o, &dir.join("g.map"), |_| {}).unwrap();
        assert_eq!(rep.good, sectors);
        let bytes = fs::read(&img_path).unwrap();
        // Los 500 primeros conservan lo que la imagen ya guardaba: NO se
        // releyeron del origen (si se hubieran releído serían 0x11).
        assert!(bytes[..500 * SECTOR_SIZE].iter().all(|&b| b == 0xAA));
        // Los huecos todo-cero sí se rellenaron desde el origen.
        assert!(bytes[500 * SECTOR_SIZE..].iter().all(|&b| b == 0x22));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `--skip-adaptive` (estilo `-K`): ante una ristra de bloques fallidos la
    /// fase directa deja de martillar el clúster y salta. El mapa final y el
    /// contenido de la imagen son IDÉNTICOS a la corrida sin flag (lo saltado
    /// lo recupera — o declara imposible — el rascado): el flag cambia el
    /// coste, nunca el resultado.
    #[test]
    fn skip_adaptive_reduce_el_martilleo_sin_alterar_el_resultado() {
        let sectors = 512u64;
        let bad: std::collections::HashSet<u64> = (128..256).collect(); // 8 bloques de 16

        let run = |adaptive: bool, tag: &str| {
            let dir = tmpdir(tag);
            let img_path = dir.join("d.raw");
            let mut map = MapFile::new(sectors);
            let mut src = MockRescue::new(sectors);
            for l in &bad {
                src.poison(*l);
            }
            let mut img = prepare_image(&img_path, sectors).unwrap();
            let mut o = opts(sectors, 16, 2, false);
            o.skip_adaptive = adaptive;
            let rep = run_rescue(&mut src, &mut img, &mut map, &o, &dir.join("m.map"), |_| {}).unwrap();
            let bytes = fs::read(&img_path).unwrap();
            let reads_other = *src.reads_other.borrow();
            let attempts: usize = src.attempts.borrow().values().sum();
            let _ = fs::remove_dir_all(&dir);
            (rep, bytes, reads_other, attempts)
        };

        let (plain, bytes_plain, reads_plain, attempts_plain) = run(false, "skip0");
        let (adapt, bytes_adapt, reads_adapt, attempts_adapt) = run(true, "skip1");

        assert!(adapt.adaptive_leaps > 0, "sin saltos el flag no hizo nada");
        assert_eq!(plain.adaptive_leaps, 0);
        // Menos bloques tropezados en la fase directa y menos lecturas totales.
        assert!(
            adapt.block_errors < plain.block_errors,
            "esperaba menos intentos fallidos: {} vs {}",
            adapt.block_errors,
            plain.block_errors
        );
        assert!(
            attempts_adapt < attempts_plain,
            "el salto debe ahorrar lecturas: {attempts_adapt} vs {attempts_plain}"
        );
        assert!(reads_adapt >= reads_plain, "los sectores sanos se leen igual o más veces");
        // El resultado final es idéntico: mismos conteos, misma imagen.
        assert_eq!(adapt.good, plain.good);
        assert_eq!(adapt.scraped, plain.scraped);
        assert_eq!(adapt.scraped, bad.len() as u64, "el clúster malo queda rascado");
        assert_eq!(adapt.good, sectors - bad.len() as u64);
        assert_eq!(bytes_adapt, bytes_plain, "la imagen no cambia según el flag");
        for l in &bad {
            let off = (*l as usize) * SECTOR_SIZE;
            assert!(bytes_adapt[off..off + SECTOR_SIZE].iter().all(|&b| b == 0));
        }
    }

    /// Un clúster totalmente ilegible ya no es un pozo sin fondo: la división
    /// desciende acotada (hasta el tamaño de bloque) y el rascado cierra la
    /// pasada intentando los sectores pendientes uno a uno. El rescate TERMINA,
    /// y su coste es acotado (antes, las dos mitades fallidas se fusionaban en
    /// el tramo original y la división repetía la misma bisección indefinidamente).
    #[test]
    fn un_tramo_totalmente_ilegible_termina_sin_martillar_sin_fin() {
        let dir = tmpdir("pozo");
        let sectors = 20_000u64;
        let mut map = MapFile::new(sectors);
        let mut src = MockRescue::new(sectors);
        for l in 4_000u64..14_000u64 {
            src.poison(l);
        }
        let mut img = prepare_image(&dir.join("d.raw"), sectors).unwrap();
        let o = opts(sectors, 128, 1, false);
        let rep = run_rescue(&mut src, &mut img, &mut map, &o, &dir.join("m.map"), |_| {}).unwrap();
        let attempts: u64 = src.attempts.borrow().values().map(|&v| v as u64).sum();
        // Techo generoso: menos de tres lecturas por sector de TODO el dominio
        // cuando la mitad está muerta. Un bucle infinito lo reventaría.
        assert!(
            attempts < 3 * sectors,
            "coste fuera de presupuesto: {attempts} intentos para {sectors} sectores"
        );
        assert_eq!(rep.scraped, 10_000, "el clúster muerto queda rascado (agotado)");
        assert_eq!(rep.good, 10_000);
        assert_eq!(rep.failed, 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
