// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Abstracción de origen de sectores crudos (regrow).
//!
//! regrow define su propio trait de acceso a sectores con backends por SO,
//! sin crates externos (std enlaza libc/kernel32 y las funciones necesarias
//! se declaran con `extern`):
//! - `FileImage`: imagen de archivo (validación forense y uso sin hardware).
//! - `RawDisk`: `/dev/rdiskN` (macOS), `/dev/sdX`/`/dev/nvme*` (Linux),
//!   `\\.\PhysicalDriveN` (Windows). Requiere root/administrador.
//!
//! Endurecimiento del acceso crudo:
//! - Linux: `O_SYNC` en escrituras (los reintentos llegan al medio, no al
//!   page cache) y sondeo `O_DIRECT` con fallback.
//! - macOS: `fcntl(F_NOCACHE)` tras abrir (bypass de la caché de unify/buffer).
//! - Windows: `FILE_FLAG_WRITE_THROUGH` + `FILE_FLAG_NO_BUFFERING` con búfer
//!   bulk alineado propio ; si el probe del NO_BUFFERING falla, se
//!   reabre en modo búfer.
//! - 4Kn : la gramática de registros es SIEMPRE de 512 B lógicos; un
//!   medio con bloques físicos de 4096 B se accede por bloque completo
//!   alineado, y las escrituras usan lectura-modificación-escritura del
//!   bloque físico para no destruir los sectores vecinos. Si el bloque
//!   físico está ilegible se RECHAZA la escritura (incluso con
//!   `--allow-data-loss`): destruiría vecinos legibles.
//! - Tipo de medio : `RawDisk::open` detecta
//!   HDD/SSD/desconocido (`/sys/block/…/rotational`, `diskutil info -plist`,
//!   `IOCTL_STORAGE_QUERY_PROPERTY`) y lo expone vía `media()`.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SECTOR_SIZE: usize = 512;

/// Tipo de medio físico . Conservador: solo `Ssd`/`Hdd` con
/// evidencia; todo lo demás es `Unknown` (informar/confirmar, no bloquear).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKind {
    Hdd,
    Ssd,
    Unknown,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Hdd => "HDD mecánico",
            MediaKind::Ssd => "SSD/NVMe sólido",
            MediaKind::Unknown => "medio desconocido",
        }
    }
}

pub trait SectorSource {
    /// Número total de sectores accesibles (gramática lógica de 512 B).
    fn sector_count(&self) -> u64;

    /// Tamaño de bloque físico del medio (512; 4Kn ⇒ 4096).
    fn phys_block(&self) -> usize {
        SECTOR_SIZE
    }

    /// Tipo de medio detectado (Unknown si no se pudo confirmar).
    fn media(&self) -> MediaKind {
        MediaKind::Unknown
    }

    /// Lee un sector LBA completo (512 B). Error ⇒ sector ilegible.
    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()>;

    /// Escribe un sector LBA completo (el mecanismo de regeneración:
    /// reescritura que fuerza reasignación/remapeo en discos reales).
    /// En 4Kn es lectura-modificación-escritura del bloque físico.
    fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_SIZE]) -> io::Result<()>;

    /// Lectura bulk de `count` sectores contiguos en `buf` (longitud
    /// count×512). Implementación por defecto: sector a sector; los backends
    /// la sobrescriben con una sola llamada cuando es seguro.
    fn read_chunk(&mut self, lba: u64, count: usize, buf: &mut [u8]) -> io::Result<()> {
        debug_assert_eq!(buf.len(), count * SECTOR_SIZE);
        let mut tmp = [0u8; SECTOR_SIZE];
        for i in 0..count {
            self.read_sector(lba + i as u64, &mut tmp)?;
            buf[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE].copy_from_slice(&tmp);
        }
        Ok(())
    }
}

/// Backend de imagen de archivo (lectura+escritura sobre un .raw).
pub struct FileImage {
    file: File,
    sectors: u64,
}

impl FileImage {
    pub fn open_rw(path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            sectors: len / SECTOR_SIZE as u64,
        })
    }

    /// Abre en modo solo lectura (modo scan no destructivo).
    pub fn open_ro(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            sectors: len / SECTOR_SIZE as u64,
        })
    }

    fn check(&self, lba: u64) -> io::Result<()> {
        if lba >= self.sectors {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("LBA {} fuera de rango ({} sectores)", lba, self.sectors),
            ));
        }
        Ok(())
    }
}

impl SectorSource for FileImage {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
        self.check(lba)?;
        self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
        // read_exact maneja lecturas parciales.
        self.file.read_exact(buf)
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
        self.check(lba)?;
        self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
        self.file.write_all(buf)
    }

    fn read_chunk(&mut self, lba: u64, count: usize, buf: &mut [u8]) -> io::Result<()> {
        self.check(lba)?;
        let need = count * SECTOR_SIZE;
        if lba as u64 + count as u64 > self.sectors || buf.len() < need {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("chunk LBA {}×{} fuera de rango", lba, count),
            ));
        }
        self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
        self.file.read_exact(&mut buf[..need])
    }
}

/// Búfer bulk alineado a 4096 B: satisface a la vez `O_DIRECT` (Linux),
/// `FILE_FLAG_NO_BUFFERING` (Windows, alineación a tamaño de sector) y la
/// lectura por bloques físicos en medios 4Kn.
const BULK_BYTES: usize = 128 * 1024;

