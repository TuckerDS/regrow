// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! SMART vía el programa `smartctl` (smartmontools) como subproceso.
//!
//! La lectura SMART se delega a `smartctl -a`
//! (presente en macOS con `brew install smartmontools`, casi siempre en
//! Linux, opcional en Windows). El passthrough ATA propio (SG_IO/SAT) queda
//! diferido. Si `smartctl` no está disponible, las funciones degradan con
//! gracefully: `smart_available() == false` y `TemperatureGuard` no aborta.
//!
//! Parsing heurístico del texto de `smartctl -a` (validado con fixtures
//! sintéticas en los tests; sin serde ni crates):
//! - Columna RAW_VALUE = primer token numérico tras la columna WHEN
//!   (`Prefail`, `Old_age`, `Usage`, `Always_`, `Offline`…).
//! - Atributos de interés: 5 Reallocated_Sector_Ct, 197 Current_Pending_Sector,
//!   198 Offline_Uncorrectable, 199 UDMA_CRC_Error_Count,
//!   194/190 Temperature_Celsius/Airflow_Temperature_Celsius.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::engine::{HealthMonitor, MonitorAction, PollInfo};
use crate::source::MediaKind;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SmartInfo {
    pub available: bool, // smartctl existe y respondió
    pub smart_enabled: bool,
    pub model: Option<String>,
    pub reallocated: Option<u64>,    // attr 5
    pub pending: Option<u64>,        // attr 197
    pub uncorrectable: Option<u64>,  // attr 198
    pub crc_errors: Option<u64>,     // attr 199
    pub temperature_c: Option<u64>,  // attr 194/190
    pub percentage_used: Option<u64>,   // NVMe "Percentage Used" (desgaste)
    pub media_errors: Option<u64>,      // NVMe "Media and Data Integrity Errors"
    pub wear_leveling: Option<u64>,     // ATA Wear_Leveling_Count (176/177/231)
    pub rotation_rate: Option<String>,  // "Solid State Device" | "7200.0 rpm"
}

impl SmartInfo {
    /// Resumen compacto para avisos del monitor.
    pub fn deltas_note(&self) -> String {
        let f = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "?".into());
        format!(
            "reasignados={} pendientes={} noCorregibles={} crc={}",
            f(self.reallocated),
            f(self.pending),
            f(self.uncorrectable),
            f(self.crc_errors)
        )
    }
}

