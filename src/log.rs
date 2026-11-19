// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Gramática de registro de sectores (formato compartido con el historial de
//! estas herramientas para interoperar con logs existentes).
//!
//! Formato del cuerpo y pie (mantenido para interoperar con los registros
//! históricos):
//!
//! ```text
//! <sector>=<FLAG><run>\r\n      ← registros del cuerpo (ordenados por sector)
//! \r\n                           ← línea vacía
//! \r\n                           ← segunda línea vacía
//! DISK <n>: <size> Mb|Gb in <sectors> sectors\r\n   ← footer (nuevo)
//! Created=<asctime>                                ← sin CRLF final
//! ```
//!
//! Los logs antiguos en formato histórico (`HDD n:` en el footer) se parsean
//! igual: el footer se conserva **crudo** para round-trip idéntico.
//!
//! - `sector`: sector inicial del tramo (decimal; se imprime como int32 con
//!   signo por compatibilidad).
//! - `FLAG`: `S` inicio de tramo, `L` fin de tramo, `B` malo no reparado,
//!   `R` recuperado, `X` lento/protegido. Legacy (solo lectura): `A,C,D,N,P`.
//!   Bandas de latencia: `T` T1, `U` T2, `V` T3; la peor banda (T4) se
//!   escribe con `X`, así los lectores históricos del formato entienden al
//!   menos la banda peor.
//! - `run`: longitud del tramo, SIEMPRE presente (run-length). Las bandas de
//!   lentitud T1–T3 usan `T`, `U`, `V`; la peor banda (T4) usa `X`, el flag
//!   ya existente del formato para "sector lento".

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flag {
    S,
    L,
    B,
    R,
    X,
    // Bandas de latencia escritas por regrow: T1→T, T2→U, T3→V; la banda 4
    // (peor) se escribe con X.
    T,
    U,
    V,
    // Legacy (solo lectura; regrow NO los escribe):
    A,
    C,
    D,
    N,
    P,
}

impl Flag {
    pub fn from_char(c: char) -> Option<Flag> {
        match c {
            'S' => Some(Flag::S),
            'L' => Some(Flag::L),
            'B' => Some(Flag::B),
            'R' => Some(Flag::R),
            'X' => Some(Flag::X),
            'T' => Some(Flag::T),
            'U' => Some(Flag::U),
            'V' => Some(Flag::V),
            'A' => Some(Flag::A),
            'C' => Some(Flag::C),
            'D' => Some(Flag::D),
            'N' => Some(Flag::N),
            'P' => Some(Flag::P),
            _ => None,
        }
    }

    pub fn char(self) -> char {
        match self {
            Flag::S => 'S',
            Flag::L => 'L',
            Flag::B => 'B',
            Flag::R => 'R',
            Flag::X => 'X',
            Flag::T => 'T',
            Flag::U => 'U',
            Flag::V => 'V',
            Flag::A => 'A',
            Flag::C => 'C',
            Flag::D => 'D',
            Flag::N => 'N',
            Flag::P => 'P',
        }
    }

    /// Normalización de flags legacy al núcleo del formato ({B,N,A}→B,
    /// {R,C,D}→R, P→X). Las bandas T/U/V/X son canónicas y no se normalizan
    /// entre sí: la banda medida es el dato.
    pub fn canonical(self) -> Flag {
        match self {
            Flag::A | Flag::N => Flag::B,
            Flag::C | Flag::D => Flag::R,
            Flag::P => Flag::X,
            f => f,
        }
    }

    /// ¿Flag canónico escrito por regrow (no alias legacy)? API pública.
    #[allow(dead_code)]
    pub fn is_core(self) -> bool {
        matches!(
            self,
            Flag::S | Flag::L | Flag::B | Flag::R | Flag::X | Flag::T | Flag::U | Flag::V
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    pub sector: u32,
    pub flag: Flag,
    pub run: u32,
}

impl Record {
    pub fn end(&self) -> u32 {
        self.sector + self.run - 1
    }

    /// `sector=FLAGrun` sin CRLF. El sector se imprime como int32 con signo
    /// (compatibilidad con el formato histórico).
    pub fn render_line(&self) -> String {
        format!("{}={}{}", self.sector as i32, self.flag.char(), self.run)
    }
}

/// Un archivo de registro: registros del cuerpo + footer crudo verbatim.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiskLog {
    pub records: Vec<Record>,
    /// Todo lo posterior al cuerpo (las dos líneas vacías y el footer),
    /// incluido el CRLF final ausente en la última línea del footer.
    pub raw_footer: Vec<u8>,
}

/// Errores estrictos de gramática. El parser de regrow es tolerante (línea
/// inválida = fin del cuerpo), así que estas variantes quedan reservadas para
/// un modo estricto futuro.
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    LineTooLong,
    InvalidSector,
    InvalidFlag,
    InvalidRun,
    MissingRun,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::LineTooLong => write!(f, "línea de registro demasiado larga"),
            ParseError::InvalidSector => write!(f, "sector no numérico"),
            ParseError::InvalidFlag => write!(f, "flag desconocido"),
            ParseError::InvalidRun => write!(f, "run-length no numérico"),
            ParseError::MissingRun => write!(f, "run-length ausente"),
        }
    }
}