#[repr(align(4096))]
struct AlignedBulk([u8; BULK_BYTES]);

/// Bloques físicos de un rango lógico [lba, lba+count) sobre bloques de `phys`.
/// Devuelve (primer bloque, nº de bloques).
fn phys_range_blocks(lba: u64, count: usize, phys: usize) -> (u64, u64) {
    let byte_lo = lba * SECTOR_SIZE as u64;
    let byte_hi = byte_lo + (count * SECTOR_SIZE) as u64;
    let blk_lo = byte_lo / phys as u64;
    let blk_hi = (byte_hi + phys as u64 - 1) / phys as u64;
    (blk_lo, blk_hi - blk_lo)
}

/// Backend de disco físico crudo. Equivale al acceso LBA del BIOS, pero vía el
/// nodo de dispositivo del SO. `write=true` es DESTRUCTIVO.
pub struct RawDisk {
    file: File,
    sectors: u64,
    bulk: Box<AlignedBulk>,
    /// Tamaño de bloque físico reportado por el medio (512|1024|2048|4096).
    phys: usize,
    /// true ⇒ toda lectura pasa por `bulk` (O_DIRECT/NO_BUFFERING/4Kn exigen
    /// alineación); false ⇒ lectura directa al búfer del llamador.
    bulk_stage: bool,
    media: MediaKind,
    /// true si el nodo NO es un disco físico entero (p. ej. contenedor APFS
    /// sintetizado en macOS). Tales nodos se excluyen de la enumeración.
    synthesized: bool,
}

impl RawDisk {
    /// Ruta nativa de la unidad física N según el SO.
    /// En Linux la convención `sdX` es heurística: para `nvme*`/`mmcblk*`
    /// usar `--dev` con la ruta explícita (o el enumerador `list_native_disks`).
    pub fn native_name(n: u32) -> String {
        #[cfg(target_os = "macos")]
        let name = format!("/dev/rdisk{n}");
        #[cfg(target_os = "linux")]
        let name = {
            let letter = (b'a' + n.min(25) as u8) as char;
            format!("/dev/sd{letter}")
        };
        #[cfg(windows)]
        let name = format!("\\\\.\\PhysicalDrive{n}");
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        let name = format!("/dev/disk{n}");
        name
    }

    /// Abre el dispositivo crudo. `write=true` para repair/regenerate.
    /// Acepta bloques físicos 512/1024/2048/4096 (4Kn con
    /// lectura-modificación-escritura). En Linux intenta `O_DIRECT` (bypass
    /// del page cache) y en Windows `FILE_FLAG_NO_BUFFERING`; si el probe
    /// falla, se reabre sin la bandera estricta.
    pub fn open(path: &Path, write: bool) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            match Self::open_with(path, write, true) {
                Ok(mut d) => match d.probe() {
                    Ok(()) => Ok(d),
                    Err(e) if e.raw_os_error() == Some(22) => {
                        // EINVAL por O_DIRECT: reintentar con caché normal.
                        Self::open_with(path, write, false)
                    }
                    // Otro error del probe (p. ej. sector 0 ilegible) no es
                    // motivo de reabrir: el dispositivo es usable.
                    Err(_) => Ok(d),
                },
                Err(e) if e.raw_os_error() == Some(22) => Self::open_with(path, write, false),
                Err(e) => Err(e),
            }
        }
        #[cfg(windows)]
        {
            // NO_BUFFERING exige búfer/offset/longitud alineados al sector:
            // `bulk` los garantiza; si aun así el probe falla, reabrir con
            // búfer normal (el medio o el puente USB lo rechaza).
            match Self::open_with(path, write, true) {
                Ok(mut d) => match d.probe() {
                    Ok(()) => Ok(d),
                    Err(_) => Self::open_with(path, write, false),
                },
                Err(e) => Err(e),
            }
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        Self::open_with(path, write, false)
    }

    /// `strict` = O_DIRECT (Linux) / NO_BUFFERING (Windows).
    fn open_with(path: &Path, write: bool, strict: bool) -> io::Result<Self> {
        #[cfg(unix)]
        let file = {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true);
            if write {
                opts.write(true);
            }
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::OpenOptionsExt;
                let mut flags = 0i32;
                if strict {
                    flags |= 0o00040000; // O_DIRECT: bypass del page cache
                }
                if write {
                    flags |= 0o0004000; // O_SYNC: las reescrituras llegan al medio
                }
                if flags != 0 {
                    opts.custom_flags(flags);
                }
            }
            opts.open(path)?
        };
        #[cfg(windows)]
        let file = windows_open(path, write, strict)?;

        let bytes = device_size(&file, path)?;
        if bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} tiene tamaño 0 (¿no es un disco crudo?)", path.display()),
            ));
        }
        if bytes < SECTOR_SIZE as u64 {
            // Menos de un sector: el tamaño reportado no es fiable (ciertos
            // nodos sintéticos/puentes devuelven capacidades absurdas).
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} reporta solo {} B (< 1 sector de 512 B): tamaño no fiable",
                    path.display(),
                    bytes
                ),
            ));
        }

        // Bloque físico aceptado: múltiplos de potencia de dos de 512 hasta
        // 4096. Formatos extraños (p. ej. 4160 B con protección de integridad)
        // se rechazan con mensaje claro.
        let phys = device_sector_size(&file, path)?;
        if !(phys == 512 || phys == 1024 || phys == 2048 || phys == 4096) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} usa bloques físicos de {} B (no soportado: se aceptan 512/1024/2048/4096)",
                    path.display(),
                    phys
                ),
            ));
        }

        // macOS: bypass de la caché de búfer (F_NOCACHE) tras abrir.
        // 48 según sys/fcntl.h actual (históricamente 64 en SDKs antiguos).
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                fcntl(file.as_raw_fd(), 48 /* F_NOCACHE */, 1);
            }
        }

        // La gramática de registros es de 512 B lógicos, también en 4Kn.
        let (media, synthesized) = detect_media(&file, path);
        Ok(Self {
            file,
            sectors: bytes / SECTOR_SIZE as u64,
            bulk: Box::new(AlignedBulk([0u8; BULK_BYTES])),
            phys,
            bulk_stage: strict || phys != SECTOR_SIZE,
            media,
            synthesized,
        })
    }

    /// Lee el bloque físico 0 con el búfer alineado para validar el modo de
    /// acceso estricto (Linux O_DIRECT / Windows NO_BUFFERING).
    #[cfg(any(target_os = "linux", windows))]
    fn probe(&mut self) -> io::Result<()> {
        self.file.rewind()?;
        let phys = self.phys;
        let mut got = 0usize;
        while got < phys {
            let n = self.file.read(&mut self.bulk.0[got..phys])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "probe: lectura corta en el bloque 0",
                ));
            }
            got += n;
        }
        Ok(())
    }

    /// Bloque físico confirmado (512|1024|2048|4096).
    #[allow(dead_code)]
    pub fn phys(&self) -> usize {
        self.phys
    }

    /// Tipo de medio detectado al abrir .
    pub fn media(&self) -> MediaKind {
        self.media
    }

    /// true si el nodo es sintetizado (no un medio físico).
    #[allow(dead_code)] // lo consume solo la enumeración de macOS
    pub fn synthesized(&self) -> bool {
        self.synthesized
    }
}

