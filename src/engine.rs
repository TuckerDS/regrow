// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Motor de escaneo/reparación/regeneración de regrow. Sigue el ciclo clásico
//! por sector de las utilidades de regeneración de sectores, con la semántica
//! propia de este programa:
//! - Modos: Scan (solo lectura), Repair (pulsos sobre problemáticos),
//!   Regenerate (rango completo).
//! - **Default no destructivo**: un sector ilegible NO se reescribe con ceros
//!   salvo `allow_data_loss` explícito.
//! - Hooks de ejecución (`RunHooks`): respaldo preventivo (`SectorBackup`),
//!   monitor de salud (`HealthMonitor`, p. ej. temperatura) y progreso
//!   (`progress`) para checkpoints/reanudación.
//! - Prescan: lectura bulk por bloques; los bloques lentos o con error se
//!   aíslan sector a sector. Opta a él Scan/Repair/Refresh (`opts.prescan`).
//! - Retardo: operación > umbral (default 200 ms).
//! - Bandas de latencia: umbrales b1<b2<b3<b4 ms clasifican
//!   las lecturas lentas en T1..T4. En el log: `T`(T1), `U`(T2), `V`(T3) y `X`
//!   para la peor banda T4 (el flag ya existente para "lento"). Con
//!   `bands: None` no hay clasificación:
//!   umbral único `delay` y los retardos solo cuentan (sin flag).
//! - Refresh: relectura + reescritura de los MISMOS bytes solo en sectores
//!   lentos (cualquier banda) o ilegibles (comportamiento `repair`); los sanos
//!   no se tocan, así que no necesita la puerta de desgaste SSD.
//! - Regeneración: read tolerante → read-back (con reintentos) → verificación
//!   (1 lectura + 3 pasadas extra). Éxito ⇒ 'R'; fallo final ⇒ 'B'.
//! - Barra de estado: 50 celdas (span = rango/50); glifos ░ idle, '█' avance,
//!   'T''U''V' bandas T1..T3, 'D' banda T4 (retardo), 'B' malo, 'R' recuperado.
//! - Registros: par S/L en los bordes del rango + corridas B/R agregadas
//!   `<sector>=<FLAG><run>` (gramática de registro compartida).

use std::time::{Duration, Instant};

use crate::backup::SectorBackup;
use crate::log::{Flag, Record};
use crate::source::{SectorSource, SECTOR_SIZE};

pub const DEFAULT_DELAY_MS: u64 = 200; // umbral moderno de retardo (histórico: 500 ms)
pub const PROGRESS_FLUSH_SECS: u64 = 60; // guardado forzado del checkpoint por tiempo
pub const MAX_BATCH: u64 = 0x7F; // lote máx. = reintentos de write_sector
pub const BAD_SECTOR_WINDOW: u64 = 0x5000; // 10 MB cerca de un fallo ⇒ lote 1
pub const EXTRA_VERIFY_PASSES: u32 = 3; // pasadas extrañas post-escritura
pub const GRID_CELLS: usize = 50; // barra de estado CLI
pub const PRESCAN_CHUNK: usize = 128; // sectores por bloque bulk (64 KiB)
pub const POLL_SECTORS: u64 = 8192; // cadencia de poll del monitor de salud
pub const UI_TICK: Duration = Duration::from_millis(100); // refresco del panel vivo

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunMode {
    Repair,
    Scan,
    /// Relee y reescribe los MISMOS bytes solo en sectores lentos (cualquier
    /// banda) o problemáticos; ilegibles con la semántica de `repair`.
    Refresh,
    Regenerate, // rango completo, incluso sectores sanos
}

/// Umbrales de latencia en ms, estrictamente crecientes :
/// clasifican las lecturas lentas en bandas T1..T4. `bands[3]` es el umbral
/// de retardo (T4) y coincide con `EngineOptions.delay`.
pub const DEFAULT_LATENCY_BANDS: [u64; 4] = [5, 20, 50, DEFAULT_DELAY_MS];

#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub mode: RunMode,
    pub start: u32,
    pub end: u32, // exclusivo
    pub delay: Duration,
    /// Bandas de latencia . `Some([b1,b2,b3,b4])` clasifica las
    /// lecturas lentas y escribe en el log los flags `T`(T1), `U`(T2), `V`(T3)
    /// y `X` (T4, la peor). `None` conserva la sin clasificación: umbral único
    /// `delay` y los retardos solo se cuentan (sin flag en el log).
    pub bands: Option<[u64; 4]>,
    /// Escribir ceros sobre sectores ilegibles para forzar la reasignación
    /// del firmware (destructivo). Por defecto **false**: el sector ilegible
    /// se registra como malo y no se toca.
    pub allow_data_loss: bool,
    /// Prescan bulk para Scan/Repair/Refresh (los bloques sanos se saltean
    /// sin lectura sector a sector). No aplica a Regenerate: ahí se escribe todo.
    pub prescan: bool,
}

impl EngineOptions {
    pub fn new(mode: RunMode, start: u32, end: u32) -> Self {
        Self {
            mode,
            start,
            end,
            delay: Duration::from_millis(DEFAULT_DELAY_MS),
            bands: Some(DEFAULT_LATENCY_BANDS),
            allow_data_loss: false,
            prescan: false,
        }
    }

    /// Fija las bandas y mantiene `delay` en el umbral superior (T4).
    pub fn set_bands(&mut self, bands: Option<[u64; 4]>) {
        if let Some(b) = bands {
            self.delay = Duration::from_millis(b[3]);
        }
        self.bands = bands;
    }
}

/// Banda de latencia: 0 = sano; 1..4 = T1..T4 (latencia
/// estrictamente mayor que el umbral). Con `bands: None` sin clasificación:
/// 4 si supera `delay`, 0 en caso contrario.
pub fn latency_band(elapsed: Duration, delay: Duration, bands: Option<[u64; 4]>) -> u8 {
    match bands {
        None => {
            if elapsed > delay {
                4
            } else {
                0
            }
        }
        Some(b) => {
            let mut k = 0u8;
            for (i, &t) in b.iter().enumerate() {
                if elapsed > Duration::from_millis(t) {
                    k = i as u8 + 1;
                }
            }
            k
        }
    }
}

/// Flag de registro para la banda 1..4: T1→`T`, T2→`U`, T3→`V`,
/// T4→`X` (el canónico de "lento", legible también para lectores históricos).
pub fn band_flag(band: u8) -> Flag {
    match band {
        1 => Flag::T,
        2 => Flag::U,
        3 => Flag::V,
        _ => Flag::X,
    }
}

/// Celda de la barra para una banda (T4 conserva el glifo clásico 'D').
fn band_cell(band: u8) -> Cell {
    match band {
        1 => Cell::Band1,
        2 => Cell::Band2,
        3 => Cell::Band3,
        _ => Cell::Delay,
    }
}

/// Acumula el contador de la banda: T4 suma en `delays` (sin clasificación
/// preservada); T1..T3 en `slow1..slow3`.
fn count_slow(stats: &mut EngineStats, band: u8) {
    match band {
        1 => stats.slow1 += 1,
        2 => stats.slow2 += 1,
        3 => stats.slow3 += 1,
        _ => stats.delays += 1,
    }
}