impl DiskLog {
    /// Intenta parsear `line` como `<sector>=<FLAG><run>`.
    /// Devuelve Some(None) si la línea es vacío o no es un registro válido
    /// (marca el fin del cuerpo).
    fn parse_record(line: &[u8]) -> Option<Option<Record>> {
        if line.is_empty() {
            return Some(None);
        }
        let text = match std::str::from_utf8(line) {
            Ok(t) => t,
            Err(_) => return Some(None),
        };
        if text.len() > 32 {
            return Some(None);
        }
        let eq = text.find('=')?;
        let (sector_s, rest) = text.split_at(eq);
        // El formato histórico imprime el sector con ltoa (int32 CON signo): un LBA
        // ≥ 2^31 aparece envuelto en negativo (p. ej. HDD3 ≈ 3.9e9 sectores).
        // Lo parseamos como i32 y lo almacenamos envuelto en u32.
        let sector: i32 = sector_s.parse().ok()?;
        let sector = sector as u32;
        let rest = &rest[1..];
        let mut chars = rest.chars();
        let flag = Flag::from_char(chars.next()?)?;
        let run_s: String = chars.collect();
        if run_s.is_empty() {
            return Some(None);
        }
        if !run_s.bytes().all(|b| b.is_ascii_digit()) {
            return Some(None);
        }
        let run: u32 = run_s.parse().ok()?;
        if run == 0 {
            return Some(None);
        }
        Some(Some(Record { sector, flag, run }))
    }

    pub fn parse(bytes: &[u8]) -> Result<DiskLog, ParseError> {
        let mut records = Vec::new();
        // Escanear líneas CRLF hasta la primera que no es registro.
        let mut pos = 0usize;
        while let Some(nl) = find(bytes, b"\r\n", pos) {
            let line = &bytes[pos..nl];
            match Self::parse_record(line) {
                Some(Some(rec)) => records.push(rec),
                _ => {
                    // Fin del cuerpo: el footer (incluida esta línea) es crudo.
                    return Ok(DiskLog {
                        records,
                        raw_footer: bytes[pos..].to_vec(),
                    });
                }
            }
            pos = nl + 2;
        }
        // Cuerpo consumió todo el archivo (sin footer).
        Ok(DiskLog {
            records,
            raw_footer: Vec::new(),
        })
    }