impl SectorSource for RawDisk {
    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn phys_block(&self) -> usize {
        self.phys
    }

    fn media(&self) -> MediaKind {
        self.media
    }

    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
        if lba >= self.sectors {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("LBA {} fuera de rango ({} sectores)", lba, self.sectors),
            ));
        }
        if self.phys == SECTOR_SIZE {
            self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
            // read_exact sobre el búfer alineado (strict exige longitud exacta).
            self.file.read_exact(&mut self.bulk.0[..SECTOR_SIZE])?;
            buf.copy_from_slice(&self.bulk.0[..SECTOR_SIZE]);
            Ok(())
        } else {
            // 4Kn: leer el bloque físico completo y extraer el sector lógico.
            let byte = lba * SECTOR_SIZE as u64;
            let blk = byte / self.phys as u64;
            let off = (byte % self.phys as u64) as usize;
            self.file.seek(SeekFrom::Start(blk * self.phys as u64))?;
            self.file.read_exact(&mut self.bulk.0[..self.phys])?;
            buf.copy_from_slice(&self.bulk.0[off..off + SECTOR_SIZE]);
            Ok(())
        }
    }

    fn write_sector(&mut self, lba: u64, buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
        if lba >= self.sectors {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("LBA {} fuera de rango ({} sectores)", lba, self.sectors),
            ));
        }
        if self.phys == SECTOR_SIZE {
            self.bulk.0[..SECTOR_SIZE].copy_from_slice(buf);
            self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
            self.file.write_all(&self.bulk.0[..SECTOR_SIZE])
        } else {
            // 4Kn: lectura-modificación-escritura del bloque físico: los
            // sectores lógicos vecinos se preservan. Si el bloque está
            // ilegible se rechaza la escritura (destruiría vecinos legibles).
            let byte = lba * SECTOR_SIZE as u64;
            let blk = byte / self.phys as u64;
            let off = (byte % self.phys as u64) as usize;
            self.file.seek(SeekFrom::Start(blk * self.phys as u64))?;
            self.file.read_exact(&mut self.bulk.0[..self.phys])?;
            self.bulk.0[off..off + SECTOR_SIZE].copy_from_slice(buf);
            self.file.seek(SeekFrom::Start(blk * self.phys as u64))?;
            self.file.write_all(&self.bulk.0[..self.phys])
        }
    }

    /// Bloque bulk. En modo estricto (O_DIRECT / NO_BUFFERING / 4Kn) toda la
    /// lectura pasa por `bulk` (búfer, offset y longitud alineados al bloque
    /// físico) y se copia al búfer del llamador; en modo búfer se lee directo.
    fn read_chunk(&mut self, lba: u64, count: usize, buf: &mut [u8]) -> io::Result<()> {
        let need = count * SECTOR_SIZE;
        if lba >= self.sectors || lba as u64 + count as u64 > self.sectors || buf.len() < need {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("chunk LBA {}×{} fuera de rango", lba, count),
            ));
        }
        if !self.bulk_stage {
            self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
            return self.file.read_exact(&mut buf[..need]);
        }
        let phys = self.phys;
        let per_iter = BULK_BYTES / phys; // bloques físicos por carga
        let (blk_lo, nblocks) = phys_range_blocks(lba, count, phys);
        let byte_lo = lba * SECTOR_SIZE as u64;
        let byte_hi = byte_lo + need as u64;
        let mut blk = blk_lo;
        while blk < blk_lo + nblocks {
            let m = per_iter.min((blk_lo + nblocks - blk) as usize);
            self.file.seek(SeekFrom::Start(blk * phys as u64))?;
            self.file.read_exact(&mut self.bulk.0[..m * phys])?;
            // Copiar los sectores lógicos cubiertos por esta carga (los
            // bordes del rango pueden compartir bloque con datos vecinos).
            let wbase = blk * phys as u64;
            let i0 = wbase.max(byte_lo);
            let i1 = (wbase + (m * phys) as u64).min(byte_hi);
            let mut b = i0;
            while b < i1 {
                let src = (b - wbase) as usize;
                let dst = (b - byte_lo) as usize;
                buf[dst..dst + SECTOR_SIZE].copy_from_slice(&self.bulk.0[src..src + SECTOR_SIZE]);
                b += SECTOR_SIZE as u64;
            }
            blk += m as u64;
        }
        Ok(())
    }
}

