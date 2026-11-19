// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! `disklist.cfg`: catálogo de unidades conocidas (generador de línea y
//! mantenimiento del fichero).
//!
//! Formato de línea (derivado del formato clásico de estos catálogos): `DISK <n>: <size> Mb|Gb in <sectors> sectors`
//! - `Mb = sectores >> 11`; si `Mb < 0x2800` ⇒ "Mb", si no ⇒ `Gb = Mb >> 10`.
//! - El conteo de sectores se imprime como **int32 con signo** (mantiene la
//!   compatibilidad con listas antiguas: la unidad de 1863 GB imprime
//!   `-387938128 sectors`).
//! - Al actualizar se acepta y reemplaza tanto la forma propia `DISK n:` como
//!   la forma legacy `HDD n:` (importación de catálogos antiguos); las líneas
//!   de otras unidades se preservan tal cual.

use std::fs;
use std::io;
use std::path::Path;

/// Línea canónica del `disklist.cfg` para la unidad `n` con `sectors` sectores.
pub fn format_disk_line(n: u32, sectors: u64) -> String {
    let mb = sectors >> 11;
    let size = if mb < 0x2800 {
        format!("{} Mb", mb)
    } else {
        format!("{} Gb", mb >> 10)
    };
    format!("DISK {}: {} in {} sectors", n, size, sectors as i32)
}

/// Actualiza `disklist.cfg` in-place: reemplaza (o añade) la línea de la
/// unidad `n`, preservando el orden y las demás líneas (incluidas legacy).
/// EOL CRLF, fichero terminado en CRLF.
pub fn update_disklist(path: &Path, n: u32, sectors: u64) -> io::Result<()> {
    let existing = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let new_line = format_disk_line(n, sectors);
    let prefixes = [format!("DISK {n}: "), format!("HDD {n}: ")];

    let mut lines: Vec<String> = existing
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty())
        .collect();

    match lines
        .iter_mut()
        .find(|l| prefixes.iter().any(|p| l.starts_with(p.as_str())))
    {
        Some(slot) => *slot = new_line,
        None => lines.push(new_line),
    }

    let mut out = String::new();
    for l in &lines {
        out.push_str(l);
        out.push_str("\r\n");
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    fs::write(path, out.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linea_mb_exacta() {
        // Equivalente a la fixture clásica "HDD 1: 1928 Mb in 3948544 sectors".
        assert_eq!(
            format_disk_line(1, 3_948_544),
            "DISK 1: 1928 Mb in 3948544 sectors"
        );
    }

    #[test]
    fn linea_gb_exacta() {
        assert_eq!(
            format_disk_line(2, 625_142_448),
            "DISK 2: 298 Gb in 625142448 sectors"
        );
    }

    #[test]
    fn sectores_como_int32_con_signo() {
        // La unidad de 1863 GB: 3.907.029.168 sectores ⇒ -387938128.
        assert_eq!(
            format_disk_line(4, 3_907_029_168),
            "DISK 4: 1863 Gb in -387938128 sectors"
        );
    }

    #[test]
    fn update_reemplaza_conserva_y_importa_legacy() {
        let dir = std::env::temp_dir().join("regrow_cfg_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("disklist.cfg");
        fs::write(
            &p,
            "HDD 1: 1928 Mb in 3948544 sectors\r\nHDD 2: 298 Gb in 625142448 sectors\r\n",
        )
        .unwrap();
        // Reemplaza la forma legacy escribiendo la forma propia.
        update_disklist(&p, 1, 3_948_545).unwrap();
        // Añade una unidad nueva.
        update_disklist(&p, 3, 1024).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert_eq!(
            got,
            "DISK 1: 1928 Mb in 3948545 sectors\r\nHDD 2: 298 Gb in 625142448 sectors\r\nDISK 3: 0 Mb in 1024 sectors\r\n"
        );
        // Volver a actualizar la unidad 1 no duplica (reconoce DISK 1:).
        update_disklist(&p, 1, 2048).unwrap();
        let got = fs::read_to_string(&p).unwrap();
        assert_eq!(got.matches("1: ").count(), 1);
        fs::remove_dir_all(&dir).ok();
    }
}
