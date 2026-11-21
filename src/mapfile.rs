// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Mapfile binario propio de regrow para el rescate disco→imagen.
//!
//! Guarda el estado por rangos de sectores (`+` bueno, `-` fallido sin
//! reintentar, `?` fallido ya reintentado) más el número de reintentos de
//! cada rango. A diferencia del checkpoint lineal de las pasadas de
//! escaneo, este mapa permite reanudar de forma NO lineal: la siguiente
//! pasada salta todo lo bueno y vuelve solo sobre lo pendiente.
//!
//! Propiedades deliberadas:
//! - Escritura atómica (temporal + `fsync` + rename): un corte a mitad de
//!   guardado jamás corrompe el mapa, y jamás deja un `.tmp` engañoso.
//! - CRC32 al final del archivo: un mapa truncado o alterado se rechaza en
//!   la carga, no se "arregla" a medias.
//! - Compatibilidad de ida con el mapfile de texto de ddrescue
//!   (`import_text`/`export_text`): los formatos son datos, y permitir
//!   continuar un rescate ajeno es la razón de existir del mapa. Unidades y
//!   leyenda verificadas contra el manual y código de ddrescue 1.29:
//!   posiciones en **bytes** (`0x…`, alineadas a 512) y estados `+` terminado,
//!   `-` sector malo, `?` sin intentar, `*` recortado fallido, `/` rascado
//!   fallido (leyenda vigente desde ddrescue 1.23; el export de regrow la
//!   respeta: Good→`+`, Failed→`?`, Scraped→`/`). El binario propio usa su
//!   propia leyenda (`+ - ?`) y unidades de sectores.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub const MAP_MAGIC: &[u8; 8] = b"RGROWMP1";
pub const MAP_VERSION: u32 = 1;
const SECTOR_BYTES: u64 = crate::source::SECTOR_SIZE as u64;

/// Estado de un rango del dominio.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapStatus {
    /// `+` leído y copiado con verificación.
    Good,
    /// `-` fallido, aún sin reintentar (incluye "sin intentar").
    Failed,
    /// `?` fallido después de agotar los reintentos (en ddrescue: `/`).
    Scraped,
}

impl MapStatus {
    /// Leyenda del binario propio (RGROWMP1).
    pub fn char(self) -> char {
        match self {
            MapStatus::Good => '+',
            MapStatus::Failed => '-',
            MapStatus::Scraped => '?',
        }
    }

    pub fn from_char(c: char) -> Option<MapStatus> {
        match c {
            '+' => Some(MapStatus::Good),
            '-' => Some(MapStatus::Failed),
            '?' => Some(MapStatus::Scraped),
            _ => None,
        }
    }

    /// Leyenda real de ddrescue 1.23+ (verificada en su manual 1.29):
    /// `+` terminado, `-` sector malo, `?` sin intentar, `*` recortado
    /// fallido, `/` rascado fallido. Lo pendiente para nosotros (`? * -`)
    /// entra como Failed; `/` ya fue rascado: Scraped.
    pub fn from_ddrescue_char(c: char) -> Option<MapStatus> {
        match c {
            '+' => Some(MapStatus::Good),
            '-' | '?' | '*' => Some(MapStatus::Failed),
            '/' => Some(MapStatus::Scraped),
            _ => None,
        }
    }

    /// Cómo exportamos cada estado para que ddrescue lo entienda sin sorpresas.
    fn ddrescue_char(self) -> char {
        match self {
            MapStatus::Good => '+',
            MapStatus::Failed => '?',  // ddrescue lo vuelve a intentar
            MapStatus::Scraped => '/', // ddrescue lo vuelve a rascar
        }
    }