impl Drop for RawDisk {
    fn drop(&mut self) {
        // Vaciado de los sectores reescritos antes de cerrar el dispositivo.
        let _ = self.file.sync_all();
    }
}

// ------------------------------------------------- enumeración de unidades ---

/// Unidad crudo nativa descubierta por el enumerador del SO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeDisk {
    pub path: PathBuf,
    pub sectors: u64,
    /// Etiqueta corta para el menú (p. ej. "rdisk1", "sda", "PhysicalDrive0").
    pub label: String,
    pub media: MediaKind,
}

/// Enumera las unidades crudo accesibles (abre cada candidato en solo
/// lectura; los que fallan por permisos o inexistencia se omiten).
pub fn list_native_disks() -> Vec<NativeDisk> {
    let mut out = Vec::new();

    #[cfg(target_os = "macos")]
    {
        for n in 0..16u32 {
            let p = PathBuf::from(format!("/dev/rdisk{n}"));
            if let Ok(d) = RawDisk::open(&p, false) {
                // Descartar discos sintetizados (contenedores APFS): heredan
                // el BusProtocol de su store físico y se disfrazan de unidad.
                if d.synthesized() {
                    continue;
                }
                out.push(NativeDisk {
                    path: p,
                    sectors: d.sector_count(),
                    label: format!("rdisk{n}"),
                    media: d.media(),
                });
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = std::fs::read_dir("/sys/block") {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .filter(|n| {
                    // Solo medios físicos: fuera loop/zram/ram (dispositivos
                    // de bloque sintéticos del kernel).
                    !(n.starts_with("loop") || n.starts_with("zram") || n.starts_with("ram"))
                })
                .collect();
            names.sort();
            for name in names {
                let p = PathBuf::from(format!("/dev/{name}"));
                if let Ok(d) = RawDisk::open(&p, false) {
                    out.push(NativeDisk {
                        path: p,
                        sectors: d.sector_count(),
                        label: name,
                        media: d.media(),
                    });
                }
            }
        }
    }

    #[cfg(windows)]
    {
        for n in 0..16u32 {
            let p = PathBuf::from(format!("\\\\.\\PhysicalDrive{n}"));
            if let Ok(d) = RawDisk::open(&p, false) {
                out.push(NativeDisk {
                    path: p,
                    sectors: d.sector_count(),
                    label: format!("PhysicalDrive{n}"),
                    media: d.media(),
                });
            }
        }
    }

    out
}

// --------------------------------------------------- detección de medio ------
//
// el tipo de medio SIEMPRE se informa; el gate solo
// bloquea `regenerate` en SSD confirmado. Política conservadora: Ssd/Hdd solo
// con evidencia directa; puentes USB caprichosos quedan Unknown (avisar y
// confirmar, nunca bloquear).

/// `MediaKind` a partir de `/sys/block/<dev>/queue/rotational`.
#[allow(dead_code)] // helper puro: puede no usarse en todos los SO
pub(crate) fn media_from_rotational(content: Option<&str>) -> MediaKind {
    match content.map(|s| s.trim()) {
        Some("0") => MediaKind::Ssd,
        Some("1") => MediaKind::Hdd,
        _ => MediaKind::Unknown,
    }
}

/// `MediaKind` a partir del plist de `diskutil info -plist` (macOS).
/// Claves medidas en campo (macOS 15, 2026-11-19): NVMe interno →
/// `BusProtocol: PCI-Express`; USB → `BusProtocol: USB` + `MediaType:
/// Generic` (informativo). macOS antiguos exponían `Medium Type` con valores
/// "Solid State"/"Rotational". En un Mac, todo almacenamiento visible sobre
/// PCI-Express es flash (no existe HDD mecánico de PCIe).
#[allow(dead_code)] // helper puro: puede no usarse en todos los SO
pub(crate) fn media_from_diskutil_plist(text: &str) -> MediaKind {
    if let Some(v) = plist_string(text, "Medium Type") {
        if v.contains("Solid State") {
            return MediaKind::Ssd;
        }
        if v.contains("Rotational") {
            return MediaKind::Hdd;
        }
    }
    if let Some(v) = plist_string(text, "BusProtocol") {
        if v == "PCI-Express" {
            return MediaKind::Ssd;
        }
    }
    MediaKind::Unknown
}

/// Valor `<string>` del plist de Apple que sigue a `<key>name</key>`.
#[allow(dead_code)] // helper puro: puede no usarse en todos los SO
fn plist_string(text: &str, key: &str) -> Option<String> {
    let marker = format!("<key>{key}</key>");
    let at = text.find(&marker)?;
    let rest = &text[at + marker.len()..];
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")?;
    Some(rest[start..start + end].trim().to_string())
}

/// Ruta que entiende `diskutil` para un nodo `/dev/rdiskN` (el alias rdisk es
/// solo de E/S; `diskutil` responde por `diskN`).
#[cfg(target_os = "macos")]
fn diskutil_device_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if let Some(rest) = name.strip_prefix("rdisk") {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("/dev/disk{rest}"));
        }
        return None;
    }
    if name.starts_with("disk") {
        return Some(format!("/dev/{name}"));
    }
    None
}

