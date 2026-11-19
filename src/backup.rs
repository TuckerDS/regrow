// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Respaldo preventivo de sectores antes de reescribirlos.
//!
//! El motor llama a `SectorBackup::put` con el contenido LEÍDO de cada sector
//! problemático ANTES de reescribirlo. Si el respaldo falla, el motor NO
//! escribe (protección de datos). El respaldo es una imagen dispersa: el
//! sector LBA se escribe en el offset LBA×512 del archivo, de modo que se
//! puede inspeccionar o verter de vuelta con herramientas estándar.

use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::source::SECTOR_SIZE;

/// Hook de respaldo del motor (ver `engine::run`).
pub trait SectorBackup {
    fn put(&mut self, lba: u64, data: &[u8; SECTOR_SIZE]) -> io::Result<()>;
}

/// Respaldo en un archivo disperso: `<backup_dir>/DISK<n>.bak.raw`.
pub struct FileBackup {
    file: File,
    #[allow(dead_code)]
    path: PathBuf,
}

impl FileBackup {
    pub fn open(backup_dir: &Path, unit: u32) -> io::Result<Self> {
        if !backup_dir.as_os_str().is_empty() {
            fs::create_dir_all(backup_dir)?;
        }
        let path = backup_dir.join(format!("DISK{unit}.bak.raw"));
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).open(&path)?;
        Ok(Self { file, path })
    }
}

impl SectorBackup for FileBackup {
    fn put(&mut self, lba: u64, data: &[u8; SECTOR_SIZE]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE as u64))?;
        self.file.write_all(data)?;
        Ok(())
    }
}

impl Drop for FileBackup {
    fn drop(&mut self) {
        // El respaldo debe sobrevivir a un corte del proceso.
        let _ = self.file.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backup_es_dispuesto_por_lba() {
        let dir = std::env::temp_dir().join(format!("regrow_bak_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        {
            let mut bk = FileBackup::open(&dir, 7).unwrap();
            let mut a = [0u8; SECTOR_SIZE];
            a[0] = 0xAA;
            let mut b = [0u8; SECTOR_SIZE];
            b[0] = 0xBB;
            bk.put(0, &a).unwrap();
            bk.put(3, &b).unwrap(); // disperso: hueco entre offsets
        }
        let bytes = fs::read(dir.join("DISK7.bak.raw")).unwrap();
        assert_eq!(bytes.len(), 4 * SECTOR_SIZE);
        assert_eq!(bytes[0], 0xAA);
        assert_eq!(bytes[SECTOR_SIZE], 0); // agujero sin respaldar
        assert_eq!(bytes[3 * SECTOR_SIZE], 0xBB);
        fs::remove_dir_all(&dir).ok();
    }
}