/// ¿Existe `smartctl` en el PATH? (se invoca con `-V`).
pub fn smart_available() -> bool {
    Command::new("smartctl")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ejecuta `smartctl` y captura stdout (truncado a 256 KiB).
fn run_smartctl(path: &Path) -> std::io::Result<String> {
    let mut child = Command::new("smartctl")
        .arg("-a")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut out = Vec::new();
    if let Some(so) = child.stdout.take() {
        let _ = so.take(256 * 1024).read_to_end(&mut out);
    }
    let _ = child.wait();
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn first_number(tok_source: &[&str]) -> Option<u64> {
    for t in tok_source {
        let clean = t.trim_matches(|c: char| !c.is_ascii_digit());
        if !clean.is_empty() {
            if let Ok(v) = clean.parse::<u64>() {
                return Some(v);
            }
        }
    }
    None
}

/// Parsea la salida de `smartctl -a` (solo lo que regrow necesita).
pub fn parse_smartctl_text(text: &str) -> SmartInfo {
    let mut info = SmartInfo {
        available: true,
        smart_enabled: text.contains("SMART overall-health self-assessment")
            || text.contains("SMART support is: Available"),
        ..Default::default()
    };
    const WHEN_COLS: [&str; 12] = [
        "Pre-fail",
        "Prefail",
        "Old_age",
        "Usage",
        "Advisory",
        "Auto_keep",
        "On_request",
        "Always",
        "Online",
        "Never",
        "Offline",
        "-",
    ];
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("Device Model:") || l.starts_with("Model Name:") {
            if info.model.is_none() {
                info.model = l.split_once(':').map(|(_, v)| v.trim().to_string());
            }
            continue;
        }
        if l.starts_with("SMART Disabled") || l.contains("SMART support is: Disabled") {
            info.smart_enabled = false;
            continue;
        }
        // Líneas informativas NVMe/ATA (sin ID de atributo): desgaste y
        // rotación, usadas por el gate de desgaste y los deltas.
        if let Some(rest) = l.strip_prefix("Rotation Rate:") {
            info.rotation_rate = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = l.strip_prefix("Percentage Used:") {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            info.percentage_used = first_number(&toks);
            continue;
        }
        if let Some(rest) = l
            .strip_prefix("Media and Data Integrity Errors:")
            .or_else(|| l.strip_prefix("Media Errors:"))
        {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            info.media_errors = first_number(&toks);
            continue;
        }
        // Línea de atributo: empieza con el ID numérico.
        let mut it = l.split_whitespace();
        let id: u32 = match it.next().and_then(|t| t.parse().ok()) {
            Some(v) if (1..=255).contains(&v) => v,
            _ => continue,
        };
        let name = it.next().unwrap_or("");
        let tokens: Vec<&str> = it.collect();
        // RAW_VALUE: primer token tras la última columna WHEN conocida.
        let raw_from = tokens.iter().rposition(|t| WHEN_COLS.contains(t));
        let raw_tokens: &[&str] = match raw_from {
            Some(p) => &tokens[p + 1..],
            None => &tokens, // tablas sin columna WHEN: usar todo lo restante
        };
        match id {
            5 => info.reallocated = first_number(raw_tokens),
            197 => info.pending = first_number(raw_tokens),
            198 => info.uncorrectable = first_number(raw_tokens),
            199 => info.crc_errors = first_number(raw_tokens),
            190 | 194 if name.contains("Temperature") => {
                info.temperature_c = first_number(raw_tokens)
            }
            176 | 177 | 231 if name.contains("Wear") => {
                info.wear_leveling = first_number(raw_tokens)
            }
            _ => {}
        }
    }
    info
}

/// Consulta SMART de un dispositivo crudo (`/dev/rdiskN`, `\\.\PhysicalDriveN`).
pub fn query(path: &Path) -> std::io::Result<SmartInfo> {
    match run_smartctl(path) {
        Ok(text) => Ok(parse_smartctl_text(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SmartInfo::default()),
        Err(e) => Err(e),
    }
}

/// Imprime el estado SMART completo (subcomando `smart`).
pub fn print_status(info: &SmartInfo, device: &str) {
    if !info.available {
        println!("{device}: SMART no disponible (instala smartmontools/smartctl)");
        return;
    }
    println!("{device}: SMART {}", if info.smart_enabled { "habilitado" } else { "deshabilitado" });
    if let Some(m) = &info.model {
        println!("  modelo: {m}");
    }
    let f = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "?".into());
    println!("  sectores reasignados: {}", f(info.reallocated));
    println!("  sectores pendientes de reasignación: {}", f(info.pending));
    println!("  sectores no corregibles: {}", f(info.uncorrectable));
    println!("  errores CRC de transporte: {}", f(info.crc_errors));
    println!("  temperatura: {}", f(info.temperature_c));
    if let Some(r) = &info.rotation_rate {
        println!("  rotación: {r}");
    }
    if info.percentage_used.is_some()
        || info.media_errors.is_some()
        || info.wear_leveling.is_some()
    {
        println!(
            "  desgaste: usado={} %  mediaErrors={}  wearLeveling={}",
            f(info.percentage_used),
            f(info.media_errors),
            f(info.wear_leveling)
        );
    }
}

/// Pista de tipo de medio desde SMART (`Rotation Rate:`): refuerza la
/// detección del SO cuando esta quedó Unknown .
pub fn media_hint(info: &SmartInfo) -> MediaKind {
    match info.rotation_rate.as_deref() {
        Some(r) if r.contains("Solid State") => MediaKind::Ssd,
        Some(r) if r.contains("rpm") => MediaKind::Hdd,
        _ => MediaKind::Unknown,
    }
}

/// Deltas SMART entre dos instantáneas : solo atributos comparables
/// en ambas. `Some("sin cambios")` si son comparables e idénticos; None si no
/// hay ningún par comparable.
pub fn delta_note(before: &SmartInfo, after: &SmartInfo) -> Option<String> {
    let pairs: [(&str, Option<u64>, Option<u64>); 7] = [
        ("reasignados", before.reallocated, after.reallocated),
        ("pendientes", before.pending, after.pending),
        ("noCorregibles", before.uncorrectable, after.uncorrectable),
        ("crc", before.crc_errors, after.crc_errors),
        ("usado%", before.percentage_used, after.percentage_used),
        ("mediaErrors", before.media_errors, after.media_errors),
        ("wear", before.wear_leveling, after.wear_leveling),
    ];
    let mut parts = Vec::new();
    let mut comparable = 0usize;
    for (name, b, a) in pairs {
        if let (Some(b), Some(a)) = (b, a) {
            comparable += 1;
            if b != a {
                let d = a as i64 - b as i64;
                let sign = if d > 0 { '+' } else { '-' };
                parts.push(format!("{name} {b}→{a} ({sign}{})", d.abs()));
            }
        }
    }
    if comparable == 0 {
        return None;
    }
    if parts.is_empty() {
        return Some("sin cambios".to_string());
    }
    Some(parts.join(", "))
}

/// Monitor de temperatura: sondea SMART en cada poll; si pasa el umbral,
/// duerme hasta que baje (poll bloqueante) y, si no baja en `max_wait`,
/// aborta el pase.
pub struct TemperatureGuard {
    pub device: std::path::PathBuf,
    pub max_c: u64,
    pub hysteresis: u64, // reanuda cuando temp <= max_c - hysteresis
    poll_secs: u64,
    max_wait: Duration,
    pub last: Option<SmartInfo>,
}

impl TemperatureGuard {
    pub fn new(device: &Path, max_c: u64) -> Self {
        Self {
            device: device.to_path_buf(),
            max_c,
            hysteresis: 2,
            poll_secs: 8,
            max_wait: Duration::from_secs(600),
            last: None,
        }
    }
}

impl HealthMonitor for TemperatureGuard {
    fn poll(&mut self, _info: &PollInfo) -> MonitorAction {
        match query(&self.device) {
            Ok(si) => {
                let temp = si.temperature_c;
                let note = si.deltas_note();
                self.last = Some(si);
                let Some(temp) = temp else {
                    return MonitorAction::Continue; // sin sensor: no bloquear
                };
                if temp <= self.max_c {
                    return MonitorAction::Continue;
                }
                println!(
                    "\naviso: {} °C (umbral {} °C) — pausando hasta enfriar… [{}]",
                    temp,
                    self.max_c,
                    note
                );
                let t0 = std::time::Instant::now();
                loop {
                    thread::sleep(Duration::from_secs(self.poll_secs));
                    match query(&self.device) {
                        Ok(si2) => {
                            self.last = Some(si2.clone());
                            if let Some(t) = si2.temperature_c {
                                if t <= self.max_c.saturating_sub(self.hysteresis) {
                                    println!("recuperada temperatura: {t} °C — reanudando");
                                    return MonitorAction::Continue;
                                }
                            }
                        }
                        Err(_) => return MonitorAction::Continue, // fallo de sondeo: no bloquear
                    }
                    if t0.elapsed() > self.max_wait {
                        println!(
                            "\nabortando: temperatura por encima de {} °C durante más de {} min",
                            self.max_c,
                            self.max_wait.as_secs() / 60
                        );
                        return MonitorAction::Abort;
                    }
                }
            }
            // Sin smartctl o fallo: el guard degrada a no-op.
            Err(_) => MonitorAction::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture sintética con el formato tabla de `smartctl -a` (ATA).
    const ATA_SAMPLE: &str = "=== START OF INFORMATION SECTION ===\n\
        Model Family: Western Digital Blue\n\
        Device Model:     WDC WD10EZEX-00B\n\
        Device is:        In smartctl database\n\
        SMART support is: Available - device has SMART capability.\n\
        SMART support is: Enabled\n\n\
        === START OF READ SMART DATA SECTION ===\n\
        SMART overall-health self-assessment test result: PASSED\n\n\
        General SMART Values:\n\
        Offline data collection status: (0x82)\tOffline data collection activity\n\
        was completed without error.\n\n\
        SMART Attributes Data Structure revision number: 16\n\
        Vendor Specific SMART Attributes with Thresholds:\n\
        ID# ATTRIBUTE_NAME          FLAG     VALUE WORST THRESH TYPE      UPDATED  WHEN_FAILED RAW_VALUE\n\
          5 Reallocated_Sector_Ct   0x0033   200   200   140    Pre-fail  Always       -       133\n\
          9 Power_On_Hours          0x0032   083   083   000    Old_age   Always       -       14567\n\
        197 Current_Pending_Sector  0x0032   200   200   000    Old_age   Always       -       12\n\
        198 Offline_Uncorrectable   0x0030   200   200   000    Old_age   Offline      -       3\n\
        199 UDMA_CRC_Error_Count    0x003e   200   200   000    Old_age   Always       -       7\n\
        194 Temperature_Celsius     0x0022   060   090   000    Old_age   Always       -       43 (Min/Max 19/57)\n";

    #[test]
    fn parse_ata_atributos_y_temperatura() {
        let si = parse_smartctl_text(ATA_SAMPLE);
        assert!(si.available);
        assert!(si.smart_enabled);
        assert_eq!(si.model.as_deref(), Some("WDC WD10EZEX-00B"));
        assert_eq!(si.reallocated, Some(133));
        assert_eq!(si.pending, Some(12));
        assert_eq!(si.uncorrectable, Some(3));
        assert_eq!(si.crc_errors, Some(7));
        assert_eq!(si.temperature_c, Some(43)); // primer token, no Min/Max
    }

    #[test]
    fn parse_nvme_estilo_basico() {
        // NVMe no tiene tabla de atributos clásica: se degrada sin valores.
        let si = parse_smartctl_text("SMART overall-health self-assessment test result: PASSED\n");
        assert!(si.available);
        assert!(si.smart_enabled);
        assert_eq!(si.reallocated, None);
        assert_eq!(si.temperature_c, None);
    }

    #[test]
    fn parse_smart_deshabilitado() {
        let si = parse_smartctl_text("SMART support is: Available\nSMART Disabled. Use option -s with value: 1\n");
        assert!(!si.smart_enabled);
    }

    #[test]
    fn deltas_note_formatea() {
        let si = SmartInfo {
            available: true,
            reallocated: Some(5),
            ..Default::default()
        };
        assert_eq!(
            si.deltas_note(),
            "reasignados=5 pendientes=? noCorregibles=? crc=?"
        );
    }

    /// Fixture NVMe estilo smartctl ≥7: desgaste y rotación en líneas
    /// informativas (no tabla de atributos ATA).
    const NVME_SAMPLE: &str = "=== START OF INFORMATION SECTION ===\n\
        Model Number:                       SAMSUNG MZ7LN512\n\
        Rotation Rate:                      Solid State Device\n\n\
        === START OF READ SMART DATA SECTION ===\n\
        SMART overall-health self-assessment test result: PASSED\n\n\
        Percentage Used:                    12%\n\
        Media and Data Integrity Errors: 0\n";

    #[test]
    fn parse_nvme_desgaste_y_rotacion() {
        let si = parse_smartctl_text(NVME_SAMPLE);
        assert!(si.available);
        assert_eq!(si.rotation_rate.as_deref(), Some("Solid State Device"));
        assert_eq!(si.percentage_used, Some(12));
        assert_eq!(si.media_errors, Some(0));
        assert_eq!(media_hint(&si), MediaKind::Ssd);
    }

    #[test]
    fn parse_ata_wear_y_rotacion_mecanica() {
        let si = parse_smartctl_text(
            "Rotation Rate: 7200.0 rpm\n\
             231 Wear_Leveling_Count   0x0013   042   042   000    Pre-fail  Always       -       3000\n",
        );
        assert_eq!(si.wear_leveling, Some(3000));
        assert_eq!(si.rotation_rate.as_deref(), Some("7200.0 rpm"));
        assert_eq!(media_hint(&si), MediaKind::Hdd);
    }

    #[test]
    fn delta_note_cambios_sin_cambios_y_no_comparable() {
        let before = SmartInfo {
            available: true,
            reallocated: Some(133),
            pending: Some(12),
            ..Default::default()
        };
        let after = SmartInfo {
            available: true,
            reallocated: Some(135),
            pending: Some(0),
            ..Default::default()
        };
        assert_eq!(
            delta_note(&before, &after).as_deref(),
            Some("reasignados 133→135 (+2), pendientes 12→0 (-12)")
        );
        assert_eq!(delta_note(&before, &before).as_deref(), Some("sin cambios"));
        let vacio = SmartInfo {
            available: true,
            ..Default::default()
        };
        assert_eq!(delta_note(&before, &vacio), None);
    }
}