/// Campos del `STORAGE_DEVICE_DESCRIPTOR` (winioctl.h) que interesan:
/// Version@0 (0x500 clásico; 0x600 = Win8+ añade SeekPenalty tras
/// RawPropertiesLength), BusType@28 (BusTypeNvme = 0x11) y SeekPenalty@36
/// (0 = sin penalación de búsqueda ⇒ sólido; 1 = mecánico; otros = dudoso).
/// La verificación en consola Windows real queda pendiente.
#[allow(dead_code)] // helper puro: puede no usarse en todos los SO
pub(crate) fn media_from_windows_descriptor(buf: &[u8]) -> MediaKind {
    if buf.len() < 32 {
        return MediaKind::Unknown;
    }
    let version = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let bus = u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
    const BUS_NVME: u32 = 0x11;
    if bus == BUS_NVME {
        return MediaKind::Ssd;
    }
    if version >= 0x600 && buf.len() >= 37 {
        match buf[36] {
            0 => return MediaKind::Ssd,
            1 => return MediaKind::Hdd,
            _ => {}
        }
    }
    MediaKind::Unknown
}

#[cfg(target_os = "macos")]
fn detect_media(_file: &File, path: &Path) -> (MediaKind, bool) {
    let Some(disk_path) = diskutil_device_path(path) else {
        return (MediaKind::Unknown, false);
    };
    // Un subproceso barato (~50 ms); si diskutil falla o no aclara ⇒ Unknown.
    match std::process::Command::new("diskutil")
        .args(["info", "-plist", &disk_path])
        .output()
    {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            (media_from_diskutil_plist(&text), diskutil_is_synthesized(&text))
        }
        _ => (MediaKind::Unknown, false),
    }
}

/// Claves exclusivas de los discos **sintetizados** (contenedores APFS): un
/// contenedor hereda el `BusProtocol` de su store físico y podría confundirse
/// con una unidad extra; no es un medio al que se pueda escanear/regenerar.
/// Medido en campo (macOS 15): el contenedor APFS expone `APFSPhysicalStores`
/// y `APFSContainerReference`; los discos físicos (incluidos sus orificios de
/// partición completos) no las tienen.
#[allow(dead_code)] // helper puro: lo consume solo la ruta macOS
pub(crate) fn diskutil_is_synthesized(text: &str) -> bool {
    text.contains("<key>APFSPhysicalStores</key>") || text.contains("<key>APFSContainerReference</key>")
}

#[cfg(target_os = "linux")]
fn detect_media(_file: &File, path: &Path) -> (MediaKind, bool) {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) if !n.is_empty() => n,
        _ => return (MediaKind::Unknown, false),
    };
    let content = std::fs::read_to_string(format!("/sys/block/{name}/queue/rotational")).ok();
    (media_from_rotational(content.as_deref()), false)
}