/// Banda a partir la cual un sector es candidato a reescribirse: `Repair` y
/// `Regenerate` solo T4 (semántica clásica de "retardo"); `Refresh` cualquier
/// banda .
fn problem_band(mode: RunMode) -> u8 {
    match mode {
        RunMode::Refresh => 1,
        _ => 4,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct EngineStats {
    pub scanned: u64, // sectores inspeccionados
    pub bad: u64,     // malos persistentes (B)
    pub recovered: u64, // recuperados por reescritura (R)
    pub delays: u64,  // retardos detectados (banda T4; solo Scan; glifo 'D')
    pub slow1: u64,   // sectores en banda T1 (flag `T`)
    pub slow2: u64,   // sectores en banda T2 (flag `U`)
    pub slow3: u64,   // sectores en banda T3 (flag `V`)
    pub regenerated_ok: u64, // escrituras a sectores sanos (Regenerate)
    pub data_lost: u64, // ilegibles reescritos con ceros (allow_data_loss)
    pub backed_up: u64, // contenidos respaldados antes de reescribir
    pub healthy_skipped: u64, // sectores sanos saltados gracias al prescan
}

#[derive(Clone, Debug)]
pub struct EngineReport {
    pub records: Vec<Record>, // incluye el par S/L de bordes (si el pase completó)
    pub stats: EngineStats,
    /// El monitor de salud abortó el pase antes de `end`.
    pub aborted: bool,
    /// Último sector procesado (para checkpoint/reanudación).
    pub last_sector: u32,
}

/// Acción que el monitor de salud impone al motor tras un poll. El poll puede
/// bloquear (p. ej. `TemperatureGuard` duerme hasta que la temperatura baja).
pub enum MonitorAction {
    Continue,
    Abort,
}

#[derive(Clone, Copy, Debug)]
/// Contexto que recibe el monitor en cada poll. (Campos informativos: los
/// monitores actuales no todos los consumen; forman la API del hook.)
#[allow(dead_code)]
pub struct PollInfo {
    pub sector: u32,
    pub stats: EngineStats,
    pub elapsed_ms: u64,
}

pub trait HealthMonitor {
    fn poll(&mut self, info: &PollInfo) -> MonitorAction;
}

/// Hooks opcionales del pase (todos `'a` para tomarlos desde main).
pub struct RunHooks<'a> {
    pub backup: Option<&'a mut dyn SectorBackup>,
    pub monitor: Option<&'a mut dyn HealthMonitor>,
    /// `progress(último_sector_completado, stats_acumuladas)` — usado por el
    /// checkpoint. Se llama cada `POLL_SECTORS` sectores y al terminar.
    pub progress: Option<&'a mut dyn FnMut(u32, &EngineStats)>,
    /// Panel vivo: recibe (barra, contadores, sector actual) en el
    /// tick de UI (cada `UI_TICK` de tiempo, y en cada poll de salud); su
    /// `false` aborta el pase (pausa por ESC). El checkpoint del punto de
    /// pausa lo garantiza la llamada final de `progress`.
    pub ui: Option<&'a mut dyn FnMut(&[Cell; GRID_CELLS], &EngineStats, u32) -> bool>,
}

impl RunHooks<'_> {
    pub fn empty() -> Self {
        Self {
            backup: None,
            monitor: None,
            progress: None,
            ui: None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProblemKind {
    Unreadable,
    /// Lento, con su banda 1..4 .
    Slow(u8),
}

#[derive(Clone, Debug)]
pub struct PrescanReport {
    /// Problemáticos detectados, ordenados por sector.
    pub problems: Vec<(u32, ProblemKind)>,
    pub scanned: u64,
    pub bulk_chunks: u64,    // bloques leídos bulk sin problema
    pub retried_chunks: u64, // bloques aislados sector a sector
    pub aborted: bool,
    pub last_sector: u32,
}

/// Estado por celda de la barra de 50.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cell {
    Empty, // '░'
    Scan,  // '█' avance (CP437 0xDB)
    Bad,   // 'B'
    Rec,   // 'R'
    Band1, // 'T' lento banda T1  
    Band2, // 'U' banda T2
    Band3, // 'V' banda T3
    Delay, // 'D' banda T4 (retardo clásico)
}

impl Cell {
    pub fn glyph(self) -> char {
        match self {
            Cell::Empty => '\u{2591}', // '░' (sombreado claro; en CP437 es 0xB0, no el signo de grado)
            Cell::Scan => '\u{2588}', // '█' avance (bloque lleno, estilo CP437 0xDB)
            Cell::Bad => 'B',
            Cell::Rec => 'R',
            Cell::Band1 => 'T',
            Cell::Band2 => 'U',
            Cell::Band3 => 'V',
            Cell::Delay => 'D',
        }
    }
}

fn cell_idx_for(cur: u32, start: u32, cell_span: u64) -> usize {
    (((cur - start) as u64 / cell_span) as usize).min(GRID_CELLS - 1)
}

// ---------------------------------------------------------------- prescan ----

/// Lectura bulk por bloques de `PRESCAN_CHUNK` sectores. Cada bloque se lee
/// con una sola `read_chunk`; si falla o es lento (supera el retardo, o con
/// bandas activas su media por sector supera el umbral T1), se aísla sector a
/// sector para clasificar Unreadable/Slow(banda). Los bloques sanos no se
/// releen: el pase posterior los saltea (`healthy_skipped`).
pub fn prescan<S: SectorSource + ?Sized>(
    src: &mut S,
    opts: &EngineOptions,
    hooks: &mut RunHooks<'_>,
    mut on_cell: impl FnMut(&[Cell; GRID_CELLS]),
) -> PrescanReport {
    let (start, end) = (opts.start, opts.end);
    let delay = opts.delay;
    assert!(start < end, "rango vacío");
    // Un bloque se aísla si su tiempo total supera el retardo (T4) o si,
    // con bandas activas, su media por sector es peor que el umbral de T1.
    let chunk_is_slow = |elapsed: Duration, n: usize| -> bool {
        if elapsed > delay {
            return true;
        }
        match opts.bands {
            Some(b) => elapsed > Duration::from_millis(b[0].saturating_mul(n as u64)),
            None => false,
        }
    };
    let span = (end - start) as u64;
    let cell_span = (span + GRID_CELLS as u64 - 1) / GRID_CELLS as u64;

    let mut rep = PrescanReport {
        problems: Vec::new(),
        scanned: 0,
        bulk_chunks: 0,
        retried_chunks: 0,
        aborted: false,
        last_sector: start,
    };
    let mut cells = [Cell::Empty; GRID_CELLS];
    let mut chunk_buf = vec![0u8; PRESCAN_CHUNK * SECTOR_SIZE];
    let mut buf = [0u8; SECTOR_SIZE];
    let t_start = Instant::now();
    let mut since_poll: u64 = 0;
    let mut last_ui = Instant::now();
    let mut last_flush = Instant::now();

    let mut cur = start;
    while cur < end {
        // Ctrl+C/kill: abortar al cerrar bloque (no esperar al bloque entero).
        if crate::tui::interrupted() {
            rep.aborted = true;
            rep.last_sector = cur.saturating_sub(1);
            break;
        }
        let n = PRESCAN_CHUNK.min((end - cur) as usize);
        let t0 = Instant::now();
        let bulk_res = src.read_chunk(cur as u64, n, &mut chunk_buf[..n * SECTOR_SIZE]);
        let bulk_slow = chunk_is_slow(t0.elapsed(), n);

        if bulk_res.is_ok() && !bulk_slow {
            rep.bulk_chunks += 1;
            for c in cur..cur + n as u32 {
                paint_scan(&mut cells, cell_idx_for(c, start, cell_span), &mut on_cell);
            }
        } else {
            // Bloque problemático: aislamiento sector a sector con medida.
            rep.retried_chunks += 1;
            for c in cur..cur + n as u32 {
                // Dentro del bloque problemático, abortar POR SECTOR: en medio
                // lento un bloque puede tardar minutos y la comprobación por
                // bloque retrasaría demasiado el Ctrl+C.
                if crate::tui::interrupted() {
                    rep.aborted = true;
                    rep.last_sector = c.saturating_sub(1);
                    break;
                }
                let idx = cell_idx_for(c, start, cell_span);
                let t = Instant::now();
                let read_failed = src.read_sector(c as u64, &mut buf).is_err();
                let band = latency_band(t.elapsed(), delay, opts.bands);
                if read_failed {
                    rep.problems.push((c, ProblemKind::Unreadable));
                    paint(&mut cells, idx, Cell::Bad, &mut on_cell);
                } else if band >= 1 {
                    rep.problems.push((c, ProblemKind::Slow(band)));
                    paint(&mut cells, idx, band_cell(band), &mut on_cell);
                } else {
                    paint_scan(&mut cells, idx, &mut on_cell);
                }
            }
        }

        if rep.aborted {
            break;
        }
        rep.scanned += n as u64;
        rep.last_sector = cur + n as u32 - 1;
        // Panel vivo durante el prescan: refresco por bloque cuando
        // ha pasado UI_TICK; su false aborta igual que el monitor.
        if hooks.ui.is_some() && last_ui.elapsed() >= UI_TICK {
            last_ui = Instant::now();
            let est = EngineStats {
                scanned: rep.scanned,
               ..Default::default()
            };
            if !hooks.ui.as_mut().unwrap()(&cells, &est, rep.last_sector) {
                rep.aborted = true;
                break;
            }
        }
        since_poll += n as u64;
        // Monitor + checkpoint también POR TIEMPO: un clúster malo puede hacer
        // que un poll de 8192 sectores tarde horas; ni la temperatura vigilada
        // ni el progreso guardado pueden envejecer tanto.
        if since_poll >= POLL_SECTORS || last_flush.elapsed() >= Duration::from_secs(PROGRESS_FLUSH_SECS)
        {
            since_poll = 0;
            last_flush = Instant::now();
            let stats = EngineStats {
                scanned: rep.scanned,
               ..Default::default()
            };
            if let Some(f) = hooks.progress.as_mut() {
                // Checkpoint vivo durante el prescan: si se interrumpe el
                // proceso (corte de luz, kill duro), el reanudado no rehace
                // todo el barrido bulk ya hecho.
                f(rep.last_sector, &stats);
            }
            if let Some(m) = hooks.monitor.as_mut() {
                if matches!(
                    m.poll(&PollInfo {
                        sector: rep.last_sector,
                        stats,
                        elapsed_ms: t_start.elapsed().as_millis() as u64,
                    }),
                    MonitorAction::Abort
                ) {
                    rep.aborted = true;
                    break;
                }
            }
        }
        cur += n as u32;
    }
    rep
}

// ------------------------------------------------------------------ run ------

/// Ejecuta el ciclo por sector sobre `src` en [start, end) aplicando `hooks`.
/// `on_cell` recibe la barra completa tras cada pincelada (UI/CLI).
pub fn run<S: SectorSource + ?Sized>(
    src: &mut S,
    opts: &EngineOptions,
    mut hooks: RunHooks<'_>,
    mut on_cell: impl FnMut(&[Cell; GRID_CELLS]),
) -> EngineReport {
    assert!(opts.start < opts.end, "rango vacío");
    let span = (opts.end - opts.start) as u64;
    let cell_span = (span + GRID_CELLS as u64 - 1) / GRID_CELLS as u64;

    let mut cells = [Cell::Empty; GRID_CELLS];
    let mut stats = EngineStats::default();
    let mut records: Vec<Record> = Vec::new();
    let mut runs = RunBuilder::default();
    let mut buf = [0u8; SECTOR_SIZE];
    let mut last_bad: i64 = -1; // sector del último fallo (ventana de lote)
    let mut aborted = false;
    let mut last_done = opts.start;
    let mut since_poll: u64 = 0;
    let mut last_ui = Instant::now();
    let mut last_flush = Instant::now();

    // Par S/L de bordes: 0=S1 … fin=L1 (L se omite si el pase se aborta).
    records.push(Record {
        sector: opts.start,
        flag: Flag::S,
        run: 1,
    });

    // Prescan opcional (Scan/Repair/Refresh): lista corta de problemáticos.
    // `prescan` solo usa `hooks.monitor` (no respalda ni reporta progreso),
    // así que se le presta el propio `hooks` y el préstamo termina aquí.
    let pre = if opts.prescan && opts.mode != RunMode::Regenerate {
        Some(prescan(src, opts, &mut hooks, &mut on_cell))
    } else {
        None
    };

    if let Some(pre) = pre {
        stats.scanned = pre.scanned;
        stats.healthy_skipped = pre.scanned - pre.problems.len() as u64;
        last_done = pre.last_sector;
        if pre.aborted {
            aborted = true;
        }
        // Ruta corta: solo los problemáticos pasan por el ciclo.
        for (cur_u32, kind) in &pre.problems {
            if crate::tui::interrupted() {
                aborted = true;
                break;
            }
            let cur_u32 = *cur_u32;
            let cell_idx = cell_idx_for(cur_u32, opts.start, cell_span);
            match opts.mode {
                RunMode::Scan => {
                    if matches!(kind, ProblemKind::Unreadable) {
                        stats.bad += 1;
                        runs.push(cur_u32, Flag::B);
                        paint(&mut cells, cell_idx, Cell::Bad, &mut on_cell);
                    } else {
                        // Lento con banda: contador por banda y, con
                        // bandas activas, flag T/U/V/X en el log.
                        let band = match kind {
                            ProblemKind::Slow(b) => *b,
                            _ => 4,
                        };
                        count_slow(&mut stats, band);
                        if opts.bands.is_some() {
                            runs.push(cur_u32, band_flag(band));
                        }
                        paint(&mut cells, cell_idx, band_cell(band), &mut on_cell);
                    }
                }
                RunMode::Repair | RunMode::Refresh => {
                    let read_failed = matches!(kind, ProblemKind::Unreadable);
                    let band = match kind {
                        ProblemKind::Slow(b) => *b,
                        _ => 4,
                    };
                    if !read_failed && band < problem_band(opts.mode) {
                        // Bands T1..T3 en Repair: lentos pero no objetivo de
                        // reescritura (Refresh sí los refresca): solo se pintan.
                        paint(&mut cells, cell_idx, band_cell(band), &mut on_cell);
                        last_done = cur_u32;
                        continue;
                    }
                    if read_failed && !opts.allow_data_loss {
                        // Default seguro: no se escribe sobre un ilegible sin permiso.
                        stats.bad += 1;
                        runs.push(cur_u32, Flag::B);
                        last_bad = cur_u32 as i64;
                        paint(&mut cells, cell_idx, Cell::Bad, &mut on_cell);
                        last_done = cur_u32;
                        continue;
                    }
                    if read_failed {
                        // allow_data_loss: write-back con ceros (destructivo).
                        stats.data_lost += 1;
                        buf = [0u8; SECTOR_SIZE];
                    } else if src.read_sector(cur_u32 as u64, &mut buf).is_err() {
                        // Se volvió ilegible entre el prescan y el ciclo.
                        stats.bad += 1;
                        runs.push(cur_u32, Flag::B);
                        last_bad = cur_u32 as i64;
                        paint(&mut cells, cell_idx, Cell::Bad, &mut on_cell);
                        last_done = cur_u32;
                        continue;
                    }
                    // Respaldo preventivo ANTES de reescribir.
                    if !backup_or_skip(
                        &mut hooks,
                        &mut buf,
                        cur_u32,
                        cell_idx,
                        &mut cells,
                        &mut runs,
                        &mut last_bad,
                        &mut stats,
                        &mut on_cell,
                        true,
                    ) {
                        last_done = cur_u32;
                        continue;
                    }
                    if !write_verify(src, opts, &mut buf, cur_u32, cell_idx, &mut cells, &mut runs, &mut last_bad, &mut stats, &mut on_cell, true) {
                        last_done = cur_u32;
                        continue;
                    }
                }
                RunMode::Regenerate => unreachable!(),
            }
            last_done = cur_u32;
        }
        if !aborted {
            // Las celdas sanas ya se pintaron '█' durante el prescan;
            // completar por si el rango quedó a medias.
            for idx in 0..GRID_CELLS {
                if cells[idx] == Cell::Empty {
                    cells[idx] = Cell::Scan;
                }
            }
            on_cell(&cells);
        }
    } else {
        // Ruta sector a sector (sin prescan).
        for cur_u32 in opts.start..opts.end {
            if crate::tui::interrupted() {
                aborted = true;
                break;
            }
            let cur = cur_u32 as u64;
            let cell_idx = cell_idx_for(cur_u32, opts.start, cell_span);
            stats.scanned += 1;

            // 1) Lectura tolerante con medida de tiempo (banda incluida).
            let t0 = Instant::now();
            let read_res = src.read_sector(cur, &mut buf);
            let band = latency_band(t0.elapsed(), opts.delay, opts.bands);
            let read_failed = read_res.is_err();
            if read_failed {
                buf = [0u8; SECTOR_SIZE];
            }

            match opts.mode {
                RunMode::Scan => {
                    // Scan NO escribe: solo detecta.
                    if read_failed {
                        stats.bad += 1;
                        runs.push(cur_u32, Flag::B);
                        last_bad = cur as i64;
                        paint(&mut cells, cell_idx, Cell::Bad, &mut on_cell);
                    } else if band >= 1 {
                        // Lento: contador por banda y barra; con bandas activas
                        // escribe el flag T/U/V/X (sin bandas el
                        // retardo solo cuenta, sin flag).
                        count_slow(&mut stats, band);
                        if opts.bands.is_some() {
                            runs.push(cur_u32, band_flag(band));
                        }
                        paint(&mut cells, cell_idx, band_cell(band), &mut on_cell);
                    } else {
                        paint_scan(&mut cells, cell_idx, &mut on_cell);
                    }
                }
                RunMode::Repair | RunMode::Refresh | RunMode::Regenerate => {
                    // Candidato a reescribir: problemático, o todo el rango
                    // en Regenerate. Sanos en Repair/Refresh: se omiten (sin
                    // registro). Refresh considera problemática cualquier banda;
                    // Repair solo T4 (retardo clásico).
                    let problematic = read_failed || band >= problem_band(opts.mode);
                    let must_write = opts.mode == RunMode::Regenerate || problematic;
                    if must_write {
                        if read_failed && !opts.allow_data_loss {
                            // Default seguro: un ilegible no tiene contenido
                            // legible y no se escribe sobre él sin permiso.
                            stats.bad += 1;
                            runs.push(cur_u32, Flag::B);
                            last_bad = cur as i64;
                            paint(&mut cells, cell_idx, Cell::Bad, &mut on_cell);
                            poll_and_progress(
                                &mut hooks,
                                &mut since_poll,
                                &mut last_done,
                                cur_u32,
                                &mut stats,
                                &mut aborted,
                                &cells,
                                &mut last_ui,
                                &mut last_flush,
                            );
                            if aborted {
                                break;
                            }
                            continue;
                        }
                        if read_failed {
                            // allow_data_loss: ceros para forzar reasignación.
                            stats.data_lost += 1;
                        }
                        // Respaldo preventivo (solo problemáticos legibles).
                        if !backup_or_skip(
                            &mut hooks,
                            &mut buf,
                            cur_u32,
                            cell_idx,
                            &mut cells,
                            &mut runs,
                            &mut last_bad,
                            &mut stats,
                            &mut on_cell,
                            problematic,
                        ) {
                            poll_and_progress(
                                &mut hooks,
                                &mut since_poll,
                                &mut last_done,
                                cur_u32,
                                &mut stats,
                                &mut aborted,
                                &cells,
                                &mut last_ui,
                                &mut last_flush,
                            );
                            if aborted {
                                break;
                            }
                            continue;
                        }
                        if !write_verify(
                            src,
                            opts,
                            &mut buf,
                            cur_u32,
                            cell_idx,
                            &mut cells,
                            &mut runs,
                            &mut last_bad,
                            &mut stats,
                            &mut on_cell,
                            problematic,
                        ) {
                            // Sector malo: registrado; el progreso se reporta al
                            // final del sector más abajo.
                        }
                    } else {
                        // No es objetivo de reescritura: si es lento se pinta
                        // su banda en la barra (coherente con la ruta prescan).
                        if band >= 1 {
                            paint(&mut cells, cell_idx, band_cell(band), &mut on_cell);
                        } else {
                            paint_scan(&mut cells, cell_idx, &mut on_cell);
                        }
                    }
                }
            }
            poll_and_progress(
                &mut hooks,
                &mut since_poll,
                &mut last_done,
                cur_u32,
                &mut stats,
                &mut aborted,
                &cells,
                &mut last_ui,
                &mut last_flush,
            );
            if aborted {
                break;
            }
        }
    }

    records.extend(runs.finish());
    if !aborted {
        records.push(Record {
            sector: opts.end - 1,
            flag: Flag::L,
            run: 1,
        });
    }
    records.sort_by_key(|r| (r.sector, r.flag.char()));

    if let Some(f) = hooks.progress.as_mut() {
        f(last_done, &stats);
    }

    EngineReport {
        records,
        stats,
        aborted,
        last_sector: last_done,
    }
}

/// Respalda el contenido legible ANTES de reescribir. Devuelve false (y
/// marca B) si el respaldo falla: sin respaldo no se escribe.
/// `problematic` controla si el respaldo aplica: solo los problemáticos se
/// respaldan (los sanos no, para no duplicar el disco entero en Regenerate).
fn backup_or_skip(
    hooks: &mut RunHooks<'_>,
    buf: &mut [u8; SECTOR_SIZE],
    cur_u32: u32,
    cell_idx: usize,
    cells: &mut [Cell; GRID_CELLS],
    runs: &mut RunBuilder,
    last_bad: &mut i64,
    stats: &mut EngineStats,
    on_cell: &mut impl FnMut(&[Cell; GRID_CELLS]),
    problematic: bool,
) -> bool {
    if problematic {
        if let Some(bk) = hooks.backup.as_mut() {
            if bk.put(cur_u32 as u64, buf).is_err() {
                stats.bad += 1;
                runs.push(cur_u32, Flag::B);
                *last_bad = cur_u32 as i64;
                paint(cells, cell_idx, Cell::Bad, on_cell);
                return false;
            }
            stats.backed_up += 1;
        }
    }
    true
}

/// Lotes de escritura + verificación (1 lectura + 3 pasadas extra).
/// Devuelve false si el sector quedó malo (B registrado).
fn write_verify<S: SectorSource + ?Sized>(
    src: &mut S,
    opts: &EngineOptions,
    buf: &mut [u8; SECTOR_SIZE],
    cur_u32: u32,
    cell_idx: usize,
    cells: &mut [Cell; GRID_CELLS],
    runs: &mut RunBuilder,
    last_bad: &mut i64,
    stats: &mut EngineStats,
    on_cell: &mut impl FnMut(&[Cell; GRID_CELLS]),
    problematic: bool,
) -> bool {
    // 2) Lote = reintentos de escritura.
    let cur = cur_u32 as u64;
    let mut batch = MAX_BATCH.min(opts.end as u64 - cur);
    if *last_bad >= 0 && (cur - *last_bad as u64) < BAD_SECTOR_WINDOW {
        batch = 1;
    }
    if opts.mode == RunMode::Regenerate {
        batch = 1;
    }

    let mut write_ok = false;
    for _ in 0..batch.max(1) {
        if src.write_sector(cur, buf).is_ok() {
            write_ok = true;
            break;
        }
    }
    if !problematic {
        stats.regenerated_ok += 1;
    }

    // 3) Verificación: 1 lectura + 3 pasadas extra (la última solo exige
    // legibilidad, no rapidez).
    let mut verified = false;
    if write_ok {
        let mut vbuf = [0u8; SECTOR_SIZE];
        for pass in 0..=EXTRA_VERIFY_PASSES {
            let t = Instant::now();
            let ok = src.read_sector(cur, &mut vbuf).is_ok();
            if !ok {
                verified = false;
                break;
            }
            verified = pass == EXTRA_VERIFY_PASSES || t.elapsed() <= opts.delay;
        }
    }

    // 4) Veredicto + barra.
    if verified {
        if problematic {
            stats.recovered += 1;
            runs.push(cur_u32, Flag::R);
            paint(cells, cell_idx, Cell::Rec, on_cell);
        }
        // Sanos regenerados (Regenerate): solo contador, SIN registro.
        true
    } else {
        stats.bad += 1;
        runs.push(cur_u32, Flag::B);
        *last_bad = cur as i64;
        paint(cells, cell_idx, Cell::Bad, on_cell);
        false
    }
}

/// Poll del monitor + reporte de progreso cada `POLL_SECTORS` sectores.
/// Marca `last_done` en cada llamada (checkpoint correcto si se aborta).
/// El hook `ui` (panel vivo) se sondea por TIEMPO (cada `UI_TICK`),
/// no por sectores: su `false` aborta el pase (ESC).
fn poll_and_progress(
    hooks: &mut RunHooks<'_>,
    since_poll: &mut u64,
    last_done: &mut u32,
    cur_u32: u32,
    stats: &mut EngineStats,
    aborted: &mut bool,
    cells: &[Cell; GRID_CELLS],
    last_ui: &mut Instant,
    last_flush: &mut Instant,
) {
    *last_done = cur_u32;
    *since_poll += 1;
    // Cadencia NORMAL por sectores; pero en medio lento un poll entero (8192
    // sectores) puede tardar horas en un clúster malo. El progreso SE GUARDA
    // cada PROGRESS_FLUSH_SECS pase lo que pase (cortes de luz, reinicios):
    // el checkpoint nunca envejece más de un minuto.
    if *since_poll < POLL_SECTORS && last_flush.elapsed() < Duration::from_secs(PROGRESS_FLUSH_SECS)
    {
        if hooks.ui.is_some() && last_ui.elapsed() >= UI_TICK {
            *last_ui = Instant::now();
            if !hooks.ui.as_mut().unwrap()(cells, stats, cur_u32) {
                *aborted = true;
            }
        }
        return;
    }
    *since_poll = 0;
    *last_flush = Instant::now();
    if let Some(m) = hooks.monitor.as_mut() {
        if matches!(
            m.poll(&PollInfo {
                sector: cur_u32,
                stats: *stats,
                elapsed_ms: 0,
            }),
            MonitorAction::Abort
        ) {
            *aborted = true;
            return;
        }
    }
    if let Some(f) = hooks.ui.as_mut() {
        *last_ui = Instant::now();
        if !f(cells, stats, cur_u32) {
            *aborted = true;
            return;
        }
    }
    if let Some(f) = hooks.progress.as_mut() {
        f(cur_u32, stats);
    }
}

/// Acumulador de corridas (gramática run-length).
#[derive(Debug, Default)]
pub struct RunBuilder {
    flag: Option<Flag>,
    start: u32,
    len: u32,
    out: Vec<Record>,
}

impl RunBuilder {
    pub fn push(&mut self, sector: u32, flag: Flag) {
        if self.flag == Some(flag) && self.start + self.len == sector {
            self.len += 1;
            return;
        }
        self.flush();
        self.flag = Some(flag);
        self.start = sector;
        self.len = 1;
    }

    fn flush(&mut self) {
        if let Some(flag) = self.flag.take() {
            self.out.push(Record {
                sector: self.start,
                flag,
                run: self.len,
            });
        }
        self.len = 0;
    }

    pub fn finish(mut self) -> Vec<Record> {
        self.flush();
        self.out
    }}

fn paint(
    cells: &mut [Cell; GRID_CELLS],
    idx: usize,
    glyph: Cell,
    on_cell: &mut impl FnMut(&[Cell; GRID_CELLS]),
) {
    cells[idx] = glyph;
    on_cell(cells);
}

fn paint_scan(
    cells: &mut [Cell; GRID_CELLS],
    idx: usize,
    on_cell: &mut impl FnMut(&[Cell; GRID_CELLS]),
) {
    if cells[idx] == Cell::Empty {
        cells[idx] = Cell::Scan; // '█' solo si la celda aún está '░'
        on_cell(cells);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io;

    /// Fuente de prueba: sectores "malos" (lectura falla hasta escribir),
    /// sectores "lentos" (sueño fijo o por banda) y contadores de escrituras.
    struct MockSource {
        sectors: u64,
        broken: HashSet<u64>,   // fallan SIEMPRE al leer (irreparables)
        degraded: HashSet<u64>, // fallan al leer hasta ser reescritos
        slow: HashSet<u64>,     // lectura OK pero > umbral (sueño fijo 15 ms)
        slow_ms: std::collections::HashMap<u64, u64>, // lectura OK con sueño exacto (bandas de latencia)
        written: HashSet<u64>,
        writes: Vec<u64>,   // escrituras EFFECTIVAS
        attempts: Vec<u64>, // intentos de escritura (incl. fallidos)
        written_bytes: Vec<(u64, u8)>, // contenido recibido en cada escritura efectiva
        noop: Option<u64>,  // hacer write falla en este sector (fallo de medio)
    }

    impl MockSource {
        fn new(sectors: u64) -> Self {
            Self {
                sectors,
                broken: HashSet::new(),
                degraded: HashSet::new(),
                slow: HashSet::new(),
                slow_ms: std::collections::HashMap::new(),
                written: HashSet::new(),
                writes: Vec::new(),
                attempts: Vec::new(),
                written_bytes: Vec::new(),
                noop: None,
            }
        }
    }

    impl SectorSource for MockSource {
        fn sector_count(&self) -> u64 {
            self.sectors
        }

        fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
            if lba >= self.sectors {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "fuera de rango"));
            }
            if let Some(&ms) = self.slow_ms.get(&lba) {
                std::thread::sleep(Duration::from_millis(ms));
            } else if self.slow.contains(&lba) {
                std::thread::sleep(Duration::from_millis(15));
            }
            if self.broken.contains(&lba)
                || (self.degraded.contains(&lba) && !self.written.contains(&lba))
            {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "error de lectura simulado",
                ));
            }
            buf[0] = lba as u8;
            Ok(())
        }

        fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
            self.attempts.push(lba);
            if Some(lba) == self.noop {
                return Err(io::Error::new(io::ErrorKind::Other, "write prohibido"));
            }
            self.writes.push(lba);
            self.written.insert(lba);
            self.written_bytes.push((lba, buf[0]));
            Ok(())
        }
    }

    fn fast_opts(mode: RunMode, start: u32, end: u32) -> EngineOptions {
        let mut o = EngineOptions::new(mode, start, end);
        o.delay = Duration::from_millis(5); // umbral inyectable (mock duerme 15 ms)
        o.bands = None; // sin clasificación: umbral único, retardos sin flag
        o
    }

    /// Opciones con bandas de latencia activas (delay = umbral superior T4).
    fn banded_opts(mode: RunMode, start: u32, end: u32, bands: [u64; 4]) -> EngineOptions {
        let mut o = EngineOptions::new(mode, start, end);
        o.set_bands(Some(bands));
        o
    }

    fn collect() -> impl FnMut(&[Cell; GRID_CELLS]) {
        |_| {}
    }

    fn hooks_empty() -> RunHooks<'static> {
        RunHooks {
            backup: None,
            monitor: None,
            progress: None,
            ui: None,
        }
    }

    #[test]
    fn scan_detecta_malos_y_delays_sin_escribir() {
        let mut src = MockSource::new(100);
        src.broken.insert(40);
        src.broken.insert(41);
        src.slow.insert(60);
        let rep = run(&mut src, &fast_opts(RunMode::Scan, 0, 100), hooks_empty(), collect());
        assert_eq!(src.writes.len(), 0, "Scan no escribe");
        assert_eq!(rep.stats.bad, 2);
        assert_eq!(rep.stats.delays, 1);
        assert!(!rep.aborted);
        assert_eq!(rep.last_sector, 99);
        // S/L + corrida B de longitud 2.
        let flags: Vec<_> = rep
           .records
           .iter()
           .map(|r| (r.sector, r.flag.char(), r.run))
           .collect();
        assert_eq!(flags, vec![(0, 'S', 1), (40, 'B', 2), (99, 'L', 1)]);
    }

    #[test]
    fn default_no_toca_ilegibles_sin_escribir_ceros() {
        let mut src = MockSource::new(100);
        src.degraded.insert(10);
        src.broken.insert(50);
        let rep = run(&mut src, &fast_opts(RunMode::Repair, 0, 100), hooks_empty(), collect());
        // Sin allow_data_loss: ningún ilegible se reescribe con ceros.
        assert_eq!(src.writes.len(), 0);
        assert_eq!(rep.stats.recovered, 0);
        assert_eq!(rep.stats.bad, 2);
        assert_eq!(rep.stats.data_lost, 0);
        let flags: Vec<_> = rep
           .records
           .iter()
           .map(|r| (r.sector, r.flag.char(), r.run))
           .collect();
        assert_eq!(flags, vec![(0, 'S', 1), (10, 'B', 1), (50, 'B', 1), (99, 'L', 1)]);
    }

    #[test]
    fn allow_data_loss_recupera_degraded_y_deja_broken() {
        let mut src = MockSource::new(100);
        src.degraded.insert(10);
        src.broken.insert(50);
        let mut opts = fast_opts(RunMode::Repair, 0, 100);
        opts.allow_data_loss = true;
        let rep = run(&mut src, &opts, hooks_empty(), collect());
        assert_eq!(rep.stats.recovered, 1);
        assert_eq!(rep.stats.bad, 1);
        // Ambos (degraded y broken) fallaron la lectura inicial ⇒ el
        // write-back no pudo preservar los datos originales (ceros).
        assert_eq!(rep.stats.data_lost, 2);
    }

    #[test]
    fn repair_no_toca_sectores_sanos() {
        let mut src = MockSource::new(50);
        let rep = run(&mut src, &fast_opts(RunMode::Repair, 0, 50), hooks_empty(), collect());
        assert!(src.writes.is_empty());
        assert_eq!(rep.stats.bad, 0);
        assert_eq!(rep.stats.recovered, 0);
    }

    #[test]
    fn regenerate_reescribe_todo_el_rango() {
        let mut src = MockSource::new(40);
        let rep = run(&mut src, &fast_opts(RunMode::Regenerate, 0, 40), hooks_empty(), collect());
        assert_eq!(src.writes.len(), 40);
        assert_eq!(rep.stats.regenerated_ok, 40);
        // Los sanos regenerados no generan registros (como el formato histórico).
        let flags: Vec<char> = rep.records.iter().map(|r| r.flag.char()).collect();
        assert_eq!(flags, vec!['S', 'L']);
    }

    #[test]
    fn write_fallido_marca_bad_y_cerca_lote_1() {
        let mut src = MockSource::new(30);
        src.degraded.insert(5);
        src.noop = Some(5); // medio no permite escribir ese sector
        let mut opts = fast_opts(RunMode::Repair, 0, 30);
        opts.allow_data_loss = true; // ilegible: se intenta el write-back
        let rep = run(&mut src, &opts, hooks_empty(), collect());
        assert_eq!(rep.stats.bad, 1);
        // batch = end-cur = 25 reintentos fallidos sobre el sector 5.
        assert_eq!(src.attempts.iter().filter(|&&s| s == 5).count(), 25);
        assert_eq!(src.writes.iter().filter(|&&s| s == 5).count(), 0);
    }

    /// Respalda en memoria lo que el motor le entrega.
    struct MockBackup {
        puts: Vec<(u64, u8)>, // (lba, primer byte del contenido respaldado)
        fail: bool,
    }

    impl crate::backup::SectorBackup for MockBackup {
        fn put(&mut self, lba: u64, data: &[u8; SECTOR_SIZE]) -> std::io::Result<()> {
            if self.fail {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "backup lleno"));
            }
            self.puts.push((lba, data[0]));
            Ok(())
        }
    }

    fn hooks_with(bk: &mut dyn crate::backup::SectorBackup) -> RunHooks<'_> {
        RunHooks {
            backup: Some(bk),
            monitor: None,
            progress: None,
            ui: None,
        }
    }

    #[test]
    fn backup_respalda_antes_de_reescribir_sector_lento() {
        let mut src = MockSource::new(20);
        src.slow.insert(7); // legible (contenido = lba) pero lento ⇒ problemático
        let opts = fast_opts(RunMode::Repair, 0, 20);
        let mut bk = MockBackup { puts: Vec::new(), fail: false };
        {
            let rep = run(&mut src, &opts, hooks_with(&mut bk), collect());
            // El write-back del sector lento culmina en la pasada final de
            // verificación (solo comprueba legibilidad) ⇒ recuperado 'R'.
            assert_eq!(rep.stats.recovered, 1);
            assert_eq!(rep.stats.bad, 0);
            assert_eq!(rep.stats.backed_up, 1);
        }
        assert_eq!(src.writes, vec![7]);
        assert_eq!(bk.puts, vec![(7, 7)]); // contenido original, ANTES del write
    }

    #[test]
    fn backup_fallido_impide_la_escritura() {
        let mut src = MockSource::new(20);
        src.slow.insert(7);
        let opts = fast_opts(RunMode::Repair, 0, 20);
        let mut bk = MockBackup { puts: Vec::new(), fail: true };
        {
            let rep = run(&mut src, &opts, hooks_with(&mut bk), collect());
            assert_eq!(rep.stats.recovered, 0);
            assert_eq!(rep.stats.bad, 1);
        }
        assert!(src.writes.is_empty(), "sin respaldo no se escribe");
    }

    #[test]
    fn regenerate_no_respalda_sectores_sanos() {
        let mut src = MockSource::new(10);
        let opts = fast_opts(RunMode::Regenerate, 0, 10);
        let mut bk = MockBackup { puts: Vec::new(), fail: false };
        let rep = run(&mut src, &opts, hooks_with(&mut bk), collect());
        assert_eq!(rep.stats.backed_up, 0); // solo los problemáticos se respaldan
        assert_eq!(src.writes.len(), 10);
    }

    // ------------------------------------------------------------- prescan ----

    #[test]
    fn prescan_detecta_y_aisla() {
        let mut src = MockSource::new(300);
        src.broken.insert(150); // en el segundo bloque bulk
        src.slow.insert(260); // en el tercer bloque bulk
        let opts = fast_opts(RunMode::Scan, 0, 300);
        let rep = prescan(&mut src, &opts, &mut hooks_empty(), collect());
        // Bloques: 0-127 OK; 128-255 aislado (contiene el broken); 256-299
        // aislado (contiene el slow ⇒ bulk lento).
        assert_eq!(rep.bulk_chunks, 1);
        assert_eq!(rep.retried_chunks, 2);
        assert_eq!(
            rep.problems,
            vec![(150, ProblemKind::Unreadable), (260, ProblemKind::Slow(4))]
        );
        assert_eq!(rep.scanned, 300);
        assert!(!rep.aborted);
    }

    #[test]
    fn run_con_prescan_salta_sanos_y_repara_problematicos() {
        let mut src = MockSource::new(300);
        src.degraded.insert(150); // ilegible hasta escribir
        src.slow.insert(250);
        let mut opts = fast_opts(RunMode::Repair, 0, 300);
        opts.allow_data_loss = true; // degraded: write-back explícito
        opts.prescan = true;
        let rep = run(&mut src, &opts, hooks_empty(), collect());
        // Solo los problemáticos reciben ciclo completo.
        assert_eq!(src.writes, vec![150, 250]);
        assert_eq!(rep.stats.recovered, 2);
        assert_eq!(rep.stats.bad, 0);
        assert!(rep.stats.healthy_skipped > 0);
        assert_eq!(rep.stats.scanned, 300);
        let flags: Vec<_> = rep
           .records
           .iter()
           .map(|r| (r.sector, r.flag.char(), r.run))
           .collect();
        assert_eq!(flags, vec![(0, 'S', 1), (150, 'R', 1), (250, 'R', 1), (299, 'L', 1)]);
    }

    #[test]
    fn run_con_prescan_scan_registra_b_y_delays() {
        let mut src = MockSource::new(200);
        src.broken.insert(140);
        src.slow.insert(30);
        // El slow 30 hace lento su bloque (15 ms > 5 ms), así que SÍ se aísla.
        let mut opts = fast_opts(RunMode::Scan, 0, 200);
        opts.prescan = true;
        let rep = run(&mut src, &opts, hooks_empty(), collect());
        assert_eq!(rep.stats.bad, 1);
        assert_eq!(rep.stats.delays, 1);
        assert!(src.writes.is_empty());
    }

    // ------------------------------------------------- bandas y refresh -----

    #[test]
    fn latency_band_clasifica_umbrales() {
        let b = [5u64, 20, 50, 200];
        let d = Duration::from_millis(200);
        assert_eq!(latency_band(Duration::from_millis(4), d, Some(b)), 0);
        assert_eq!(latency_band(Duration::from_millis(6), d, Some(b)), 1);
        assert_eq!(latency_band(Duration::from_millis(25), d, Some(b)), 2);
        assert_eq!(latency_band(Duration::from_millis(60), d, Some(b)), 3);
        assert_eq!(latency_band(Duration::from_millis(250), d, Some(b)), 4);
        // Sin bandas (sin clasificación): solo el umbral `delay`.
        assert_eq!(latency_band(Duration::from_millis(15), Duration::from_millis(5), None), 4);
        assert_eq!(latency_band(Duration::from_millis(3), Duration::from_millis(5), None), 0);
        // Flags canónicos de banda .
        assert_eq!(band_flag(1).char(), 'T');
        assert_eq!(band_flag(2).char(), 'U');
        assert_eq!(band_flag(3).char(), 'V');
        assert_eq!(band_flag(4).char(), 'X');
    }

    // Umbrales y sueño elegidos con margen ancho: en runners compartidos
    // `thread::sleep` puede pasarse decenas de ms, y el test quedaría flaky.
    #[test]
    fn scan_clasifica_bandas_y_escribe_flags() {
        let mut src = MockSource::new(50);
        src.slow_ms.insert(10, 15); // T1 → 'T'
        src.slow_ms.insert(11, 17); // T1 contiguo ⇒ corrida T2
        src.slow_ms.insert(20, 120); // T2 → 'U'
        src.slow_ms.insert(30, 250); // T3 → 'V'
        src.slow_ms.insert(40, 600); // T4 → 'X' (retardo)
        let opts = banded_opts(RunMode::Scan, 0, 50, [5, 100, 200, 400]);
        let rep = run(&mut src, &opts, hooks_empty(), collect());
        assert_eq!(src.writes.len(), 0, "Scan no escribe");
        assert_eq!(
            (rep.stats.slow1, rep.stats.slow2, rep.stats.slow3, rep.stats.delays),
            (2, 1, 1, 1)
        );
        let flags: Vec<_> = rep
           .records
           .iter()
           .map(|r| (r.sector, r.flag.char(), r.run))
           .collect();
        assert_eq!(
            flags,
            vec![(0, 'S', 1), (10, 'T', 2), (20, 'U', 1), (30, 'V', 1), (40, 'X', 1), (49, 'L', 1)]
        );
    }

    #[test]
    fn prescan_clasifica_bandas_de_problemas() {
        let mut src = MockSource::new(300);
        // Todos los lentos en el primer bloque: su tiempo total (≈865 ms)
        // supera el retardo (400 ms) ⇒ el bloque se aísla y se mide sector a
        // sector con bandas (margen ancho, ver nota del test anterior).
        src.slow_ms.insert(3, 250); // T3
        src.slow_ms.insert(4, 600); // T4
        src.slow_ms.insert(5, 15); // T1
        let opts = banded_opts(RunMode::Scan, 0, 300, [5, 100, 200, 400]);
        let rep = prescan(&mut src, &opts, &mut hooks_empty(), collect());
        assert_eq!(
            rep.problems,
            vec![
                (3, ProblemKind::Slow(3)),
                (4, ProblemKind::Slow(4)),
                (5, ProblemKind::Slow(1)),
            ]
        );
        assert_eq!(rep.retried_chunks, 1);
        assert_eq!(rep.bulk_chunks, 2);
    }

    /// Refresh: reescribe los mismos bytes de los lentos (cualquier
    /// banda) y deja intactos los sanos; el ilegible no se toca sin
    /// `--allow-data-loss` (comportamiento `repair`).
    #[test]
    fn refresh_reescribe_lentos_preservando_contenido() {
        let mut src = MockSource::new(40);
        src.slow_ms.insert(10, 15); // T1: Repair no la toca; Refresh sí
        src.slow_ms.insert(20, 600); // T4
        src.degraded.insert(30); // ilegible hasta escribir
        let opts = banded_opts(RunMode::Refresh, 0, 40, [5, 100, 200, 400]);
        let rep = run(&mut src, &opts, hooks_empty(), collect());
        assert_eq!(src.writes, vec![10, 20], "sin permiso no se escribe sobre ilegibles");
        assert_eq!(rep.stats.recovered, 2);
        assert_eq!(rep.stats.bad, 1);
        assert_eq!(rep.stats.data_lost, 0);
        // Refresh conserva el contenido leído (buf[0] == lba en el mock).
        assert_eq!(src.written_bytes, vec![(10, 10), (20, 20)]);
        let flags: Vec<_> = rep
           .records
           .iter()
           .map(|r| (r.sector, r.flag.char(), r.run))
           .collect();
        assert_eq!(flags, vec![(0, 'S', 1), (10, 'R', 1), (20, 'R', 1), (30, 'B', 1), (39, 'L', 1)]);
    }

    /// Repair clásico no refresca bandas T1..T3: solo pinta, no escribe.
    #[test]
    fn repair_no_refresca_bandas_bajas() {
        let mut src = MockSource::new(40);
        src.slow_ms.insert(10, 15); // T1
        src.slow_ms.insert(20, 600); // T4
        let opts = banded_opts(RunMode::Repair, 0, 40, [5, 100, 200, 400]);
        let rep = run(&mut src, &opts, hooks_empty(), collect());
        assert_eq!(src.writes, vec![20], "solo la banda T4 se reescribe");
        assert_eq!(rep.stats.recovered, 1);
        assert_eq!(rep.stats.bad, 0);
    }

    // ------------------------------------------------- monitor y progreso -----

    /// Monitor que aborta tras N polls.
    struct AbortAfter {
        polls_left: u32,
    }
    impl HealthMonitor for AbortAfter {
        fn poll(&mut self, _info: &PollInfo) -> MonitorAction {
            self.polls_left -= 1;
            if self.polls_left == 0 {
                MonitorAction::Abort
            } else {
                MonitorAction::Continue
            }
        }
    }

    #[test]
    fn monitor_puede_abortar_el_pase() {
        let total = (POLL_SECTORS * 2 + 10) as u32;
        let mut src = MockSource::new(total as u64);
        let mut monitor = AbortAfter { polls_left: 1 };
        let mut last_progress = u32::MAX;
        let mut progress = |sector: u32, _s: &EngineStats| {
            last_progress = sector;
        };
        let hooks = RunHooks {
            backup: None,
            monitor: Some(&mut monitor as &mut dyn HealthMonitor),
            progress: Some(&mut progress),
            ui: None,
        };
        let opts = fast_opts(RunMode::Scan, 0, total);
        let rep = run(&mut src, &opts, hooks, collect());
        assert!(rep.aborted);
        // El poll cae en el múltiplo de POLL_SECTORS; al abortar, el reporte
        // final de progreso deja un checkpoint en ese sector.
        assert_eq!(rep.last_sector, POLL_SECTORS as u32 - 1);
        assert_eq!(last_progress, POLL_SECTORS as u32 - 1);
        // Abortado: sin marca L (span incompleto).
        assert!(!rep.records.iter().any(|r| r.flag == crate::log::Flag::L));
    }

    #[test]
    fn progreso_reporta_al_terminar() {
        let mut last = 0u32;
        let mut src = MockSource::new(50);
        let mut progress = |sector: u32, _s: &EngineStats| {
            last = sector;
        };
        let hooks = RunHooks {
            backup: None,
            monitor: None,
            progress: Some(&mut progress),
            ui: None,
        };
        let rep = run(&mut src, &fast_opts(RunMode::Scan, 0, 50), hooks, collect());
        assert!(!rep.aborted);
        assert_eq!(last, 49); // último sector (reporte final)
    }

    /// Pausa por ESC: el hook `ui` devuelve false; el pase se
    /// aborta en ese punto y el reporte final de `progress` deja el
    /// checkpoint exactamente ahí (base de `--resume`).
    #[test]
    fn ui_puede_pausar_el_pase_y_deja_checkpoint() {
        let total = (POLL_SECTORS * 2 + 10) as u32;
        let mut src = MockSource::new(total as u64);
        let mut ui_calls = 0u32;
        let mut last_progress = u32::MAX;
        let mut ui = |_cells: &[Cell; GRID_CELLS], _s: &EngineStats, _sec: u32| -> bool {
            ui_calls += 1;
            false // "ESC"
        };
        let mut progress = |sector: u32, _s: &EngineStats| {
            last_progress = sector;
        };
        let hooks = RunHooks {
            backup: None,
            monitor: None,
            progress: Some(&mut progress),
            ui: Some(&mut ui),
        };
        let rep = run(&mut src, &fast_opts(RunMode::Scan, 0, total), hooks, collect());
        assert!(rep.aborted);
        // El mock es instantáneo: el primer tick cae en el poll de salud
        // (sector POLL_SECTORS-1); si el reloj del test fuera lento podría
        // caer antes por tiempo — el invariante es el checkpoint en la pausa.
        assert!(ui_calls >= 1);
        assert_eq!(rep.last_sector, last_progress);
        assert!(rep.last_sector < total - 1, "se cortó antes del fin");
        assert!(!rep.records.iter().any(|r| r.flag == crate::log::Flag::L));
    }
}