    /// Renderiza el cuerpo canónico + footer crudo → round-trip byte-idéntico
    /// cuando el input ya está en forma canónica.
    pub fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for rec in &self.records {
            out.extend_from_slice(rec.render_line().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(&self.raw_footer);
        out
    }

    /// Construye el footer estándar de un log nuevo de regrow:
    /// `\r\n\r\nDISK n…\r\n` + líneas extra + `Created=<asctime>` sin CRLF final.
    pub fn make_footer(disk_line: &str, extra_lines: &[String], created: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\r\n\r\n");
        out.extend_from_slice(disk_line.as_bytes());
        out.extend_from_slice(b"\r\n");
        for l in extra_lines {
            out.extend_from_slice(l.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(created.as_bytes());
        out
    }

    /// Fusión run-length (misma semántica que el merge histórico): tramos
    /// contiguos con la misma bandera se unen incrementando `run`.
    /// Colapsa tramos contiguos del mismo flag (normalización; usada en tests
    /// y expuesta para el panel Drive Map).
    #[allow(dead_code)]
    pub fn merge_runs(records: Vec<Record>) -> Vec<Record> {
        let mut sorted = records;
        sorted.sort_by_key(|r| (r.sector, r.flag as u8));
        let mut out: Vec<Record> = Vec::new();
        for r in sorted {
            // Tramo fusionable: mismo flag y exactamente contiguo.
            if let Some(last) = out.last_mut() {
                if last.flag == r.flag && r.sector == last.sector + last.run {
                    last.run += r.run;
                    continue;
                }
            }
            out.push(r);
        }
        out
    }

    /// Expande los tramos a un mapa sector→flag.
    pub fn to_sector_map(&self) -> std::collections::BTreeMap<u32, Flag> {
        let mut map = std::collections::BTreeMap::new();
        for r in &self.records {
            for s in r.sector..=r.end() {
                map.insert(s, r.flag);
            }
        }
        map
    }
}

/// Merge de dos logs sobre la matriz de fusión del formato,
/// proyectado sobre el subconjunto core
/// S/L/B/R/X que escribe regrow (los legacy se normalizan al leer):
///
/// - Span del pase nuevo = `[new_start, new_end]` (marcas S/L del pase nuevo).
/// - Sector dentro del span:
///   - flag nuevo B/R ⇒ gana (B→B/N canónico = B; R→R/D canónico = R).
///   - sin flag nuevo, viejo B/R ⇒ promocionado a R (el pase nuevo pudo leerlo).
///   - viejo X ⇒ preservado (marca protegida).
/// - Sector fuera del span: se conserva el viejo tal cual (X/B/R).
/// - S/L viejos descartados; los nuevos provienen del pase.
/// - Bandas (T/U/V/X): la banda nueva medida gana; la banda vieja se
///   preserva como marca protegida (misma regla que X).
pub fn merge_logs(old: &DiskLog, new_pass: &DiskLog) -> DiskLog {
    let mut out_records: Vec<Record> = Vec::new();

    // Span del pase nuevo.
    let new_start = new_pass
        .records
        .iter()
        .find(|r| r.flag == Flag::S)
        .map(|r| r.sector);
    let new_end = new_pass
        .records
        .iter()
        .find(|r| r.flag == Flag::L)
        .map(|r| r.end());
    // Span del pase nuevo. Si el pase está incompleto (pasada abortada: tiene
    // S pero no L) su cobertura real no se conoce, así que se toma la frontera
    // conservadora: hasta el record más lejano que emitió. Sirve SOLO para la
    // matriz de fusión; la marca L emitida nunca se inventa (ver `top`).
    let new_cover_end = new_end.or_else(|| {
        new_pass
            .records
            .iter()
            .filter(|r| r.flag != Flag::S)
            .map(|r| r.end())
            .max()
    });
    let span: Option<(u32, u32)> = match (new_start, new_cover_end) {
        (Some(a), Some(b)) if b >= a => Some((a, b)),
        _ => None,
    };

    // Reanudación: el span que se EMITE es el histórico, no el
    // del pase nuevo. `floor` = primer sector escaneado jamás registrado en
    // este log (si el pase nuevo reanuda en 50, el log debe seguir diciendo
    // que el disco se empezó a escanear en 0); `top` = cierre, que solo
    // existe si el pase nuevo lo trae o si el log previo ya estaba cerrado.
    // Ojo: la matriz de fusión de flags sigue usando `span` (el del pase
    // nuevo) para descartar los B no re-confirmados.
    let old_start = old
        .records
        .iter()
        .filter(|r| r.flag == Flag::S)
        .map(|r| r.sector)
        .min();
    let old_end = old
        .records
        .iter()
        .filter(|r| r.flag == Flag::L)
        .map(|r| r.end())
        .max();
    let floor = match (old_start, new_start) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let top = match (new_end, old_end) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };

    let mut old_map = std::collections::BTreeMap::new();
    for r in &old.records {
        match r.flag {
            Flag::S | Flag::L => continue, // descartados (los S/L los fija el pase nuevo)
            f => {
                for s in r.sector..=r.end() {
                    old_map.insert(s, f.canonical());
                }
            }
        }
    }
    let new_map = new_pass.to_sector_map();

    // Sector a sector del dominio conjunto.
    let mut merged: std::collections::BTreeMap<u32, Flag> = std::collections::BTreeMap::new();
    let all: std::collections::BTreeSet<u32> =
        old_map.keys().chain(new_map.keys()).copied().collect();
    for s in all {
        let old_f = old_map.get(&s).copied();
        let new_f = match &span {
            Some((a, e)) if s >= *a && s <= *e => new_map
                .get(&s)
                .copied()
                .filter(|f| matches!(f, Flag::B | Flag::R | Flag::X | Flag::T | Flag::U | Flag::V)),
            _ => None,
        };
        // Matriz de fusión del formato (bucle de dos iteradores):
        //   nuevo X          ⇒ X (por sector, ambos lados)
        //   nuevo B          ⇒ B (o A≈B si el viejo era {R,C,D})
        //   nuevo R          ⇒ R (o C≈R si el viejo era {R,C,D})
        //   sin flag nuevo   ⇒ X viejo preservado; {R,C,D}→R; viejo B/N/A
        //                     DENTRO del span nuevo ⇒ descartado (no
        //                     re-confirmado); fuera del span ⇒ conservado.
        // Bandas T/U/V: una banda nueva medida gana sobre la
        // marca vieja; una banda vieja se preserva igual que X (marca
        // protegida: el pase nuevo no reescribe sanos, así que no puede
        // desmentirla). B/R siguen mandando sobre las bandas.
        let resolved: Flag = match (old_f, new_f) {
            (_, Some(Flag::X)) => Flag::X,
            (_, Some(Flag::B)) => Flag::B,
            (_, Some(Flag::R)) => Flag::R,
            (_, Some(f @ (Flag::T | Flag::U | Flag::V))) => f,
            (Some(Flag::X), None) => Flag::X,
            (Some(Flag::R), None) => Flag::R,
            (Some(Flag::B), None) => {
                if span.as_ref().is_some_and(|(a, e)| s >= *a && s <= *e) {
                    continue; // malo no re-confirmado por el pase nuevo
                }
                Flag::B // fuera del span nuevo: conservado
            }
            (None, Some(f)) => f,
            // old ∈ {S,L} nunca llega (old_map los descarta); catch-all teórico.
            (Some(f), _) => f,
            (None, None) => continue,
        };
        merged.insert(s, resolved);
    }

    // colapsar mapa → tramos run-length, fusionando contiguos del mismo flag
    let mut cur: Option<Record> = None;
    for (s, f) in merged {
        match &mut cur {
            Some(r) if r.flag == f && r.end() + 1 == s => r.run += 1,
            _ => {
                if let Some(r) = cur.take() {
                    out_records.push(r);
                }
                cur = Some(Record {
                    sector: s,
                    flag: f,
                    run: 1,
                });
            }
        }
    }
    if let Some(r) = cur {
        out_records.push(r);
    }

    // Reinsertar las marcas S/L del span histórico  .
    if let Some(a) = floor {
        out_records.push(Record {
            sector: a,
            flag: Flag::S,
            run: 1,
        });
    }
    if let Some(e) = top {
        out_records.push(Record {
            sector: e,
            flag: Flag::L,
            run: 1,
        });
    }
    // Orden ascendente por sector (los tramos del
    // mapa van primero y las marcas S/L al final del vector; un log fusionado
    // desordenado despista al que lo lee con `more`).
    out_records.sort_by_key(|r| r.sector);

    DiskLog {
        records: out_records,
        raw_footer: new_pass.raw_footer.clone(),
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Contadores del panel post-escaneo:
/// malos persistentes y total alguna vez recuperado.
#[derive(Clone, Copy, Debug, Default)]
#[allow(dead_code)]
pub struct PanelStats {
    pub remain_bad: u64, // B (+legacy N/A)
    pub ever_recovered: u64, // R (+legacy C/D)
    pub delays: u64,     // X: sectores marcados como lentos (banda T4)
    pub slow1: u64,      // T: banda T1
    pub slow2: u64,      // U: banda T2
    pub slow3: u64,      // V: banda T3
}

impl PanelStats {
    #[allow(dead_code)]
    pub fn from_records(records: &[Record]) -> PanelStats {
        let mut s = PanelStats::default();
        for r in records {
            match r.flag.canonical() {
                Flag::B => s.remain_bad += r.run as u64,
                Flag::R => s.ever_recovered += r.run as u64,
                Flag::X => s.delays += r.run as u64,
                Flag::T => s.slow1 += r.run as u64,
                Flag::U => s.slow2 += r.run as u64,
                Flag::V => s.slow3 += r.run as u64,
                _ => {}
            }
        }
        s
    }
}

/// Extensión escaneada que el log revela: desde el primer marcador S.
/// - Con marcador L (pasada completa): [S..L+1) cerrada.
/// - Sin L (pasada interrumpida): el log solo conoce el mayor sector
///   registrado (anomalías); se marca `closed=false` — el progreso exacto
///   de un escaneo limpio e interrumpido NO queda en el log (el formato
///   solo registra anomalías + bordes S/L).
pub fn scan_extent(log: &DiskLog) -> Option<(u64, u64, bool)> {
    let start = log
        .records
        .iter()
        .filter(|r| r.flag == Flag::S)
        .map(|r| r.sector as u64)
        .min()?;
    if let Some(end) = log
        .records
        .iter()
        .filter(|r| r.flag == Flag::L)
        .map(|r| r.end() as u64 + 1)
        .max()
    {
        return Some((start, end, true));
    }
    let known = log
        .records
        .iter()
        .map(|r| r.end() as u64 + 1)
        .max()
        .unwrap_or(start + 1)
        .max(start + 1);
    Some((start, known, false))
}

/// Fecha `Created=<asctime>` del footer del log (si está presente).
pub fn log_created(raw_footer: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw_footer);
    let idx = text.find("Created=")?;
    let rest = &text[idx + "Created=".len()..];
    let end = rest.find(['\r', '\n']).unwrap_or(rest.len());
    let val = rest[..end].trim();
    (!val.is_empty()).then(|| val.to_string())
}

/// HH:MM:SS desde segundos totales (formato de resumen).
pub fn format_hms(total_secs: u64) -> String {
    let h = total_secs / 3600;
    let m = (total_secs / 60) % 60;
    let s = total_secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// `asctime` para el footer del log: `Wed Jan 14 02:39:16 2026`,
/// en **hora local** (bug de campo 2026-11-19: el sello se escribía en UTC,
/// desfasado 1 h respecto del reloj del usuario). Unix: `localtime_r` +
/// `strftime`; Windows: `_localtime64` + `asctime`. El `struct tm` se trata
/// como búfer opaco (jamás leemos sus campos; misma política que termios).
pub fn format_asctime(secs_since_epoch: i64) -> String {
    #[cfg(unix)]
    {
        extern "C" {
            fn localtime_r(timep: *const i64, result: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
            fn strftime(s: *mut u8, max: usize, fmt: *const u8, tm: *const core::ffi::c_void) -> usize;
        }
        // struct tm opaco: 128 B cubre con holgura macOS (~56 B) y Linux x86_64 (36 B).
        let mut tm = [0u64; 16];
        let mut out = [0u8; 64];
        unsafe {
            if !localtime_r(&secs_since_epoch, tm.as_mut_ptr() as *mut core::ffi::c_void).is_null() {
                // "%e" = día con relleno de espacio, como asctime ("Nov  9").
                let n = strftime(
                    out.as_mut_ptr(),
                    out.len(),
                    b"%a %b %e %H:%M:%S %Y\0".as_ptr(),
                    tm.as_ptr() as *const core::ffi::c_void,
                );
                if n > 0 {
                    return String::from_utf8_lossy(&out[..n]).trim_end().to_string();
                }
            }
        }
        format_asctime_utc(secs_since_epoch)
    }
    #[cfg(windows)]
    {
        extern "C" {
            fn _localtime64(timep: *const i64, result: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
            fn asctime(tm: *const core::ffi::c_void) -> *const u8;
        }
        let mut tm = [0u64; 16];
        unsafe {
            if !_localtime64(&secs_since_epoch, tm.as_mut_ptr() as *mut core::ffi::c_void).is_null() {
                let p = asctime(tm.as_ptr() as *const core::ffi::c_void);
                if !p.is_null() {
                    let mut v: Vec<u8> = Vec::with_capacity(26);
                    for i in 0..32usize {
                        let c = *p.add(i);
                        if c == 0 {
                            break;
                        }
                        v.push(c);
                    }
                    return String::from_utf8_lossy(&v).trim_end().to_string();
                }
            }
        }
        format_asctime_utc(secs_since_epoch)
    }
    #[cfg(not(any(unix, windows)))]
    {
        format_asctime_utc(secs_since_epoch)
    }
}

/// Variante UTC pura (civil_from_days de Howard Hinnant): fallback si las
/// funciones de zona horaria del SO fallan, y referencia para los tests.
pub fn format_asctime_utc(secs_since_epoch: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs_since_epoch.div_euclid(86400);
    let rem = secs_since_epoch.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    // 1970-01-01 (epoch day 0) fue jueves ⇒ DAYS[0]="Sun" exige +4.
    let weekday = DAYS[(days + 4).rem_euclid(7) as usize];
    format!(
        "{} {} {:02} {:02}:{:02}:{:02} {}",
        weekday,
        MONTHS[(m - 1) as usize],
        d,
        rem / 3600,
        (rem / 60) % 60,
        rem % 60,
        y
    )
}

/// Algoritmo civil_from_days de Howard Hinnant (fecha proleptica gregoriana).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        // Logs externos de interoperabilidad: solo si el usuario los señala
        // explícitamente con la variable de entorno REGROW_REF_LOGS.
        PathBuf::from(std::env::var("REGROW_REF_LOGS").unwrap_or_default())
    }

    /// Tests de interoperabilidad con logs externos NO versionados en el
    /// repo. Sin la variable `REGROW_REF_LOGS` apuntando a un directorio,
    /// estos tests se saltan limpiamente (el repo publicado es autocontenido).
    fn fixtures_available() -> Option<PathBuf> {
        let d = fixture_dir();
        if d.is_dir() {
            Some(d)
        } else {
            eprintln!("test saltado: falta {} (fixtures locales de referencia)", d.display());
            None
        }
    }

    #[test]
    fn roundtrip_identicos_con_logs_reales() {
        // Round-trip byte-idéntico contra TODOS los logs de referencia.
        let Some(dir) = fixtures_available() else { return };
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() == ".DS_Store" {
                continue;
            }
            let data = std::fs::read(&path).unwrap();
            let parsed = DiskLog::parse(&data)
                .unwrap_or_else(|e| panic!("{}: parse falló: {e}", path.display()));
            let rendered = parsed.render();
            assert_eq!(
                data, rendered,
                "round-trip NO idéntico en {}",
                path.display()
            );
            checked += 1;
        }
        assert!(checked >= 8, "se esperaban ≥8 fixtures, se comprobaron {checked}");
    }

    /// `scan_extent`: con L el span es exacto y cerrado; sin L (pasada
    /// interrumpida) devuelve la cota conocida y closed=false.
    #[test]
    fn scan_extent_cerrada_y_abierta() {
        let closed = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::S, run: 1 },
                Record { sector: 10, flag: Flag::B, run: 2 },
                Record { sector: 99, flag: Flag::L, run: 1 },
            ],
            raw_footer: vec![],
        };
        assert_eq!(scan_extent(&closed), Some((0, 100, true)));
        let open = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::S, run: 1 },
                Record { sector: 10, flag: Flag::X, run: 2 },
            ],
            raw_footer: vec![],
        };
        assert_eq!(scan_extent(&open), Some((0, 12, false)));
        // Solo S (scan limpio interrumpido): cota mínima honesta.
        let only_s = DiskLog {
            records: vec![Record { sector: 0, flag: Flag::S, run: 1 }],
            raw_footer: vec![],
        };
        assert_eq!(scan_extent(&only_s), Some((0, 1, false)));
    }

    #[test]
    fn panel_stats_cuenta_delays() {
        let recs = [
            Record { sector: 0, flag: Flag::S, run: 1 },
            Record { sector: 4, flag: Flag::X, run: 3 },
            Record { sector: 9, flag: Flag::B, run: 1 },
            Record { sector: 9, flag: Flag::R, run: 1 },
        ];
        let ps = PanelStats::from_records(&recs);
        assert_eq!((ps.remain_bad, ps.ever_recovered, ps.delays), (1, 1, 3));
    }

    #[test]
    fn log_created_sale_del_footer() {
        let footer = b"\r\n\r\nDISK 2: 298 Gb in 625142448 sectors\r\nLog Created=Wed Jan 14 02:39:16 2026";
        assert_eq!(
            log_created(footer).as_deref(),
            Some("Wed Jan 14 02:39:16 2026")
        );
        assert_eq!(log_created(b"sin fecha").as_deref(), None);
    }

    /// Parseado del log histórico abierto HDD2.LOG (fixture local).
    #[test]
    fn flags_de_hdd2_log() {
        let Some(dir) = fixtures_available() else { return };
        let data = std::fs::read(dir.join("HDD2.LOG")).unwrap();
        let log = DiskLog::parse(&data).unwrap();
        assert_eq!(log.records[0], Record { sector: 0, flag: Flag::S, run: 1 });
        assert_eq!(
            log.records[1],
            Record {
                sector: 415823,
                flag: Flag::R,
                run: 1
            }
        );
        let last = *log.records.last().unwrap();
        assert_eq!(
            last,
            Record {
                sector: 625142447,
                flag: Flag::L,
                run: 1
            }
        );
        let stats = PanelStats::from_records(&log.records);
        // 10×R1 + 3×R2 = 16 sectores recuperados (13 registros).
        assert_eq!(stats.ever_recovered, 16);
        assert_eq!(stats.remain_bad, 0);
    }

    /// Log histórico abierto con la marca protegida X (fixture local).
    #[test]
    fn x_protegido_en_001() {
        let Some(dir) = fixtures_available() else { return };
        let data = std::fs::read(dir.join("HDD2.001")).unwrap();
        let log = DiskLog::parse(&data).unwrap();
        assert_eq!(log.records[0], Record { sector: 0, flag: Flag::X, run: 2 });
        assert_eq!(
            log.records[1],
            Record {
                sector: 900000000,
                flag: Flag::S,
                run: 1
            }
        );
    }

    #[test]
    fn merge_runs_contiguos() {
        let merged = DiskLog::merge_runs(vec![
            Record { sector: 10, flag: Flag::B, run: 1 },
            Record { sector: 11, flag: Flag::B, run: 1 },
            Record { sector: 12, flag: Flag::B, run: 1 },
            Record { sector: 20, flag: Flag::R, run: 2 },
        ]);
        assert_eq!(
            merged,
            vec![
                Record { sector: 10, flag: Flag::B, run: 3 },
                Record { sector: 20, flag: Flag::R, run: 2 },
            ]
        );
    }

    #[test]
    fn hms_y_asctime() {
        assert_eq!(format_hms(3661), "01:01:01");
        assert_eq!(format_hms(0), "00:00:00");
        // HDD2.LOG footer: Log Created=Wed Jan 14 02:39:16 2026
        // 2026-01-14T02:39:16Z = 1768358356 (la variante UTC es la
        // referencia fija; format_asctime aplica la zona horaria local).
        assert_eq!(format_asctime_utc(1768358356), "Wed Jan 14 02:39:16 2026");
        // Sello local: formato asctime válido (día de semana + mes + hora).
        let local = format_asctime(1768358356);
        assert!(local.starts_with("Wed Jan 1") && local.ends_with("2026"), "{local}");
    }

    #[test]
    fn footer_estandar_roundtrip() {
        let log = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::S, run: 1 },
                Record { sector: 5, flag: Flag::B, run: 2 },
                Record { sector: 9, flag: Flag::L, run: 1 },
            ],
            raw_footer: DiskLog::make_footer(
                "DISK 1: 1928 Mb in 3948544 sectors",
                &[],
                "Created=Wed Jan 14 02:39:16 2026",
            ),
        };
        let rendered = log.render();
        let reparsed = DiskLog::parse(&rendered).unwrap();
        assert_eq!(log.records, reparsed.records);
        assert_eq!(rendered, reparsed.render());
        // sanity gramatical
        let s = String::from_utf8_lossy(&rendered);
        assert!(s.contains("5=B2\r\n9=L1\r\n\r\n\r\nDISK 1: 1928 Mb in 3948544 sectors\r\n"));
        assert!(!rendered.ends_with(b"\r\n"));
    }

    /// Sector envuelto como int32 (equivale al `ltoa` del formato histórico).
    #[test]
    fn sector_envuelto_signed_roundtrip() {
        let rec = Record {
            sector: 3_907_029_167,
            flag: Flag::L,
            run: 1,
        };
        assert_eq!(rec.render_line(), "-387938129=L1");
        let back = DiskLog::parse(b"-387938129=L1\r\n").unwrap();
        assert_eq!(back.records[0], rec);
    }

    #[test]
    fn merge_logs_matriz_core() {
        let old = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::X, run: 2 },
                Record { sector: 100, flag: Flag::R, run: 1 },
                Record { sector: 200, flag: Flag::B, run: 1 },
                Record { sector: 300, flag: Flag::S, run: 1 }, // descartado
            ],
            raw_footer: Vec::new(),
        };
        let new_pass = DiskLog {
            records: vec![
                Record { sector: 50, flag: Flag::S, run: 1 },
                Record { sector: 100, flag: Flag::B, run: 1 }, // nuevo malo ⇒ B
                Record { sector: 900, flag: Flag::R, run: 1 }, // recuperado
                Record { sector: 999, flag: Flag::L, run: 1 },
            ],
            raw_footer: b"\r\n\r\nDISK 1: 1 Mb in 2048 sectors\r\nCreated=X".to_vec(),
        };
        let m = merge_logs(&old, &new_pass);
        let map = m.to_sector_map();
        assert_eq!(map.get(&0), Some(&Flag::X)); // X preservado
        assert_eq!(map.get(&1), Some(&Flag::X));
        assert_eq!(map.get(&100), Some(&Flag::B)); // nuevo flag gana
        assert_eq!(map.get(&200), None); // B viejo DENTRO del span nuevo (50..999) sin confirmar ⇒ descartado
        assert_eq!(map.get(&900), Some(&Flag::R));
        // S/L viejos descartados; S/L nuevos presentes
        assert!(m.records.iter().any(|r| r.flag == Flag::S && r.sector == 50));
        assert!(m.records.iter().any(|r| r.flag == Flag::S && r.sector == 300) == false);
        assert_eq!(m.raw_footer, new_pass.raw_footer);
    }

    /// el span que se escribe al fusionar es el histórico (S del
    /// pase previo + cierre del más lejano), y los records salen ordenados por
    /// sector como en los logs originales.
    #[test]
    fn merge_logs_span_historico_y_orden() {
        // Log de una pasada interrumpida: empezó en 0, cortada en 40.
        let old = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::S, run: 1 },
                Record { sector: 10, flag: Flag::B, run: 2 },
            ],
            raw_footer: Vec::new(),
        };
        // Pasada reanudada 50..100 y esta sí completada.
        let new_pass = DiskLog {
            records: vec![
                Record { sector: 50, flag: Flag::S, run: 1 },
                Record { sector: 60, flag: Flag::X, run: 1 },
                Record { sector: 99, flag: Flag::L, run: 1 },
            ],
            raw_footer: b"\r\n\r\nDISK 3: 50 Kb in 100 sectors\r\nCreated=X".to_vec(),
        };
        let m = merge_logs(&old, &new_pass);
        let body: Vec<String> = m.records.iter().map(|r| r.render_line()).collect();
        assert_eq!(body, vec!["0=S1", "10=B2", "60=X1", "99=L1"]);
        // El panel ve el span completo (0..100) y la pasada como cerrada.
        assert_eq!(scan_extent(&m), Some((0, 100, true)));

        // Reanudar y volver a abortar: se conserva el floor y sigue sin L.
        let partial = DiskLog {
            records: vec![
                Record { sector: 50, flag: Flag::S, run: 1 },
                Record { sector: 70, flag: Flag::R, run: 1 },
            ],
            raw_footer: Vec::new(),
        };
        let m2 = merge_logs(&old, &partial);
        let body2: Vec<String> = m2.records.iter().map(|r| r.render_line()).collect();
        assert_eq!(body2, vec!["0=S1", "10=B2", "70=R1"]);
        assert_eq!(scan_extent(&m2), Some((0, 71, false)));
    }

    /// las bandas T/U/V (y X para T4) se renderizan y se
    /// re-parsean literalmente — la gramática `<sector>=<FLAG><run>` no cambia.
    #[test]
    fn bandas_d16_roundtrip_y_panel() {
        let recs = vec![
            Record { sector: 0, flag: Flag::S, run: 1 },
            Record { sector: 100, flag: Flag::T, run: 3 },
            Record { sector: 103, flag: Flag::U, run: 2 },
            Record { sector: 110, flag: Flag::V, run: 1 },
            Record { sector: 111, flag: Flag::X, run: 1 },
            Record { sector: 200, flag: Flag::L, run: 1 },
        ];
        let body: String = recs.iter().map(|r| r.render_line() + "\r\n").collect();
        let log = DiskLog::parse(body.as_bytes()).unwrap();
        assert_eq!(log.records, recs);
        assert!(log.records.iter().all(|r| r.flag.is_core()));
        let ps = PanelStats::from_records(&log.records);
        assert_eq!((ps.slow1, ps.slow2, ps.slow3, ps.delays), (3, 2, 1, 1));
    }

    /// la banda nueva medida gana sobre la marca vieja; una
    /// banda vieja sin flag nuevo queda protegida (misma regla que X);
    /// B/R mandan sobre las bandas.
    #[test]
    fn merge_matriz_de_bandas() {
        let old = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::S, run: 1 },
                Record { sector: 10, flag: Flag::V, run: 1 },
                Record { sector: 20, flag: Flag::X, run: 1 },
                Record { sector: 30, flag: Flag::T, run: 1 },
                Record { sector: 39, flag: Flag::L, run: 1 },
            ],
            raw_footer: Vec::new(),
        };
        let new_pass = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::S, run: 1 },
                Record { sector: 10, flag: Flag::T, run: 1 }, // ahora medida T1: mejora
                Record { sector: 20, flag: Flag::U, run: 1 }, // T2 nueva gana sobre X vieja
                Record { sector: 30, flag: Flag::R, run: 1 }, // refrescada: R manda
                Record { sector: 39, flag: Flag::L, run: 1 },
            ],
            raw_footer: Vec::new(),
        };
        let m = merge_logs(&old, &new_pass);
        let body: Vec<String> = m.records.iter().map(|r| r.render_line()).collect();
        assert_eq!(body, vec!["0=S1", "10=T1", "20=U1", "30=R1", "39=L1"]);

        // Pasada nueva sin flags: las bandas viejas se preservan.
        let quiet = DiskLog {
            records: vec![
                Record { sector: 0, flag: Flag::S, run: 1 },
                Record { sector: 39, flag: Flag::L, run: 1 },
            ],
            raw_footer: Vec::new(),
        };
        let m2 = merge_logs(&old, &quiet);
        let body2: Vec<String> = m2.records.iter().map(|r| r.render_line()).collect();
        assert_eq!(body2, vec!["0=S1", "10=V1", "20=X1", "30=T1", "39=L1"]);
    }

    #[test]
    fn rotacion_cascada_nombres_tres_digitos() {
        let dir = std::env::temp_dir().join("regrow_rot_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("DISK2.LOG");
        // Preexistente: .LOG, .001, .005 (el .005 debe borrarse).
        std::fs::write(&log, b"new").unwrap();
        std::fs::write(dir.join("DISK2.001"), b"old1").unwrap();
        std::fs::write(dir.join("DISK2.005"), b"old5").unwrap();
        crate::log::rotate_log_chain(&log, 5).unwrap();
        assert!(!dir.join("DISK2.005").exists(), "el .005 se borra");
        assert_eq!(std::fs::read(dir.join("DISK2.002")).unwrap(), b"old1"); // .001→.002
        assert_eq!(std::fs::read(dir.join("DISK2.001")).unwrap(), b"new"); // .LOG→.001
        assert!(!log.exists()); // .LOG queda libre para el llamador
        std::fs::write(&log, b"nuevo-log").unwrap(); // lo que hace main tras rotar
        assert_eq!(std::fs::read(&log).unwrap(), b"nuevo-log");
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Rotación cascada de la cadena de logs (pre-borrado del más antiguo):
/// borra `stem.00{keep}`, renombra `.00(keep-1)`→`.00keep` … `.LOG`→`.001`.
/// El llamador escribe después el nuevo `.LOG`. `keep` = nº de rotados (5).
pub fn rotate_log_chain(log_path: &std::path::Path, keep: usize) -> std::io::Result<()> {
    let base = log_path.to_path_buf();
    let stem = base
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            let n = n.trim_end_matches(".LOG");
            n.trim_end_matches(".log")
        })
        .unwrap_or("DISK0")
        .to_string();
    let dir = base.parent().unwrap_or(std::path::Path::new("."));

    // 1. borrar el más antiguo (.005 con keep=5).
    let oldest = dir.join(format!("{}.00{}", stem, keep));
    let _ = std::fs::remove_file(oldest);

    // 2. cascada .00(keep-1)→.00keep … .LOG→.001 (nombres de 3 dígitos).
    for i in (1..keep).rev() {
        let from = dir.join(format!("{}.00{}", stem, i));
        let to = dir.join(format!("{}.00{}", stem, i + 1));
        if from.exists() {
            std::fs::rename(&from, &to)?;
        }
    }
    if base.exists() {
        std::fs::rename(&base, dir.join(format!("{}.001", stem)))?;
    }
    Ok(())
}

/// Garantiza el directorio padre y escribe el log renderizado (compartida por
/// CLI y menú interactivo).
pub fn write_log(path: &std::path::Path, hlog: &DiskLog) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    std::fs::write(path, hlog.render())?;
    Ok(())
}