#[cfg(windows)]
fn detect_media(file: &File, _path: &Path) -> (MediaKind, bool) {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;

    #[repr(C)]
    struct StoragePropertyQuery {
        property_id: u32, // StorageDeviceProperty = 0
        query_id: u32,    // StandardQuery = 0
    }

    extern "system" {
        fn DeviceIoControl(
            h_device: *mut c_void,
            dw_io_control_code: u32,
            lp_in_buffer: *mut c_void,
            n_in_buffer_size: u32,
            lp_out_buffer: *mut c_void,
            n_out_buffer_size: u32,
            lp_bytes_returned: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> i32;
    }

    let mut buf = [0u8; 512];
    let mut written: u32 = 0;
    let q = StoragePropertyQuery {
        property_id: 0,
        query_id: 0,
    };
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            &q as *const StoragePropertyQuery as *mut c_void,
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 && written >= 32 {
        (media_from_windows_descriptor(&buf[..written as usize]), false)
    } else {
        (MediaKind::Unknown, false)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn detect_media(_file: &File, _path: &Path) -> (MediaKind, bool) {
    (MediaKind::Unknown, false)
}

// ------------------------------------------------------------ bajo nivel -----

/// Tamaño en bytes del dispositivo crudo.
#[cfg(unix)]
fn device_size(file: &File, path: &Path) -> io::Result<u64> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();

    // macOS: familia DK (IOBlockStorageDriver), la fiable en nodos de disco
    // completo. El legado DIOCGMEDIASIZE responde tamaños sin sentido en
    // nodos completos (0 en disco sintético; 256 B en un USB real medido
    // 2026-11-18).
    #[cfg(target_os = "macos")]
    {
        let mut count: u64 = 0;
        let rc = unsafe { ioctl(fd, 0x4008_6419, &mut count) }; // DKIOCGETBLOCKCOUNT (_IOR('d',25,uint64))
        if rc == 0 && count > 0 {
            let mut bs: u32 = 0;
            if unsafe { ioctl(fd, 0x4004_6418, &mut bs) } == 0 && bs > 0 {
                // DKIOCGETBLOCKSIZE (_IOR('d',24,uint32))
                return Ok(count.saturating_mul(bs as u64));
            }
            return Ok(count.saturating_mul(SECTOR_SIZE as u64));
        }
    }

    // Legado: DIOCGMEDIASIZE (macOS) / BLKGETSIZE64 (Linux).
    let mut size: u64 = 0;
    #[cfg(target_os = "macos")]
    const REQ: u64 = 0x4008_6440; // DIOCGMEDIASIZE (_IOR('d', 64, uint64))
    #[cfg(target_os = "linux")]
    const REQ: u64 = 0x8008_1272; // BLKGETSIZE64 (_IOR(0x12, 114, size_t))
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let r = unsafe { ioctl(fd, REQ, &mut size) };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let ioctl_err = if r != 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let r = -1i32;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let ioctl_err: Option<io::Error> = None;
    if r == 0 && size > 0 {
        return Ok(size);
    }
    // Fallback: stat (los block devices de Linux reportan st_size).
    let len = file.metadata()?.len();
    if len > 0 {
        return Ok(len);
    }
    let causa = match ioctl_err {
        Some(e) => format!("ioctl: {e}"),
        None => "el ioctl devolvió tamaño 0 (medio sin tamaño reportado)".to_string(),
    };
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "no se pudo determinar el tamaño de {} ({})",
            path.display(),
            causa
        ),
    ))
}

/// Tamaño de sector físico (512 esperado; ≠512 ⇒ el llamador rechaza 4Kn).
#[cfg(target_os = "macos")]
fn device_sector_size(file: &File, _path: &Path) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // DKIOCGETPHYSICALBLOCKSIZE (_IOR('d',77,uint32)): tamaño físico real,
    // detecta 4Kn en nodos de disco completo (el legado responde ENOTTY ahí).
    let mut size: u32 = 0;
    if unsafe { ioctl(fd, 0x4004_644D, &mut size) } == 0 && size >= 512 {
        return Ok(size as usize);
    }
    size = 0;
    const REQ: u64 = 0x4004_6441; // DIOCGSECTORSIZE (_IOR('d', 65, uint32))
    let r = unsafe { ioctl(fd, REQ, &mut size) };
    if r == 0 && size >= 512 {
        return Ok(size as usize);
    }
    // ioctl no soportado (p. ej. imágenes de archivo): asumir 512.
    Ok(SECTOR_SIZE)
}

#[cfg(target_os = "linux")]
fn device_sector_size(file: &File, _path: &Path) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let mut size: i32 = 0;
    const REQ: u64 = 0x1268; // BLKSSZGET
    let r = unsafe { ioctl(file.as_raw_fd(), REQ, &mut size) };
    if r == 0 && size >= 512 {
        return Ok(size as usize);
    }
    Ok(SECTOR_SIZE)
}

#[cfg(windows)]
fn device_sector_size(file: &File, _path: &Path) -> io::Result<usize> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const IOCTL_DISK_GET_DRIVE_GEOMETRY: u32 = 0x0007_0010;

    #[repr(C)]
    struct DriveGeometry {
        cylinders: u32,
        tracks_per_cylinder: u32,
        sectors_per_track: u32,
        bytes_per_sector: u32,
    }

    extern "system" {
        fn DeviceIoControl(
            h_device: *mut c_void,
            dw_io_control_code: u32,
            lp_in_buffer: *mut c_void,
            n_in_buffer_size: u32,
            lp_out_buffer: *mut c_void,
            n_out_buffer_size: u32,
            lp_bytes_returned: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> i32;
    }

    let mut geo = DriveGeometry {
        cylinders: 0,
        tracks_per_cylinder: 0,
        sectors_per_track: 0,
        bytes_per_sector: 0,
    };
    let mut written: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            IOCTL_DISK_GET_DRIVE_GEOMETRY,
            std::ptr::null_mut(),
            0,
            &mut geo as *mut DriveGeometry as *mut c_void,
            std::mem::size_of::<DriveGeometry>() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 && geo.bytes_per_sector >= 512 {
        return Ok(geo.bytes_per_sector as usize);
    }
    Ok(SECTOR_SIZE)
}

/// Fallback fuera de Unix/Linux: asumir 512.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn device_sector_size(_file: &File, _path: &Path) -> io::Result<usize> {
    Ok(SECTOR_SIZE)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

#[cfg(target_os = "macos")]
extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