    fn byte(self) -> u8 {
        self.char() as u8
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MapEntry {
    pub start: u64,
    pub len: u64,
    pub status: MapStatus,
    /// Reintentos fallidos acumulados en este rango (0..255).
    pub retries: u8,
}

/// Mapa de dominio [0, sector_count). Solo se almacenan rangos con estado
/// conocido; un hueco entre rangos equivale a `Failed` sin reintentar.
#[derive(Debug)]
pub struct MapFile {
    pub sector_count: u64,
    entries: Vec<MapEntry>,
}

const ENTRY_BYTES: usize = 20; // start u64 + len u64 + status u8 + retries u8 + 2 reservado

impl MapFile {
    pub fn new(sector_count: u64) -> Self {
        Self { sector_count, entries: Vec::new() }
    }

    pub fn entries(&self) -> &[MapEntry] {
        &self.entries
    }

    /// Reintentos acumulados en el rango que contiene `sector` (0 si el
    /// sector está sin mapear). Lo usan las pasadas quirúrgicas para anotar
    /// cuántas veces un tramo pendiente ha fracasado ya.
    pub fn retries_at(&self, sector: u64) -> u8 {
        self.entries
            .iter()
            .find(|e| e.start <= sector && sector < e.start + e.len)
            .map(|e| e.retries)
            .unwrap_or(0)
    }

    /// Marca [start, start+len) con `status`, partiendo los rangos existentes
    /// que la nueva operación cubre solo a medias, y fusionando vecinos
    /// idénticos. `retries` se fija para el rango resultante.
    pub fn set(&mut self, start: u64, len: u64, status: MapStatus, retries: u8) {
        if len == 0 {
            return;
        }
        let a = start.min(self.sector_count);
        let b = (start + len).min(self.sector_count);
        if a >= b {
            return;
        }
        let mut out: Vec<MapEntry> = Vec::with_capacity(self.entries.len() + 2);
        for e in &self.entries {
            let ea = e.start;
            let eb = e.start + e.len;
            if eb <= a || ea >= b {
                out.push(*e);
                continue;
            }
            // Solapa: conservar los trozos fuera de [a,b).
            if ea < a {
                out.push(MapEntry { start: ea, len: a - ea, status: e.status, retries: e.retries });
            }
            if eb > b {
                out.push(MapEntry { start: b, len: eb - b, status: e.status, retries: e.retries });
            }
        }
        out.push(MapEntry { start: a, len: b - a, status, retries });
        out.sort_by_key(|e| e.start);
        // Fusionar vecinos contiguos con estado y reintentos iguales.
        let mut merged: Vec<MapEntry> = Vec::with_capacity(out.len());
        for e in out {
            match merged.last_mut() {
                Some(prev)
                    if prev.status == e.status
                        && prev.retries == e.retries
                        && prev.start + prev.len == e.start =>
                {
                    prev.len += e.len;
                }
                _ => merged.push(e),
            }
        }
        self.entries = merged;
    }

    /// Rango pendiente (hueco o con alguno de `wanted`) que empieza en o
    /// después de `from`, limitado a `to`. Devuelve el primer hallazgo.
    /// Un hueco cuenta como pendiente solo si se pide `Failed`; `Good` nunca
    /// es pendiente.
    pub fn next_pending(&self, from: u64, to: u64, wanted: &[MapStatus]) -> Option<(u64, u64)> {
        let mut pos = from.min(to);
        for e in &self.entries {
            if e.start + e.len <= pos {
                continue;
            }
            if e.start >= to {
                break;
            }
            if e.start > pos {
                // Hueco [pos, e.start): pendiente por defecto.
                if wanted.contains(&MapStatus::Failed) {
                    return Some((pos, e.start - pos));
                }
                pos = e.start;
            }
            if e.status != MapStatus::Good && wanted.contains(&e.status) {
                return Some((pos.max(e.start), e.start + e.len - pos.max(e.start)));
            }
            pos = (e.start + e.len).max(pos);
            if pos >= to {
                break;
            }
        }
        if pos < to && wanted.contains(&MapStatus::Failed) {
            return Some((pos, to - pos));
        }
        None
    }

    /// Todos los rangos pendientes en [from, to) con los estados dados
    /// (los huecos cuentan como `Failed`).
    pub fn pending_ranges(&self, from: u64, to: u64, wanted: &[MapStatus]) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut pos = from;
        while let Some((a, len)) = self.next_pending(pos, to, wanted) {
            if len == 0 {
                break;
            }
            out.push((a, len));
            pos = a + len;
        }
        out
    }

    /// `--requeue`: los tramos agotados (`Scraped`, el `/` de ddrescue)
    /// vuelven a pendientes (`Failed`, reintentos a cero) para que el
    /// algoritmo completo les dé otra pasada entera — útil cuando el medio
    /// cambió desde la última corrida (p. ej. tras un remap). Devuelve
    /// cuántos sectores volvieron a pendientes.
    pub fn requeue_scraped(&mut self) -> u64 {
        let mut n = 0u64;
        for e in &mut self.entries {
            if e.status == MapStatus::Scraped {
                e.status = MapStatus::Failed;
                e.retries = 0;
                n += e.len;
            }
        }
        n
    }

    /// Conteos por estado y total de sectores aún sin mapear.
    pub fn stats(&self) -> (u64, u64, u64, u64) {
        let (mut good, mut failed, mut scraped) = (0u64, 0u64, 0u64);
        for e in &self.entries {
            match e.status {
                MapStatus::Good => good += e.len,
                MapStatus::Failed => failed += e.len,
                MapStatus::Scraped => scraped += e.len,
            }
        }
        let untried = self.sector_count.saturating_sub(good + failed + scraped);
        (good, failed, scraped, untried)
    }

    // ------------------------------------------------ persistente binario ----

    fn serialize(&self) -> Vec<u8> {
        let mut b: Vec<u8> = Vec::with_capacity(24 + self.entries.len() * ENTRY_BYTES + 4);
        b.extend_from_slice(MAP_MAGIC);
        b.extend_from_slice(&MAP_VERSION.to_le_bytes());
        b.extend_from_slice(&self.sector_count.to_le_bytes());
        b.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            b.extend_from_slice(&e.start.to_le_bytes());
            b.extend_from_slice(&e.len.to_le_bytes());
            b.push(e.status.byte());
            b.push(e.retries);
            b.extend_from_slice(&[0u8; 2]); // reservado, alineado
        }
        let crc = crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// Guardado atómico: temporal en el mismo directorio + fsync + rename.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let tmp = tmp_sibling(path);
        {
            let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
            f.write_all(&self.serialize())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<MapFile, String> {
        let bytes = fs::read(path).map_err(|e| format!("no se puede leer {}: {e}", path.display()))?;
        if bytes.len() < 28 {
            return Err("mapfile truncado (cabecera incompleta)".into());
        }
        if &bytes[0..8] != MAP_MAGIC {
            return Err("no es un mapfile de regrow (cabecera desconocida)".into());
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != MAP_VERSION {
            return Err(format!(
                "versión de mapfile no soportada: {version} (regrow maneja la v{MAP_VERSION})"
            ));
        }
        let body_crc_end = bytes.len() - 4;
        let stored = u32::from_le_bytes(bytes[body_crc_end..].try_into().unwrap());
        if crc32(&bytes[..body_crc_end]) != stored {
            return Err("mapfile corrupto: el CRC no cuadra (archivo truncado o alterado)".into());
        }
        let sector_count = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
        let n = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
        if 24 + n * ENTRY_BYTES + 4 != bytes.len() {
            return Err("mapfile corrupto: tamaño de entradas inconsistentes".into());
        }
        let mut entries = Vec::with_capacity(n);
        let mut prev_end = 0u64;
        for i in 0..n {
            let o = 24 + i * ENTRY_BYTES;
            let start = u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
            let len = u64::from_le_bytes(bytes[o + 8..o + 16].try_into().unwrap());
            let status = MapStatus::from_char(bytes[o + 16] as char)
                .ok_or_else(|| format!("mapfile corrupto: estado '{}' inválido", bytes[o + 16] as char))?;
            let retries = bytes[o + 17];
            if len == 0 || start >= sector_count || start + len > sector_count {
                return Err("mapfile corrupto: rango fuera del dominio".into());
            }
            if start < prev_end {
                return Err("mapfile corrupto: rangos solapados".into());
            }
            prev_end = start + len;
            entries.push(MapEntry { start, len, status, retries });
        }
        Ok(MapFile { sector_count, entries })
    }

    // ---------------------------------------------------- texto ddrescue ----

    /// Exporta al formato de texto de ddrescue: posiciones en BYTES (`0x…`)
    /// y leyenda 1.23+ (`+`/`?`//`/`), de modo que ddrescue y ddrescuelog
    /// pueden leer el mapa sin reinterpretar nada. Los reintentos quedan solo
    /// en el binario propio.
    pub fn export_text(&self, path: &Path) -> io::Result<()> {
        let mut s = String::new();
        s.push_str("# Mapfile. Created by regrow\n");
        s.push_str("#      pos        size  status\n");
        for e in &self.entries {
            s.push_str(&format!(
                "0x{:08x}  0x{:08x}  {}\n",
                e.start * SECTOR_BYTES,
                e.len * SECTOR_BYTES,
                e.status.ddrescue_char()
            ));
        }
        fs::write(path, s.as_bytes())
    }

    /// Importa un mapfile de texto. El dominio real lo fija el origen
    /// (`sector_count`), no el texto importado. Acepta:
    /// - ddrescue 1.20+: posiciones en bytes (hex `0x…`, alineadas a 512) y
    ///   leyenda `+ - ? * /`; ignora comentarios (incluso tras espacio), la
    ///   línea de estado y cualquier fila malformada.
    /// - Exportaciones propias antiguas (v0.1.0): sectores con cabecera
    ///   `# domain: … sectores de 512 B`, detectadas por esa marca.
    pub fn import_text(path: &Path, sector_count: u64) -> Result<MapFile, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("no se puede leer {}: {e}", path.display()))?;
        let legacy_sectors = text
            .lines()
            .any(|l| l.starts_with("# domain:") && l.contains("sectores de 512 B"));
        let mut mf = MapFile::new(sector_count);
        let mut loaded = 0usize;
        for line in text.lines() {
            // En ddrescue un '#' empieza comentario al inicio o a media línea.
            let line = match line.split_once('#') {
                Some((before, _)) => before,
                None => line,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() != 3 {
                continue; // p. ej. la línea de estado `0x03730600 / 1`
            }
            let Ok(pos) = parse_hex(cols[0]) else { continue };
            let Ok(size) = parse_hex(cols[1]) else { continue };
            let (start, len) = if legacy_sectors {
                (pos, size)
            } else {
                // ddrescue escribe bytes; en disco siempre múltiplos de 512.
                if pos % SECTOR_BYTES != 0 || size % SECTOR_BYTES != 0 {
                    continue;
                }
                (pos / SECTOR_BYTES, size / SECTOR_BYTES)
            };
            let first = cols[2].chars().next().unwrap_or(' ');
            let parsed = if legacy_sectors {
                MapStatus::from_char(first)
            } else {
                MapStatus::from_ddrescue_char(first)
            };
            let Some(status) = parsed else { continue };
            if len == 0 || start >= sector_count {
                continue;
            }
            let len = len.min(sector_count - start);
            mf.set(start, len, status, 0);
            loaded += 1;
        }
        if loaded == 0 {
            return Err("mapfile de texto sin rangos utilizables".into());
        }
        Ok(mf)
    }
}

fn parse_hex(s: &str) -> Result<u64, String> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if h.is_empty() || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("hex inválido '{s}'"));
    }
    u64::from_str_radix(h, 16).map_err(|_| format!("hex fuera de rango '{s}'"))
}