#[cfg(windows)]
fn windows_open(path: &Path, write: bool, strict: bool) -> io::Result<File> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;

    type HANDLE = *mut c_void;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: HANDLE,
        ) -> HANDLE;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let access = GENERIC_READ | if write { GENERIC_WRITE } else { 0 };
    // NO_BUFFERING : exige búfer/offset/longitud alineados al tamaño de
    // sector; `RawDisk::bulk` (128 KiB, align 4096) los garantiza. `open()`
    // lo desactiva reabriendo si el probe del medio lo rechaza.
    let mut flags = FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH | FILE_FLAG_SEQUENTIAL_SCAN;
    if strict {
        flags |= FILE_FLAG_NO_BUFFERING;
    }
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == -1isize as HANDLE {
        return Err(io::Error::last_os_error());
    }
    // El File resultante es dueño del handle (lo cierra al drop).
    Ok(unsafe { File::from_raw_handle(handle) })
}

#[cfg(windows)]
fn device_size(file: &File, path: &Path) -> io::Result<u64> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;

    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;

    extern "system" {
        fn DeviceIoControl(
            h_device: *mut c_void,
            dw_io_control_code: u32,
            lp_in_buffer: *mut c_void,
            n_in_buffer_size: u32,
            lp_out_buffer: *mut c_void,
            n_out_buffer_size: u32,
            lp_bytes_returned: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> i32;
    }

    let mut length: u64 = 0;
    let mut written: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            IOCTL_DISK_GET_LENGTH_INFO,
            std::ptr::null_mut(),
            0,
            &mut length as *mut u64 as *mut c_void,
            8,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 && written == 8 {
        return Ok(length);
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "no se pudo determinar el tamaño de {}: {}",
            path.display(),
            io::Error::last_os_error()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_name_por_so() {
        #[cfg(target_os = "macos")]
        assert_eq!(RawDisk::native_name(1), "/dev/rdisk1");
        #[cfg(target_os = "linux")]
        assert_eq!(RawDisk::native_name(1), "/dev/sdb");
        #[cfg(windows)]
        assert_eq!(RawDisk::native_name(1), "\\\\.\\PhysicalDrive1");
    }

    /// Regresión: los números ioctl de disco se derivan de la codificación
    /// BSD (sys/ioccom.h), no se copian a mano. Un minor incorrecto en
    /// DIOCGMEDIASIZE devolvía ENOTTY y vaciaba la enumeración nativa.
    #[cfg(target_os = "macos")]
    #[test]
    fn ioctl_disco_codificacion_bsd() {
        // _IOR(g, n, t) = IOC_OUT (0x40000000) | sizeof(t)<<16 | g<<8 | n
        let ior = |g: u64, n: u64, size: u64| 0x4000_0000u64 | (size << 16) | (g << 8) | n;
        assert_eq!(ior(b'd' as u64, 64, 8), 0x4008_6440); // DIOCGMEDIASIZE
        assert_eq!(ior(b'd' as u64, 65, 4), 0x4004_6441); // DIOCGSECTORSIZE
        // Familia DK (la fiable en nodos de disco completo, sys/disk.h).
        assert_eq!(ior(b'd' as u64, 24, 4), 0x4004_6418); // DKIOCGETBLOCKSIZE
        assert_eq!(ior(b'd' as u64, 25, 8), 0x4008_6419); // DKIOCGETBLOCKCOUNT
        assert_eq!(ior(b'd' as u64, 77, 4), 0x4004_644D); // DKIOCGETPHYSICALBLOCKSIZE
    }

    #[test]
    fn file_image_read_chunk_contiguo() {
        let dir = std::env::temp_dir().join(format!("regrow_src_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.raw");
        // 8 sectores, cada uno llenos del número de sector.
        let mut data = Vec::new();
        for s in 0..8u8 {
            data.extend_from_slice(&[s; SECTOR_SIZE]);
        }
        std::fs::write(&p, &data).unwrap();
        let mut img = FileImage::open_ro(&p).unwrap();
        let mut buf = vec![0u8; 4 * SECTOR_SIZE];
        img.read_chunk(2, 4, &mut buf).unwrap();
        for (i, chunk) in buf.chunks(SECTOR_SIZE).enumerate() {
            assert_eq!(chunk[0], 2 + i as u8);
            assert!(chunk.iter().all(|&b| b == 2 + i as u8));
        }
        // Chunk fuera de rango ⇒ error.
        assert!(img.read_chunk(6, 4, &mut buf).is_err());
        // La ruta por defecto (trait) y la optimizada coinciden en FileImage.
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Solo con permisos de root y un medio descartable:
    /// `cargo test -- --ignored` (no destructivo: abre en solo lectura).
    #[test]
    #[ignore = "requiere root/administrador sobre el disco crudo 0"]
    fn raw_disk_abierto_ro() {
        let name = RawDisk::native_name(0);
        let d = RawDisk::open(Path::new(&name), false).unwrap();
        assert!(d.sector_count() > 0, "{}: 0 sectores", name);
        assert!(matches!(d.phys(), 512 | 1024 | 2048 | 4096));
    }

    /// No destructivo: solo enumerar (puede devolver vacío sin permisos).
    #[test]
    fn list_native_disks_no_panic() {
        let disks = list_native_disks();
        for d in &disks {
            assert!(d.sectors > 0);
            assert!(!d.label.is_empty());
        }
    }

    #[test]
    fn media_from_rotational_valores() {
        assert_eq!(media_from_rotational(Some("0\n")), MediaKind::Ssd);
        assert_eq!(media_from_rotational(Some("1")), MediaKind::Hdd);
        assert_eq!(media_from_rotational(Some("2")), MediaKind::Unknown);
        assert_eq!(media_from_rotational(None), MediaKind::Unknown);
    }

    /// Snippets reales del `diskutil info -plist` medido en campo (macOS 15):
    /// NVMe interno (PCI-Express, DeviceBlockSize 4096) y USB externo.
    #[test]
    fn media_from_diskutil_plist_medido_en_campo() {
        let nvme = "\t<key>BusProtocol</key>\n\t\t<string>PCI-Express</string>\n\
                    \t<key>DeviceBlockSize</key>\n\t\t<integer>4096</integer>\n\
                    \t<key>MediaType</key>\n\t\t<string>Generic</string>";
        assert_eq!(media_from_diskutil_plist(nvme), MediaKind::Ssd);
        let usb = "\t<key>BusProtocol</key>\n\t\t<string>USB</string>\n\
                   \t<key>MediaType</key>\n\t\t<string>Generic</string>";
        assert_eq!(media_from_diskutil_plist(usb), MediaKind::Unknown);
        // macOS antiguos con Medium Type explícito.
        assert_eq!(
            media_from_diskutil_plist("<key>Medium Type</key><string>Rotational</string>"),
            MediaKind::Hdd
        );
        assert_eq!(
            media_from_diskutil_plist("<key>Medium Type</key><string>Solid State</string>"),
            MediaKind::Ssd
        );
        assert_eq!(media_from_diskutil_plist("sin claves"), MediaKind::Unknown);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn diskutil_device_path_mapea_rdisk() {
        assert_eq!(
            diskutil_device_path(Path::new("/dev/rdisk2")).as_deref(),
            Some("/dev/disk2")
        );
        assert_eq!(
            diskutil_device_path(Path::new("/dev/disk0s2")).as_deref(),
            Some("/dev/disk0s2")
        );
        assert_eq!(diskutil_device_path(Path::new("/dev/rdiskX")), None);
        assert_eq!(diskutil_device_path(Path::new("img.raw")), None);
    }

    /// Descriptor Windows sintético: Version@0, BusType@28, SeekPenalty@36.
    #[test]
    fn media_from_windows_descriptor_fixtures() {
        let mk = |version: u32, bus: u32, seek: u8| {
            let mut b = vec![0u8; 64];
            b[0..4].copy_from_slice(&version.to_le_bytes());
            b[28..32].copy_from_slice(&bus.to_le_bytes());
            b[36] = seek;
            b
        };
        // NVMe (BusType 0x11) ⇒ Ssd con cualquier version.
        assert_eq!(media_from_windows_descriptor(&mk(0x500, 0x11, 0xFF)), MediaKind::Ssd);
        // 0x600 + SeekPenalty 0 ⇒ sólido; 1 ⇒ mecánico; 0xFF dudoso ⇒ Unknown.
        assert_eq!(media_from_windows_descriptor(&mk(0x600, 3, 0)), MediaKind::Ssd);
        assert_eq!(media_from_windows_descriptor(&mk(0x600, 3, 1)), MediaKind::Hdd);
        assert_eq!(media_from_windows_descriptor(&mk(0x600, 3, 0xFF)), MediaKind::Unknown);
        // Version clásica 0x500 (sin SeekPenalty válido): bus ATA ⇒ Unknown.
        assert_eq!(media_from_windows_descriptor(&mk(0x500, 3, 0)), MediaKind::Unknown);
        assert_eq!(media_from_windows_descriptor(&[0u8; 16]), MediaKind::Unknown);
    }

    /// Snippet medido del contenedor APFS sintetizado (disk1 en el Mac de
    /// campo): hereda BusProtocol y SMART del store físico; las claves
    /// APFSPhysicalStores/APFSContainerReference lo delatan.
    #[test]
    fn diskutil_is_synthesized_contenedor_apfs() {
        let contenedor = "\t<key>BusProtocol</key>\n\t\t<string>PCI-Express</string>\n\
            \t<key>APFSContainerReference</key>\n\t\t<string>disk1</string>\n\
            \t<key>APFSPhysicalStores</key>\n\t\t<array>\n\t\t\t<string>disk0s2</string>\n\t\t</array>";
        assert!(diskutil_is_synthesized(contenedor));
        // Snippet de disco físico (disk2 USB medido): sin esas claves.
        let fisico = "\t<key>BusProtocol</key>\n\t\t<string>USB</string>\n\
            \t<key>ParentWholeDisk</key>\n\t\t<string>disk2</string>";
        assert!(!diskutil_is_synthesized(fisico));
    }

    /// Mapeo lógico 512 B → bloques físicos 4Kn (el corazón del soporte 4Kn).
    #[test]
    fn phys_range_blocks_4kn() {
        // 8 sectores lógicos = 1 bloque exacto.
        assert_eq!(phys_range_blocks(0, 8, 4096), (0, 1));
        // Rango alineado: sectores 8..24 = bloques 1..2.
        assert_eq!(phys_range_blocks(8, 16, 4096), (1, 2));
        // Rango desalineado: sector 3, 10 sectores → bytes 1536..6656 → bloques 0..1.
        assert_eq!(phys_range_blocks(3, 10, 4096), (0, 2));
        // Phys 512: bloque = sector.
        assert_eq!(phys_range_blocks(7, 3, 512), (7, 3));
    }
}