fn tmp_sibling(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "map".to_string());
    name.push_str(".tmp");
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(name)
}

/// CRC32 (polinomio 0xEDB88320) sin tablas: suficiente para archivos de
/// unas pocas MB y sin dependencias.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = crc & 1;
            crc >>= 1;
            if mask != 0 {
                crc ^= 0xEDB8_8320;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("regrow_map_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn set_parte_y_fusiona_rangos() {
        let mut m = MapFile::new(1000);
        m.set(0, 100, MapStatus::Good, 0);
        m.set(50, 10, MapStatus::Failed, 0);
        let e = m.entries();
        assert_eq!(e.len(), 3);
        assert_eq!((e[0].start, e[0].len, e[0].status), (0, 50, MapStatus::Good));
        assert_eq!((e[1].start, e[1].len, e[1].status), (50, 10, MapStatus::Failed));
        assert_eq!((e[2].start, e[2].len, e[2].status), (60, 40, MapStatus::Good));
        // Vecinos idénticos se fusionan.
        m.set(50, 10, MapStatus::Good, 0);
        assert_eq!(m.entries().len(), 1);
        assert_eq!(m.entries()[0].len, 100);
    }

    #[test]
    fn requeue_devuelve_agotados_a_pendientes() {
        let mut m = MapFile::new(1000);
        m.set(0, 100, MapStatus::Good, 0);
        m.set(100, 1, MapStatus::Scraped, 3);
        m.set(500, 4, MapStatus::Scraped, 2);
        assert_eq!(m.requeue_scraped(), 5);
        let (_, failed, scraped, _) = m.stats();
        assert_eq!((failed, scraped), (5, 0));
        for e in m.entries().iter().filter(|e| e.status == MapStatus::Failed) {
            assert_eq!(e.retries, 0, "el reintento se reinicia al reencolar");
        }
        // Lo bueno no se toca.
        assert_eq!(m.stats().0, 100);
    }

    #[test]
    fn huecos_cuentan_como_pendientes() {
        let mut m = MapFile::new(1000);
        m.set(100, 100, MapStatus::Good, 0);
        let r = m.pending_ranges(0, 1000, &[MapStatus::Failed]);
        assert_eq!(r, vec![(0, 100), (200, 800)]);
        // Solo Good pedido: ningún pendiente.
        assert!(m.pending_ranges(0, 1000, &[MapStatus::Good]).is_empty());
    }

    #[test]
    fn next_pending_no_lineal() {
        let mut m = MapFile::new(1000);
        m.set(0, 400, MapStatus::Good, 0);
        m.set(400, 50, MapStatus::Scraped, 3);
        m.set(450, 550, MapStatus::Good, 0);
        let p = m.next_pending(0, 1000, &[MapStatus::Scraped, MapStatus::Failed]).unwrap();
        assert_eq!(p, (400, 50));
    }

    #[test]
    fn stats_separan_estados_y_huecos() {
        let mut m = MapFile::new(1000);
        m.set(0, 100, MapStatus::Good, 0);
        m.set(100, 20, MapStatus::Scraped, 2);
        let (g, f, s, u) = m.stats();
        assert_eq!((g, f, s, u), (100, 0, 20, 880));
    }

    #[test]
    fn roundtrip_binario_y_crc_detecta_corrupcion() {
        let dir = tmpdir("rt");
        let p = dir.join("a.map");
        let mut m = MapFile::new(5000);
        m.set(10, 90, MapStatus::Good, 0);
        m.set(100, 5, MapStatus::Scraped, 3);
        m.save(&p).unwrap();
        assert!(!tmp_sibling(&p).exists(), "no debe quedar residuo .tmp");
        let loaded = MapFile::load(&p).unwrap();
        assert_eq!(loaded.sector_count, 5000);
        assert_eq!(loaded.entries(), m.entries());

        // Corromper un byte del cuerpo ⇒ carga rechazada.
        let mut bytes = fs::read(&p).unwrap();
        bytes[26] ^= 0xFF;
        fs::write(&p, &bytes).unwrap();
        let err = MapFile::load(&p).unwrap_err();
        assert!(err.contains("CRC"), "{err}");

        // Versión futura ⇒ rechazo explícito, no lectura a medias.
        let mut bytes = fs::read(dir.join("x.map")).unwrap_or_default();
        if bytes.is_empty() {
            let mut m2 = MapFile::new(1);
            m2.set(0, 1, MapStatus::Good, 0);
            m2.save(&dir.join("x.map")).unwrap();
            bytes = fs::read(dir.join("x.map")).unwrap();
        }
        bytes[8] = 2; // versión 2
        fs::write(&p, &bytes).unwrap();
        let err = MapFile::load(&p).unwrap_err();
        assert!(err.contains("versión"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_import_text_estilo_ddrescue() {
        let dir = tmpdir("txt");
        let mut m = MapFile::new(1 << 20);
        m.set(0, 0x01BE_8C00, MapStatus::Good, 0);
        m.set(0x01BE_8C00, 0x45400, MapStatus::Scraped, 3);
        m.set(0x01C2E000, 0x1000, MapStatus::Good, 0);
        let p = dir.join("map.txt");
        m.export_text(&p).unwrap();
        let imported = MapFile::import_text(&p, 1 << 20).unwrap();
        // El texto no guarda reintentos: mismo estado, retries 0.
        let mut expect = m.entries().to_vec();
        for e in &mut expect {
            e.retries = 0;
        }
        assert_eq!(imported.entries(), &expect[..]);

        // Aceptar el formato real de ddrescue 1.29: bytes alineados, leyenda
        // `+ - ? /`, línea de estado intercalada y comentarios.
        let raw = concat!(
            "# Mapfile. Created by GNU ddrescue version 1.29\n",
            "# Command line: ddrescue -d -a 10485760 /dev/sda5 img map\n",
            "# current_pos  current_status  current_pass\n",
            "0x03730600     /               1\n",
            "#      pos        size  status\n",
            "0x00000000  0x01BE8C00  +\n",
            "0x01BE8C00  0x00045400  -\n",
            "0x01C2F000  0x000C6200  ?\n",
            "0x01CF5200  0x00008000  /  # rascado y sigue malo\n",
        );
        fs::write(&p, raw).unwrap();
        let im = MapFile::import_text(&p, 1 << 30).unwrap();
        assert_eq!(im.entries().len(), 4);
        // Bytes → sectores: 0x01BE8C00 B = 57158 sectores de dominio bueno.
        assert_eq!(im.entries()[0].start, 0);
        assert_eq!(im.entries()[0].len, 0x01BE8C00 / SECTOR_BYTES);
        assert_eq!(im.entries()[1].start, 0x01BE8C00 / SECTOR_BYTES);
        assert_eq!(im.entries()[1].status, MapStatus::Failed);
        // `?` en ddrescue 1.23+ significa SIN INTENTAR: entra como pendiente.
        assert_eq!(im.entries()[2].status, MapStatus::Failed);
        // `/` fue rascado y sigue malo: Scraped.
        assert_eq!(im.entries()[3].status, MapStatus::Scraped);
        assert_eq!(im.entries()[3].len, 0x8000 / SECTOR_BYTES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_format_propio_antiguo_por_sectores() {
        // Exportaciones de regrow 0.1.0 (sectores + leyenda propia): la marca
        // `# domain: … sectores de 512 B` las sigue leyendo bien.
        let dir = tmpdir("legacy");
        let p = dir.join("old.txt");
        fs::write(
            &p,
            concat!(
                "# Mapfile. Created by regrow\n",
                "# domain: 4096 sectores de 512 B\n",
                "#      start        size   #flags\n",
                "0000000000000000 0000000000000400 +\n",
                "0000000000000400 0000000000000100 ?\n",
                "0000000000000500 0000000000000100 -\n",
            ),
        )
        .unwrap();
        let im = MapFile::import_text(&p, 4096).unwrap();
        assert_eq!(im.entries()[1].status, MapStatus::Scraped);
        assert_eq!(im.entries()[1].start, 0x400);
        assert_eq!(im.entries()[2].status, MapStatus::Failed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn crc32_vector_conocido() {
        // Vector canónico: "123456789" -> 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// `retries_at` lee los reintentos del rango que contiene el sector
    /// (0 si está sin mapear): lo usa la pasada quirúrgica para contabilizar
    /// cuántas veces un tramo pendiente ha fracasado.
    #[test]
    fn retries_at_lee_el_rango_contenedor() {
        let mut m = MapFile::new(100);
        m.set(10, 20, MapStatus::Failed, 3);
        m.set(60, 5, MapStatus::Good, 0);
        assert_eq!(m.retries_at(10), 3);
        assert_eq!(m.retries_at(29), 3);
        assert_eq!(m.retries_at(30), 0); // fuera del rango: sin mapear
        assert_eq!(m.retries_at(60), 0);
        assert_eq!(m.retries_at(999), 0);
    }
}
