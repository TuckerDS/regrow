// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Capa lógica de SOLO LECTURA de regrow (`regrow fs`): firmas y geometría
//! de los sistemas de archivos, tablas MBR/GPT y el layout NTFS derivable
//! del BPB — todo derivado de las especificaciones públicas (MBR/GPT del
//! estándar UEFI, BPB NTFS/FAT) sin montar nada y sin escribir jamás.
//!
//! El mapa de sectores manda: el estado de una estructura se consulta en el
//! mapfile, no se deduce de que la lectura «funcione».

use crate::mapfile::{crc32, MapFile, MapStatus};
use crate::source::{FileImage, RawDisk, SectorSource, SECTOR_SIZE};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// ------------------------------------------------------------------ firmas ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootKind {
    Ntfs,
    Fat32,
    Fat16,
    ExFat,
    Unknown,
}

/// Firma del sector de arranque (sector 0 del dominio o de una partición).
pub fn detect_boot(s0: &[u8]) -> BootKind {
    if s0.len() < 512 {
        return BootKind::Unknown;
    }
    if s0[3..11] == *b"NTFS    " {
        return BootKind::Ntfs;
    }
    if s0[3..11] == *b"EXFAT   " {
        return BootKind::ExFat;
    }
    if s0[82..90] == *b"FAT32   " {
        return BootKind::Fat32;
    }
    if s0[54..62] == *b"FAT16   " {
        return BootKind::Fat16;
    }
    BootKind::Unknown
}

pub fn has_boot_marker(s0: &[u8]) -> bool {
    s0.len() >= 512 && s0[510] == 0x55 && s0[511] == 0xAA
}

// ------------------------------------------------------------------- MBR -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MbrEntry {
    pub bootable: bool,
    /// Tipo de partición (0x00 vacía, 0xEE GPT protector, 0x07 NTFS/FAT…).
    pub kind: u8,
    pub start_lba: u64,
    pub sectors: u64,
}

pub struct Mbr {
    pub entries: [MbrEntry; 4],
}

impl Mbr {
    /// Parsea la tabla de particiones del sector 0. Un MBR es válido si lleva
    /// la marca 0x55AA; las entradas se leen siempre (algunos medios legítimos
    /// carecen de marca pero tienen tabla).
    pub fn parse(s0: &[u8]) -> Option<Mbr> {
        if s0.len() < 512 {
            return None;
        }
        let mut entries = [MbrEntry { bootable: false, kind: 0, start_lba: 0, sectors: 0 }; 4];
        for i in 0..4 {
            let e = &s0[446 + i * 16..462 + i * 16];
            entries[i] = MbrEntry {
                bootable: e[0] == 0x80,
                kind: e[4],
                start_lba: u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64,
                sectors: u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64,
            };
        }
        if !has_boot_marker(s0) {
            // Sin marca: solo se ofrece la tabla si hay entradas con sectores.
            if entries.iter().all(|e| e.sectors == 0) {
                return None;
            }
        }
        Some(Mbr { entries })
    }

    pub fn is_gpt_protective(&self) -> bool {
        self.entries.iter().any(|e| e.kind == 0xEE && e.sectors > 0)
    }
}

// ------------------------------------------------------------------- GPT -----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GptPartition {
    pub type_guid: String,
    pub unique_guid: String,
    pub name: String,
    pub start_lba: u64,
    pub end_lba: u64,
    pub attrs: u64,
}

#[derive(Debug, Clone)]
pub struct Gpt {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub entries_lba: u64,
    pub partitions: Vec<GptPartition>,
    /// CRC del header verificado (campo de CRC puesto a 0 durante el cálculo).
    pub header_crc_ok: bool,
    /// CRC del array de entradas verificado.
    pub entries_crc_ok: bool,
    /// Nº de entradas declarado vs. usable según `entry_size`.
    pub entries_declared: u32,
}

fn guid_string(b: &[u8]) -> String {
    // Mezcla canónica: los tres primeros grupos en little endian, el resto en big endian.
    let a = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let c = u16::from_le_bytes(b[4..6].try_into().unwrap());
    let d = u16::from_le_bytes(b[6..8].try_into().unwrap());
    format!(
        "{a:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        c, d, b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn utf16_name(b: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i + 1 < b.len() {
        let u = u16::from_le_bytes([b[i], b[i + 1]]);
        if u == 0 {
            break;
        }
        out.push(char::from_u32(u as u32).unwrap_or('?'));
        i += 2;
    }
    out
}

/// Valida una cabecera GPT (leída en su LBA) y su array de entradas.
/// `header` son ≥92 bytes del LBA de la cabecera; `entries` el array completo
/// (`num_entries × entry_size`).
pub fn parse_gpt(header: &[u8], entries: &[u8]) -> Option<Gpt> {
    if header.len() < 92 || &header[0..8] != b"EFI PART" {
        return None;
    }
    // Layout canónico: firma 0..8, revisión 8..12, tamaño 12..16, CRC header
    // 16..20, reservado 20..24, my_lba 24..32, alternate 32..40, primero
    // utilizable 40..48, último 48..56, GUID disco 56..72, entradas LBA
    // 72..80, nº entradas 80..84, tamaño entrada 84..88, CRC array 88..92.
    let hdr_size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    if hdr_size < 92 {
        return None;
    }
    let stored_hdr_crc = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let mut copy = header[..hdr_size.min(header.len())].to_vec();
    copy[16..20].copy_from_slice(&[0u8; 4]);
    let header_crc_ok = crc32(&copy) == stored_hdr_crc;

    let my_lba = u64::from_le_bytes(header[24..32].try_into().unwrap());
    let alternate_lba = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let first_usable = u64::from_le_bytes(header[40..48].try_into().unwrap());
    let last_usable = u64::from_le_bytes(header[48..56].try_into().unwrap());
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let num_entries = u32::from_le_bytes(header[80..84].try_into().unwrap());
    let entry_size = u32::from_le_bytes(header[84..88].try_into().unwrap());
    let stored_arr_crc = u32::from_le_bytes(header[88..92].try_into().unwrap());
    if entry_size < 128 {
        return None;
    }
    let avail = entries.len() / entry_size as usize;
    let n = num_entries.min(avail as u32);
    let entries_crc_ok = crc32(&entries[..n as usize * entry_size as usize]) == stored_arr_crc;

    let mut partitions = Vec::new();
    for i in 0..n as usize {
        let e = &entries[i * entry_size as usize..(i + 1) * entry_size as usize];
        if e[0..16] == [0u8; 16] {
            continue; // entrada vacía
        }
        // Layout de entrada: type_guid 0..16, unique_guid 16..32,
        // start_lba 32..40, end_lba 40..48, attrs 48..56, nombre UTF-16 56..128.
        partitions.push(GptPartition {
            type_guid: guid_string(&e[0..16]),
            unique_guid: guid_string(&e[16..32]),
            name: utf16_name(&e[56..128]),
            start_lba: u64::from_le_bytes(e[32..40].try_into().unwrap()),
            end_lba: u64::from_le_bytes(e[40..48].try_into().unwrap()),
            attrs: u64::from_le_bytes(e[48..56].try_into().unwrap()),
        });
    }
    Some(Gpt {
        my_lba,
        alternate_lba,
        first_usable,
        last_usable,
        entries_lba,
        partitions,
        header_crc_ok,
        entries_crc_ok,
        entries_declared: num_entries,
    })
}

// ------------------------------------------------------------- BPB NTFS ------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NtfsBpb {
    pub sector_size: usize,
    pub sectors_per_cluster: u32,
    /// Nº de sectores declarados en el BPB (`sectors_nbr`). El volumen real
    /// ocupa `sectors_nbr + 1` sectores: el último es el arranque de respaldo.
    pub sectors_nbr: u64,
    pub mft_lcn: u64,
    pub mftmirr_lcn: u64,
    pub clusters_per_mft_record: i8,
    pub clusters_per_index_record: i8,
    /// Tamaño de registro MFT en bytes (regla: >0 n×cluster; <0 2^|n|).
    pub mft_record_size: usize,
    // derivados:
    pub cluster_size: usize,
    pub mft_sector: u64,
    pub mftmirr_sector: u64,
    /// Sector del arranque de respaldo (último sector del volumen).
    pub backup_boot_sector: u64,
    /// Nº de clusters del volumen (topado por `sectors_nbr / spc`).
    pub cluster_count: u64,
}

pub fn parse_ntfs_bpb(s0: &[u8]) -> Option<NtfsBpb> {
    if s0.len() < 512 || s0[3..7] != *b"NTFS" || !has_boot_marker(s0) {
        return None;
    }
    // Coherencia estructural (criterio clásico de validación): campos FAT a
    // cero y `reserved` 0. Los campos propios de NTFS empiezan en 0x28.
    if s0[14] != 0
        || s0[15] != 0
        || s0[16] != 0
        || s0[17] != 0
        || s0[18] != 0
        || s0[19] != 0
        || s0[20] != 0
        || s0[22] != 0
        || s0[23] != 0
        || s0[32] != 0
        || s0[33] != 0
        || s0[34] != 0
        || s0[35] != 0
    {
        return None;
    }
    let sector_size = u16::from_le_bytes(s0[11..13].try_into().unwrap()) as usize;
    let spc = s0[13];
    if spc.count_ones() != 1 || !(1..=128).contains(&spc) {
        return None;
    }
    let sectors_nbr = u64::from_le_bytes(s0[40..48].try_into().unwrap());
    if sectors_nbr == 0 {
        return None;
    }
    let mft_lcn = u64::from_le_bytes(s0[0x30..0x38].try_into().unwrap());
    let mftmirr_lcn = u64::from_le_bytes(s0[0x38..0x40].try_into().unwrap());
    let cpr = s0[0x40] as i8;
    if cpr == 0 {
        return None;
    }
    let cpi = s0[0x44] as i8;
    let spc = spc as u32;
    let cluster_size = spc as usize * sector_size;
    let mft_record_size = if cpr < 0 {
        1usize << cpr.unsigned_abs()
    } else {
        cpr as usize * cluster_size
    };
    Some(NtfsBpb {
        sector_size,
        sectors_per_cluster: spc,
        sectors_nbr,
        mft_lcn,
        mftmirr_lcn,
        clusters_per_mft_record: cpr,
        clusters_per_index_record: cpi,
        mft_record_size,
        cluster_size,
        mft_sector: mft_lcn * spc as u64,
        mftmirr_sector: mftmirr_lcn * spc as u64,
        backup_boot_sector: sectors_nbr,
        cluster_count: sectors_nbr / spc as u64,
    })
}

/// Fallback sin BPB legible: con dos registros MFT espejo encontrados en
/// sectores `sa` y `sb` con LCN conocidos `la` y `lb` (primer run de cada
/// atributo $DATA residente), la fórmula estándar `spc = Δsector / Δlcn`,
/// aceptando solo potencias de 2.
pub fn derive_spc_from_mft_pair(sa: u64, la: u64, sb: u64, lb: u64) -> Option<u32> {
    let ds = sa.abs_diff(sb);
    let dl = la.abs_diff(lb);
    if dl == 0 || ds == 0 || !ds.is_multiple_of(dl) {
        return None;
    }
    let spc = ds / dl;
    if spc > 128 || spc.count_ones() != 1 {
        return None;
    }
    Some(spc as u32)
}

// ------------------------------------------------------------- BPB FAT32 -----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fat32Bpb {
    pub sector_size: usize,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    pub fat_size: u32,
    pub root_cluster: u32,
    pub fsinfo_sector: u32,
    pub backup_boot_offset: u32,
    pub total_sectors: u64,
    /// Primer sector de datos (reservados + tablas FAT).
    pub data_start: u64,
}

pub fn parse_fat32_bpb(s0: &[u8]) -> Option<Fat32Bpb> {
    if s0.len() < 512 || s0[82..90] != *b"FAT32   " || !has_boot_marker(s0) {
        return None;
    }
    let sector_size = u16::from_le_bytes(s0[11..13].try_into().unwrap()) as usize;
    let spc = s0[13];
    if spc.count_ones() != 1 || !(1..=128).contains(&spc) {
        return None;
    }
    // Un BPB FAT32 auténtico tiene vacíos los campos FAT16: raíz (17..19),
    // sectores de 16 bits (19..21 puede usarse, pero 22..24 debe ser 0) y
    // el contador grande de 32 bits en 32..36 suele ser 0 si el tamaño vive
    // en `total_sectors32` (32..36) — se acepta cualquiera de los dos.
    if s0[17] != 0 || s0[18] != 0 || s0[22] != 0 || s0[23] != 0 {
        return None;
    }
    let reserved = u16::from_le_bytes(s0[14..16].try_into().unwrap()) as u32;
    let num_fats = s0[16];
    if num_fats == 0 {
        return None;
    }
    let total_small = u16::from_le_bytes(s0[19..21].try_into().unwrap()) as u64;
    let total_big = u32::from_le_bytes(s0[32..36].try_into().unwrap()) as u64;
    let total_sectors = if total_big != 0 { total_big } else { total_small };
    if total_sectors == 0 {
        return None;
    }
    let fat_size = u32::from_le_bytes(s0[36..40].try_into().unwrap());
    if fat_size == 0 {
        return None;
    }
    let root_cluster = u32::from_le_bytes(s0[44..48].try_into().unwrap());
    let fsinfo = u16::from_le_bytes(s0[48..50].try_into().unwrap()) as u32;
    let backup = u16::from_le_bytes(s0[50..52].try_into().unwrap()) as u32;
    Some(Fat32Bpb {
        sector_size,
        sectors_per_cluster: spc as u32,
        reserved_sectors: reserved,
        num_fats: num_fats as u32,
        fat_size,
        root_cluster,
        fsinfo_sector: fsinfo,
        backup_boot_offset: backup,
        total_sectors,
        data_start: reserved as u64 + num_fats as u64 * fat_size as u64,
    })
}

// ------------------------------------------------- mapping pairs (runlists) --

/// Un tramo decodificado de una runlist NTFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// Nº de clusters del tramo.
    pub clusters: u64,
    /// LCN de inicio (acumulativo). Sin significado si `hole` es true.
    pub lcn: i64,
    /// Agujero sparse (`LCN_HOLE`): rango no asignado.
    pub hole: bool,
}

/// Decodificador de mapping pairs con la convención completa: cabecera →
/// nibble BAJO = nº de bytes del campo LONGITUD (sin signo), nibble ALTO =
/// nº de bytes del DELTA de LCN (complemento a 2). En el array va primero
/// la longitud y después el delta. Nibble bajo 0 = fin del runlist; nibble
/// alto 0 con longitud > 0 = agujero sparse. Los deltas son acumulativos.
pub fn decode_runs(bytes: &[u8]) -> Option<Vec<Run>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut lcn: i64 = 0;
    loop {
        if pos >= bytes.len() {
            return None; // runlist sin terminador
        }
        let hdr = bytes[pos];
        pos += 1;
        let len_n = hdr & 0x0F;
        let off_n = (hdr >> 4) & 0x0F;
        if len_n == 0 {
            return Some(out); // fin del runlist
        }
        if len_n > 8 || off_n > 8 || pos + len_n as usize + off_n as usize > bytes.len() {
            return None; // truncado o longitud desmesurada
        }
        let mut clusters = 0u64;
        for i in 0..len_n as usize {
            clusters |= (bytes[pos + i] as u64) << (8 * i);
        }
        pos += len_n as usize;
        let mut delta = 0i64;
        for i in 0..off_n as usize {
            delta |= (bytes[pos + i] as i64) << (8 * i);
        }
        pos += off_n as usize;
        if off_n > 0 && (delta >> (8 * off_n - 1)) & 1 == 1 {
            delta -= 1i64 << (8 * off_n); // complemento a 2
        }
        if off_n == 0 {
            out.push(Run { clusters, lcn: 0, hole: true });
        } else {
            lcn += delta;
            out.push(Run { clusters, lcn, hole: false });
        }
    }
}

// ------------------------------------------------ registro MFT (raw, tolerant) --

/// Registro MFT decodificado de forma tolerante: si algo no cuadra se anota
/// y se continúa (RF-5 de la spec); jamás aborta un recorrido.
#[derive(Debug, Clone)]
pub struct MftRecord {
    pub deleted: bool,
    pub is_directory: bool,
    /// false si la array USA no cuadró: el registro puede estar corrupto.
    pub fixups_ok: bool,
    pub has_attr_list: bool,
    pub name: Option<String>,
    /// Nº de registro padre (bits bajos de la referencia $FILE_NAME).
    pub parent: Option<u64>,
    /// Tamaño real declarado en $FILE_NAME.
    pub real_size: u64,
    /// Runlists $DATA no residentes concatenadas (vacío si residente).
    pub data_runs: Vec<Run>,
    /// El registro tiene atributo $BITMAP residente (caso $MFT).
    pub has_bitmap: bool,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn utf16_string(b: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i + 1 < b.len() {
        let u = u16::from_le_bytes([b[i], b[i + 1]]);
        if u == 0 {
            break;
        }
        out.push(char::from_u32(u as u32).unwrap_or('?'));
        i += 2;
    }
    out
}

/// Aplica fixups USA de forma defensiva: acepta tanto la colocación de la
/// array tras USA[0] (offset 8 + 2i) como la declarada por el campo
/// `usa_offset`, y también registros ya fixupeados (coinciden con USA[0]).
/// Si ningún candidato cuadra, marca `fixups_ok = false` y deja los bytes
/// tal cual — nunca aborta el parseo.
fn apply_fixups(buf: &mut [u8]) -> bool {
    let usa0 = le16(buf, 8);
    let count = le16(buf, 6);
    let usa_off = {
        let o = le16(buf, 4);
        if (8..64).contains(&o) { o as usize } else { 8 }
    };
    let mut ok = true;
    let blocks = buf.len() / 512;
    let n = (count.saturating_sub(1) as usize).min(blocks.saturating_sub(1));
    for i in 1..=n {
        let tail = i * 512 - 2;
        if tail + 2 > buf.len() {
            break;
        }
        let stored = le16(buf, tail);
        let c1 = 8 + i * 2;
        let c2 = usa_off + (i - 1) * 2;
        if stored == usa0 {
            continue; // ya fixupeado (lectura pasada por un lector USA)
        }
        if (c1 + 2 <= buf.len() && stored == le16(buf, c1))
            || (c2 + 2 <= buf.len() && stored == le16(buf, c2))
        {
            buf[tail..tail + 2].copy_from_slice(&usa0.to_le_bytes());
        } else {
            ok = false;
        }
    }
    ok
}

const ATTR_ATTR_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_INDEX_ROOT: u32 = 0x90;
const ATTR_INDEX_ALLOC: u32 = 0xA0;
const ATTR_BITMAP: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// Parsea un registro MFT raw (tamaño `buf.len()`, 1024 o cluster).
/// `None` solo si no es un registro reconocible (sin `FILE`, cabecera fuera
/// de rango); un atributo malformado corta la lista, no el registro.
/// `number` se acepta por claridad de llamada; el registro no lo repite.
pub fn parse_mft_record(_number: u64, raw: &[u8]) -> Option<MftRecord> {
    if raw.len() < 512 || raw[0..4] != *b"FILE" {
        return None;
    }
    let mut buf = raw.to_vec();
    let fixups_ok = apply_fixups(&mut buf);
    let flags = le16(&buf, 0x1C);
    let attrs_off = le16(&buf, 0x1E) as usize;
    // La guarda clásica: attrs_offset % 8 == 0 y ≥ 42 (cabecera + holgura).
    if attrs_off < 42 || !attrs_off.is_multiple_of(8) || attrs_off >= buf.len() {
        return None;
    }
    let in_use = flags & 0x01 != 0;
    let mut rec = MftRecord {
        deleted: !in_use,
        is_directory: flags & 0x02 != 0,
        fixups_ok,
        has_attr_list: false,
        name: None,
        parent: None,
        real_size: 0,
        data_runs: Vec::new(),
        has_bitmap: false,
    };
    let mut pos = attrs_off;
    while pos + 16 <= buf.len() {
        let atype = le32(&buf, pos);
        let alen = le32(&buf, pos + 4) as usize;
        if atype == ATTR_END || alen < 16 || pos + alen > buf.len() || !alen.is_multiple_of(8) {
            break; // lista terminada o corrupta: preservamos lo parsado
        }
        let non_resident = buf[pos + 8] != 0;
        match atype {
            ATTR_FILE_NAME if !non_resident => {
                // valor residente: parent u64, 4×time, alloc u64, real u64,
                // flags u32, ea u32, max_compr u32, encoded u32, len u8, ns u8.
                let voff = pos + { le32(&buf, pos + 20) as usize };
                // `vlen` no se usa como guarda: preferimos bounds-check por
                // campo (tolerante, RF-5) frente a un valor posiblemente herido.
                let _vlen = le32(&buf, pos + 16) as usize;
                if voff + 0x42 <= buf.len() {
                    let parent_ref = le64(&buf, voff);
                    rec.parent = Some(parent_ref & 0xFFFF_FFFF_FFFF);
                    rec.real_size = le64(&buf, voff + 48);
                    let name_len = buf[voff + 0x40] as usize;
                    let name_at = voff + 0x42;
                    if name_at + name_len * 2 <= buf.len() {
                        rec.name = Some(utf16_string(&buf[name_at..name_at + name_len * 2]));
                    }
                    if le32(&buf, voff + 56) & 0x02 != 0 {
                        rec.is_directory = true; // marca de directorio en $FILE_NAME
                    }
                }
            }
            ATTR_DATA if !non_resident => {
                // $DATA residente: sin runlist; el tamaño real está en el valor.
                let vlen = le32(&buf, pos + 16) as usize;
                if rec.real_size == 0 {
                    rec.real_size = vlen as u64;
                }
            }
            ATTR_DATA => {
                let rl_off = pos + le16(&buf, pos + 32) as usize;
                let initialized = le64(&buf, pos + 56);
                if rl_off < buf.len() {
                    if let Some(runs) = decode_runs(&buf[rl_off..]) {
                        rec.data_runs.extend(runs);
                    }
                }
                if initialized != 0 {
                    rec.real_size = rec.real_size.max(initialized);
                }
            }
            ATTR_INDEX_ROOT | ATTR_INDEX_ALLOC => rec.is_directory = true,
            ATTR_ATTR_LIST => rec.has_attr_list = true,
            ATTR_BITMAP if !non_resident => rec.has_bitmap = true,
            _ => {} // $STANDARD_INFO y demás: no hacen falta para el informe
        }
        pos += alen;
    }
    Some(rec)
}

// ------------------------------------------------ estado desde el mapfile ----

/// Estado agregado de un rango de sectores según el mapfile: cuántos son
/// `+`, cuántos `-` (huecos sin mapear incluidos) y cuántos `?`. La capa
/// lógica etiqueta estructuras sin reabrir medio alguno.
pub fn range_status(m: &MapFile, start: u64, len: u64) -> (u64, u64, u64) {
    if len == 0 {
        return (0, 0, 0);
    }
    let end = start.saturating_add(len).min(m.sector_count);
    if start >= end {
        return (0, 0, 0);
    }
    let entries = m.entries();
    // primer rango que termina después de `start`
    let mut i = entries.partition_point(|e| e.start.saturating_add(e.len) <= start);
    let (mut g, mut f, mut s) = (0u64, 0u64, 0u64);
    let mut pos = start;
    while pos < end {
        if i < entries.len() && entries[i].start <= pos {
            let e = &entries[i];
            let upto = e.start.saturating_add(e.len).min(end).max(pos);
            match e.status {
                MapStatus::Good => g += upto - pos,
                MapStatus::Failed => f += upto - pos,
                MapStatus::Scraped => s += upto - pos,
            }
            pos = upto;
            i += 1;
        } else {
            // hueco sin mapear: pendiente.
            let next = if i < entries.len() { entries[i].start.min(end) } else { end };
            f += next - pos;
            pos = next;
        }
    }
    (g, f, s)
}

/// Etiqueta legible para un rango a partir del recuento de estados.
pub fn status_label(g: u64, f: u64, s: u64) -> &'static str {
    let total = g + f + s;
    if total == 0 {
        return "sin dato";
    }
    if g == total {
        return "terminado";
    }
    if f == 0 {
        return "agotado";
    }
    if s == 0 {
        return "pendiente";
    }
    "mixto"
}

// ------------------------------------------------------------------ tests ----

// ------------------------------------------------------------------- fuente --

/// Vista de partición: los LBA lógicos (512 B) se cuentan desde el inicio de
/// la partición. Las lecturas se desplazan; las escrituras están prohibidas
/// (esta capa es de solo lectura, RF-7).
struct PartitionView<'a> {
    src: &'a mut dyn SectorSource,
    base: u64,
}

impl SectorSource for PartitionView<'_> {
    fn sector_count(&self) -> u64 {
        self.src.sector_count().saturating_sub(self.base)
    }
    fn phys_block(&self) -> usize {
        self.src.phys_block()
    }
    fn media(&self) -> crate::source::MediaKind {
        self.src.media()
    }
    fn read_sector(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> io::Result<()> {
        match self.base.checked_add(lba) {
            Some(abs) => self.src.read_sector(abs, buf),
            None => Err(io::Error::new(io::ErrorKind::InvalidInput, "LBA fuera de la partición")),
        }
    }
    fn write_sector(&mut self, _lba: u64, _buf: &[u8; SECTOR_SIZE]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "la capa lógica fs nunca escribe",
        ))
    }
}

fn fs_err(msg: String) -> ExitCode {
    eprintln!("{msg}");
    ExitCode::from(2)
}

// ------------------------------------------------------- caminante de la $MFT --

/// Entrada de la $MFT con su estado según el mapfile.
#[derive(Debug)]
pub struct MftEntry {
    pub number: u64,
    pub name: String,
    pub parent: Option<u64>,
    pub is_directory: bool,
    pub deleted: bool,
    pub real_size: u64,
    pub sector_start: u64,
    pub sectors: u64,
    pub map_status: &'static str,
    pub fixups_ok: bool,
    pub data_runs: Vec<Run>,
}

#[derive(Debug)]
pub struct MftWalk {
    pub entries: Vec<MftEntry>,
    pub mft_start: u64,
    pub mft_sectors: u64,
    pub records_scanned: u64,
    pub records_bad: u64,
    pub window_reads_failed: u64,
    pub truncated: bool,
}

fn rec_sectors(layout: &NtfsBpb) -> usize {
    (layout.mft_record_size / SECTOR_SIZE).max(1)
}

/// Extensión física (sectores) descrita por una runlist de clusters.
fn extent_from_runs(runs: &[Run], spc: u64) -> Option<(u64, u64)> {
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    let mut any = false;
    for r in runs {
        if r.hole || r.lcn < 0 {
            continue;
        }
        any = true;
        let s = r.lcn as u64 * spc;
        hi = hi.max(s.saturating_add(r.clusters * spc));
        lo = lo.min(s);
    }
    if !any {
        None
    } else {
        Some((lo, hi - lo))
    }
}

/// Extensión de la $MFT a partir del registro 0 o de su espejo (el registro 0
/// de la MFTMirr es copia exacta del de la $MFT, incluidos sus runs).
fn mft_extent(src: &mut dyn SectorSource, layout: &NtfsBpb) -> Option<(u64, u64)> {
    let rs = rec_sectors(layout);
    for pos in [layout.mft_sector, layout.mftmirr_sector] {
        if pos >= src.sector_count() {
            continue;
        }
        let mut buf = vec![0u8; rs * SECTOR_SIZE];
        if src.read_chunk(pos, rs, &mut buf).is_err() {
            continue;
        }
        if let Some(rec) = parse_mft_record(0, &buf) {
            if let Some(ext) = extent_from_runs(&rec.data_runs, layout.sectors_per_cluster as u64) {
                return Some(ext);
            }
        }
    }
    None
}

/// Recorrido completo (no navegación B+tree) de la $MFT por ventanas de 1 MiB.
/// Un registro herido solo pierde ese registro; la lectura corta de una
/// ventana no aborta el recorrido (RF-5). `max_entries` acota la memoria.
pub fn walk_mft(
    src: &mut dyn SectorSource,
    map: Option<&MapFile>,
    layout: &NtfsBpb,
    max_entries: usize,
) -> Result<MftWalk, String> {
    let (mft_start, mft_sectors) =
        mft_extent(src, layout).ok_or("$MFT no es legible ni a través de su espejo (MFTMirr)")?;
    let rs = rec_sectors(layout);
    let window = 2048usize; // 1 MiB
    let mut w = MftWalk {
        entries: Vec::new(),
        mft_start,
        mft_sectors,
        records_scanned: 0,
        records_bad: 0,
        window_reads_failed: 0,
        truncated: false,
    };
    let mut n = 0u64;
    let mut sec = 0u64;
    while sec < mft_sectors {
        let wlen = window.min((mft_sectors - sec) as usize).max(1);
        let mut buf = vec![0u8; wlen * SECTOR_SIZE];
        if src.read_chunk(mft_start + sec, wlen, &mut buf).is_err() {
            // sector a sector: preservamos lo legible del ventana herida
            let mut tmp = [0u8; SECTOR_SIZE];
            for i in 0..wlen {
                if let Ok(()) = src.read_sector(mft_start + sec + i as u64, &mut tmp) {
                    buf[i * SECTOR_SIZE..(i + 1) * SECTOR_SIZE].copy_from_slice(&tmp);
                } else {
                    w.window_reads_failed += 1;
                }
            }
        }
        let recs = wlen * SECTOR_SIZE / layout.mft_record_size;
        for k in 0..recs {
            let off = k * layout.mft_record_size;
            let rec_no = n + k as u64;
            w.records_scanned += 1;
            let rec_sector = mft_start + (sec as usize + off / SECTOR_SIZE) as u64;
            match parse_mft_record(rec_no, &buf[off..off + layout.mft_record_size]) {
                Some(rec) => {
                    let name = match rec.name.clone() {
                        Some(n) if !n.is_empty() => n,
                        _ => {
                            w.records_bad += 1;
                            continue;
                        }
                    };
                    let st = match map {
                        Some(m) => {
                            let (g, f, s) = range_status(m, rec_sector, rs as u64);
                            status_label(g, f, s)
                        }
                        None => "sin dato",
                    };
                    w.entries.push(MftEntry {
                        number: rec_no,
                        name,
                        parent: rec.parent,
                        is_directory: rec.is_directory,
                        deleted: rec.deleted,
                        real_size: rec.real_size,
                        sector_start: rec_sector,
                        sectors: rs as u64,
                        map_status: st,
                        fixups_ok: rec.fixups_ok,
                        data_runs: rec.data_runs,
                    });
                    if w.entries.len() >= max_entries {
                        w.truncated = true;
                        return Ok(w);
                    }
                }
                None => w.records_bad += 1,
            }
        }
        n += recs as u64;
        sec += wlen as u64;
    }
    Ok(w)
}

// ------------------------------------------------------------- árbol de rutas --

pub fn children_map(w: &MftWalk) -> HashMap<u64, Vec<usize>> {
    let mut ch: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, e) in w.entries.iter().enumerate() {
        if let Some(p) = e.parent {
            ch.entry(p).or_default().push(i);
        }
    }
    for v in ch.values_mut() {
        v.sort_by(|a, b| {
            let (ea, eb) = (&w.entries[*a], &w.entries[*b]);
            eb.is_directory
                .cmp(&ea.is_directory)
                .then(ea.name.to_lowercase().cmp(&eb.name.to_lowercase()))
        });
    }
    ch
}

/// La raíz es el registro cuyo nombre es `.` y cuyo padre es él mismo;
/// en su defecto el número 5 (posición canónica de la raíz en NTFS).
pub fn root_index(w: &MftWalk) -> Option<usize> {
    w.entries
        .iter()
        .position(|e| e.name == "." && e.parent == Some(e.number))
        .or_else(|| w.entries.iter().position(|e| e.number == 5))
}

pub fn path_of(w: &MftWalk, idx: usize) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut cur = idx;
    loop {
        let e = &w.entries[cur];
        if !seen.insert(e.number) || parts.len() >= 32 {
            break; // ciclo o profundidad excesiva
        }
        if e.name != "." {
            parts.push(&e.name);
        }
        match e.parent {
            Some(p) if p != e.number => match w.entries.iter().position(|x| x.number == p) {
                Some(pi) => cur = pi,
                None => break,
            },
            _ => break,
        }
    }
    parts.reverse();
    let mut out = String::from("/");
    out.push_str(&parts.join("/"));
    out
}

pub fn resolve_path(w: &MftWalk, ch: &HashMap<u64, Vec<usize>>, root: usize, path: &str) -> Option<usize> {
    let mut cur = root;
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        let seg_lc = seg.to_lowercase();
        let found = ch
            .get(&w.entries[cur].number)
            .and_then(|kids| {
                kids.iter()
                    .find(|&&k| w.entries[k].name.to_lowercase() == seg_lc)
                    .copied()
            });
        cur = found?;
    }
    Some(cur)
}

// ------------------------------------------------- localización sin BPB legible --

/// Con la posición física `sa` de un registro `$MFT` y su primer run no hueco
/// `la`, el tamaño de cluster se deriva directo: spc = sa / la. Se expresa
/// como par contra el origen del dominio (0,0) para reutilizar la fórmula.
fn spc_from_self(sa: u64, la: u64) -> Option<u32> {
    derive_spc_from_mft_pair(sa, la, 0, 0)
}

/// Búsqueda de la `$MFT` cuando el BPB no es legible: escanea las primeras
/// 16 MiB del dominio (donde casi siempre está el espejo MFTMirr) y una
/// ventana central de 2 MiB (donde suele estar la $MFT en volúmenes grandes).
/// Si encuentra un registro `$MFT`, deriva geometría y devuelve un layout.
pub fn locate_mft_by_scan(src: &mut dyn SectorSource) -> Option<(NtfsBpb, &'static str)> {
    let total = src.sector_count();
    let mut windows: Vec<(u64, u64)> = Vec::new();
    windows.push((16, 16 + 32_768));
    if total > 65_536 {
        let mid = total / 2;
        windows.push((mid - 1024, mid + 1024));
    }
    for rec_size in [1024usize, 4096] {
        for (from, to) in &windows {
            let to = (*to).min(total);
            let mut buf = vec![0u8; rec_size];
            let mut pos = *from;
            while pos + (rec_size / SECTOR_SIZE) as u64 <= to {
                if src
                    .read_chunk(pos, rec_size / SECTOR_SIZE, &mut buf)
                    .is_ok()
                {
                    if let Some(rec) = parse_mft_record(0, &buf) {
                        if rec.name.as_deref() == Some("$MFT") {
                            if let Some(first) = rec
                                .data_runs
                                .iter()
                                .find(|r| !r.hole && r.lcn > 0)
                            {
                                let la = first.lcn as u64;
                                if let Some(spc) = spc_from_self(pos, la) {
                                    let mut bpb = NtfsBpb {
                                        sector_size: SECTOR_SIZE,
                                        sectors_per_cluster: spc,
                                        sectors_nbr: total.saturating_sub(1),
                                        mft_lcn: la,
                                        mftmirr_lcn: 0,
                                        clusters_per_mft_record: -10,
                                        clusters_per_index_record: -16,
                                        mft_record_size: rec_size,
                                        cluster_size: spc as usize * SECTOR_SIZE,
                                        mft_sector: pos,
                                        mftmirr_sector: 0,
                                        backup_boot_sector: total.saturating_sub(1),
                                        cluster_count: total.saturating_sub(1) / spc as u64,
                                    };
                                    bpb.mftmirr_sector = bpb.mftmirr_lcn * spc as u64;
                                    return Some((bpb, "derivado por espejo $MFT/$MFTMirr (BPB ilegible)"));
                                }
                            }
                        }
                    }
                }
                pos += (rec_size / SECTOR_SIZE) as u64;
            }
        }
    }
    None
}

// ------------------------------------------------------------------ informe ----

/// Tramos no terminados del mapfile (huecos incluidos), fusionados y con
/// etiqueta legible. Base del `fs report`. Los huecos entre rangos y el
/// final sin mapear cuentan como pendientes.
pub fn pending_ranges(m: &MapFile) -> Vec<(u64, u64, &'static str)> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut pos = 0u64;
    let mut cur: Option<(u64, u64)> = None; // corrida pendiente (inicio, longitud)
    for e in m.entries() {
        if e.start > pos {
            // hueco previo: pendiente; abre o prolonga la corrida.
            let s = cur.map(|(s, _)| s).unwrap_or(pos);
            cur = Some((s, e.start - s));
        }
        let end = e.start.saturating_add(e.len);
        if e.status == MapStatus::Good {
            if let Some((s, l)) = cur.take() {
                runs.push((s, l));
            }
            pos = pos.max(end);
        } else {
            let s = cur.map(|(s, _)| s).unwrap_or(e.start);
            cur = Some((s, end - s));
            pos = pos.max(end);
        }
    }
    if let Some((s, _)) = cur {
        // la corrida se prolonga por el final sin mapear del dominio.
        runs.push((s, m.sector_count.saturating_sub(s)));
    } else if pos < m.sector_count {
        runs.push((pos, m.sector_count - pos));
    }
    runs.into_iter()
        .map(|(s, l)| {
            let (g, f, x) = range_status(m, s, l);
            (s, l, status_label(g, f, x))
        })
        .collect()
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Informe JSONL tramo→metadatos tocados (artefacto de la Fase 4).
/// Un registro puede aparecer como `registro` (su hueco en la $MFT) y como
/// `datos` (sus runlists fuera de la $MFT).
pub fn write_report(w: &MftWalk, layout: &NtfsBpb, map: &MapFile, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let spc = layout.sectors_per_cluster as u64;
    let mut spans: Vec<(u64, u64, usize)> = Vec::new(); // (inicio, fin, idx de entrada)
    for (i, e) in w.entries.iter().enumerate() {
        spans.push((e.sector_start, e.sector_start + e.sectors, i));
        for r in &e.data_runs {
            if !r.hole && r.lcn >= 0 {
                let s = r.lcn as u64 * spc;
                spans.push((s, s.saturating_add(r.clusters * spc), i));
            }
        }
    }
    spans.sort();
    let ranges = pending_ranges(map);
    let mut por_tramo: HashMap<usize, (std::collections::HashSet<usize>, usize)> = HashMap::new();
    for (s, e, idx) in &spans {
        // primer tramo cuyo fin es posterior al inicio del span; los tramos
        // son ordenados y no solapados, así que basta avanzar mientras el
        // tramo empiece antes del fin del span.
        let mut j = ranges.partition_point(|r| r.0.saturating_add(r.1) <= *s);
        while j < ranges.len() && ranges[j].0 < *e {
            let entry = por_tramo.entry(j).or_insert_with(|| (HashSet::new(), 0));
            entry.1 += 1;
            entry.0.insert(*idx);
            j += 1;
        }
    }
    let mut con_meta = 0usize;
    for (r, (idxs, total)) in ranges.iter().zip(
        (0..ranges.len()).map(|j| {
            por_tramo
                .get(&j)
                .cloned()
                .unwrap_or((HashSet::new(), 0))
        }),
    ) {
        if !idxs.is_empty() {
            con_meta += 1;
        }
        let mut items: Vec<&usize> = idxs.iter().collect();
        items.sort();
        let items_str = items
            .iter()
            .take(50)
            .map(|&&i| {
                let e = &w.entries[i];
                format!(
                    "{{\"ref\":{},\"nombre\":\"{}\",\"path\":\"{}\",\"tipo\":\"{}\"}}",
                    e.number,
                    json_escape(&e.name),
                    json_escape(&path_of(w, i)),
                    if e.is_directory { "directorio" } else { "archivo" }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        writeln!(
            out,
            "{{\"tramo\":[{},{}],\"estado\":\"{}\",\"entradas_total\":{},\"entradas\":[{}{}]}}",
            r.0,
            r.1,
            r.2,
            total,
            items_str,
            if total > 50 { ",…" } else { "" }
        )?;
    }
    writeln!(
        out,
        "{{\"resumen\":{{\"tramos\":{},\"tramos_con_metadatos\":{}, \"registros_escaneados\":{}, \"registros_no_parseables\":{}, \"lecturas_de_ventana_fallidas\":{}}}}}",
        ranges.len(),
        con_meta,
        w.records_scanned,
        w.records_bad,
        w.window_reads_failed
    )?;
    Ok(())
}

// --------------------------------------------------------------------- CLI -----

fn fs_usage() -> ExitCode {
    eprintln!(
        "uso: regrow fs <status|ls|report> <imagen|--raw N|--dev PATH> [opciones]\n\
         \x20 solo lectura: estos comandos jamás escriben en medios, imágenes ni mapas.\n\
         \n\
         \x20 status  firmas y layout (MBR/GPT, NTFS/FAT32) con estado según el mapa\n\
         \x20 ls      listado de la $MFT sin montar (borrados marcados como [borrado])\n\
         \x20 report  JSONL tramo-pendiente → metadatos afectados (necesita --map)\n\
         \n\
         opciones:\n\
         \x20 --map M          mapfile para etiquetar el estado de cada estructura\n\
         \x20 --partition N    trabajar sobre la partición N (1.ª, 2.ª…) en lugar del sector 0\n\
         \x20 --path RUTA      subdirectorio a listar en ls (defecto /)\n\
         \x20 --out F          escribir el informe en F (defecto: salida estándar)\n\
         \x20 --max-entries N  tope de entradas del recorrido (defecto 1.000.000)\n"
    );
    ExitCode::from(2)
}

struct FsArgs {
    image: Option<String>,
    raw: Option<u32>,
    dev: Option<String>,
    map: Option<PathBuf>,
    partition: Option<usize>,
    path: String,
    out: Option<PathBuf>,
    max_entries: usize,
}

fn parse_fs_args(rest: &[String]) -> Result<FsArgs, String> {
    let mut a = FsArgs {
        image: None,
        raw: None,
        dev: None,
        map: None,
        partition: None,
        path: "/".into(),
        out: None,
        max_entries: 1_000_000,
    };
    let mut i = 0usize;
    while i < rest.len() {
        let flag = rest[i].as_str();
        // consume el valor del flag: avanza i sobre él (como los CLI del motor)
        let mut next = |name: &str| -> Result<String, String> {
            if i + 1 < rest.len() {
                i += 1;
                Ok(rest[i].clone())
            } else {
                Err(format!("falta valor para {name}"))
            }
        };
        match flag {
            "--map" => a.map = Some(PathBuf::from(next("--map")?)),
            "--dev" => a.dev = Some(next("--dev")?),
            "--raw" => {
                a.raw = Some(next("--raw")?.parse().map_err(|_| "--raw necesita un número de medio".to_string())?)
            }
            "--partition" => {
                a.partition = Some(next("--partition")?.parse().map_err(|_| "--partition necesita un número".to_string())?)
            }
            "--path" => a.path = next("--path")?,
            "--out" => a.out = Some(PathBuf::from(next("--out")?)),
            "--max-entries" => {
                a.max_entries = next("--max-entries")?.parse().map_err(|_| "--max-entries necesita un número".to_string())?
            }
            _ if flag.starts_with('-') => return Err(format!("flag desconocido: {flag}")),
            _ => {
                if a.image.is_some() {
                    return Err(format!("argumento de más: {flag}"));
                }
                a.image = Some(flag.to_string());
            }
        }
        i += 1;
    }
    Ok(a)
}

/// Abre SIEMPRE en modo lectura (RF-9): imágenes con open_ro, medios crudos
/// con la cascada segura de RawDisk (sin escritura).
fn open_ro(fs: &FsArgs) -> Result<(String, Box<dyn SectorSource>), (String, ExitCode)> {
    if let Some(n) = fs.raw {
        let name = RawDisk::native_name(n);
        return match RawDisk::open(Path::new(&name), false) {
            Ok(s) => Ok((name.clone(), Box::new(s))),
            Err(e) => Err((format!("error abriendo {name}: {e}"), ExitCode::from(1))),
        };
    }
    if let Some(p) = &fs.dev {
        let path = PathBuf::from(p);
        return match RawDisk::open(&path, false) {
            Ok(s) => Ok((p.clone(), Box::new(s))),
            Err(e) => Err((format!("error abriendo {p}: {e}"), ExitCode::from(1))),
        };
    }
    if let Some(p) = &fs.image {
        let path = PathBuf::from(p);
        return match FileImage::open_ro(&path) {
            Ok(s) => Ok((p.clone(), Box::new(s))),
            Err(e) => Err((format!("error abriendo {p}: {e}"), ExitCode::from(2))),
        };
    }
    Err(("error: falta la imagen o --raw/--dev".into(), ExitCode::from(2)))
}

fn read_sectors(src: &mut dyn SectorSource, at: u64, count: usize) -> Option<Vec<u8>> {
    if count == 0 || at >= src.sector_count() {
        return None;
    }
    let mut buf = vec![0u8; count * SECTOR_SIZE];
    src.read_chunk(at, count, &mut buf).ok().map(|_| buf)
}

/// Estado de una posición concreta del layout según el mapa.
fn pos_status(map: Option<&MapFile>, sector: u64, len: u64) -> String {
    match map {
        Some(m) => {
            let (g, f, s) = range_status(m, sector, len);
            status_label(g, f, s).to_string()
        }
        None => "—".to_string(),
    }
}

fn cmd_status(a: &FsArgs) -> ExitCode {
    let (label, mut src) = match open_ro(a) {
        Ok(v) => v,
        Err((e, _)) => return fs_err(e),
    };
    let map = match &a.map {
        Some(p) => match MapFile::load(p) {
            Ok(m) => Some(m),
            Err(e) => return fs_err(e),
        },
        None => None,
    };
    println!("fuente: {label} ({} sectores)", src.sector_count());

    // 1) capa de particionado en el sector 0 / LBA 1
    let s0 = match read_sectors(&mut *src, 0, 1) {
        Some(b) => b,
        None => return fs_err("no se pudo leer el sector 0".into()),
    };
    let mbr = if detect_boot(&s0) == BootKind::Unknown {
        Mbr::parse(&s0)
    } else {
        // El sector 0 ya es un BPB (partición suelta, el caso del campo):
        // los bytes 446.. son código de arranque, no tabla de particiones.
        if a.partition.is_some() {
            return fs_err("el dominio es una partición suelta (firma de FS en el sector 0): no hay tabla de particiones que seleccionar".into());
        }
        None
    };
    let mut base = 0u64;
    let mut base_note = String::from("sector 0 del dominio");
    let mut gpt_shown = false;
    if src.sector_count() > 1 {
        if let Some(h) = read_sectors(&mut *src, 1, 1) {
            if h[0..8] == *b"EFI PART" {
                let num = u32::from_le_bytes(h[80..84].try_into().unwrap()) as usize;
                let esz = u32::from_le_bytes(h[84..88].try_into().unwrap()) as usize;
                let elba = u64::from_le_bytes(h[72..80].try_into().unwrap());
                let entries = read_sectors(&mut *src, elba, (num * esz).div_ceil(SECTOR_SIZE).min(128));
                if let Some(g) = parse_gpt(&h, entries.as_deref().unwrap_or(&[])) {
                    gpt_shown = true;
                    println!(
                        "GPT: header {} CRC {}, entradas {} CRC {} (declaradas {})",
                        g.my_lba,
                        if g.header_crc_ok { "OK" } else { "ROTO" },
                        elba,
                        if g.entries_crc_ok { "OK" } else { "ROTO" },
                        g.entries_declared
                    );
                    println!(
                        "  dominio utilizable {}..{} · array de entradas en LBA {}",
                        g.first_usable, g.last_usable, g.entries_lba
                    );
                    if g.alternate_lba < src.sector_count() {
                        if let Some(bh) = read_sectors(&mut *src, g.alternate_lba, 1) {
                            let bnum = u32::from_le_bytes(bh[80..84].try_into().unwrap()) as usize;
                            let besz = u32::from_le_bytes(bh[84..88].try_into().unwrap()) as usize;
                            let belba = u64::from_le_bytes(bh[72..80].try_into().unwrap());
                            let bentries = read_sectors(&mut *src, belba, bnum * besz / SECTOR_SIZE + 1);
                            match parse_gpt(&bh, bentries.as_deref().unwrap_or(&[])) {
                                Some(bg) => println!(
                                    "GPT backup ({}): header CRC {}, entradas CRC {}",
                                    g.alternate_lba,
                                    if bg.header_crc_ok { "OK" } else { "ROTO" },
                                    if bg.entries_crc_ok { "OK" } else { "ROTO" }
                                ),
                                None => println!("GPT backup ({}): ilegible o sin firma", g.alternate_lba),
                            }
                        }
                    }
                    for (i, p) in g.partitions.iter().enumerate() {
                        println!(
                            "  partición {}: {}..{}  {}  «{}»  estado mapa: {}",
                            i + 1,
                            p.start_lba,
                            p.end_lba,
                            p.type_guid,
                            p.name,
                            pos_status(map.as_ref(), p.start_lba, p.end_lba - p.start_lba + 1)
                        );
                    }
                    if let Some(n) = a.partition {
                        match g.partitions.get(n.wrapping_sub(1)) {
                            Some(p) => {
                                base = p.start_lba;
                                base_note = format!("GPT partición {n} (sector {})", p.start_lba);
                            }
                            None => return fs_err(format!("no hay partición GPT {n}")),
                        }
                    }
                }
            }
        }
    }
    if !gpt_shown {
        if let Some(m) = &mbr {
            let vivos: Vec<&MbrEntry> = m.entries.iter().filter(|e| e.sectors > 0 && e.kind != 0).collect();
            if m.is_gpt_protective() {
                println!("MBR protector GPT presente (la tabla GPT no se encontró o está dañada)");
            }
            for (i, e) in vivos.iter().enumerate() {
                println!(
                    "  entrada MBR {}: tipo {:#04X}  {}..{} {}",
                    i + 1,
                    e.kind,
                    e.start_lba,
                    e.start_lba + e.sectors - 1,
                    if e.bootable { "arrancable" } else { "" }
                );
            }
            if let Some(n) = a.partition {
                match m.entries.get(n.wrapping_sub(1)) {
                    Some(e) if e.sectors > 0 => {
                        base = e.start_lba;
                        base_note = format!("MBR entrada {n} (sector {})", e.start_lba);
                    }
                    _ => return fs_err(format!("no hay entrada MBR {n}")),
                }
            }
        }
    }

    // 2) sistema de archivos en la base elegida
    let mut view = PartitionView { src: &mut *src, base };
    println!("base: {base_note}");
    let sec0 = match read_sectors(&mut view, 0, 1) {
        Some(b) => b,
        None => return fs_err("no se pudo leer el sector de arranque de la base".into()),
    };
    match detect_boot(&sec0) {
        BootKind::Ntfs => {
            println!("firma NTFS presente");
            let layout = match parse_ntfs_bpb(&sec0) {
                Some(l) => l,
                None => match locate_mft_by_scan(&mut view) {
                    Some((l, note)) => {
                        println!("BPB ilegible: {note}");
                        l
                    }
                    None => {
                        println!("layout no derivable (BPB ilegible y no se localizó la $MFT)");
                        return ExitCode::SUCCESS;
                    }
                },
            };
            println!(
                "geometría: {} B/sector, {} sectores/cluster (cluster {} B), {} clusters, registro MFT {} B (cpr {})",
                layout.sector_size,
                layout.sectors_per_cluster,
                layout.cluster_size,
                layout.cluster_count,
                layout.mft_record_size,
                layout.clusters_per_mft_record
            );
            println!(
                "  $MFT     sector {:>12}  [{}/mapa]",
                layout.mft_sector,
                pos_status(map.as_ref(), layout.mft_sector, 8 * layout.sectors_per_cluster as u64)
            );
            println!(
                "  MFTMirr  sector {:>12}  [{}/mapa]",
                layout.mftmirr_sector,
                pos_status(map.as_ref(), layout.mftmirr_sector, 8 * layout.sectors_per_cluster as u64)
            );
            println!(
                "  backup boot sector {:>12}  [{}/mapa]",
                layout.backup_boot_sector,
                pos_status(map.as_ref(), layout.backup_boot_sector, 1)
            );
        }
        BootKind::Fat32 => {
            println!("firma FAT32 presente");
            match parse_fat32_bpb(&sec0) {
                Some(l) => {
                    println!(
                        "geometría: {} B/sector, {} sectores/cluster, reservados {}, {} FAT × {} sectores, datos desde {}, clusters raíz {}, fsinfo {}, backup boot +{}",
                        l.sector_size,
                        l.sectors_per_cluster,
                        l.reserved_sectors,
                        l.num_fats,
                        l.fat_size,
                        l.data_start,
                        l.root_cluster,
                        l.fsinfo_sector,
                        l.backup_boot_offset
                    );
                    println!("  estado FS según mapa: [{}]", pos_status(map.as_ref(), 0, 1));
                    println!("  estado FAT según mapa: [{}]", pos_status(map.as_ref(), l.reserved_sectors as u64, 8));
                }
                None => println!("layout no derivable (BPB FAT32 ilegible)"),
            }
        }
        BootKind::Fat16 => println!("firma FAT16 presente (el parseo completo de FAT16 no está en esta versión)"),
        BootKind::ExFat => println!("firma exFAT presente (no soportada por regrow)"),
        BootKind::Unknown => {
            if has_boot_marker(&sec0) {
                println!("sector de arranque con marca 55AA pero sin firma conocida");
            } else {
                println!("sin sector de arranque reconocible en esta base");
            }
        }
    }
    ExitCode::SUCCESS
}

fn ntfs_layout_for(src: &mut dyn SectorSource) -> Result<NtfsBpb, String> {
    let sec0 = read_sectors(src, 0, 1).ok_or("no se pudo leer el sector de arranque")?;
    if detect_boot(&sec0) != BootKind::Ntfs {
        return Err("no hay firma NTFS en esta base (fs ls/report solo soporta NTFS por ahora)".into());
    }
    if let Some(l) = parse_ntfs_bpb(&sec0) {
        return Ok(l);
    }
    locate_mft_by_scan(src)
        .map(|(l, note)| {
            eprintln!("aviso: BPB ilegible; {note}");
            l
        })
        .ok_or_else(|| "BPB ilegible y la $MFT no se localizó".to_string())
}

fn cmd_ls(a: &FsArgs) -> ExitCode {
    let (label, mut src) = match open_ro(a) {
        Ok(v) => v,
        Err((e, _)) => return fs_err(e),
    };
    let map = match &a.map {
        Some(p) => match MapFile::load(p) {
            Ok(m) => Some(m),
            Err(e) => return fs_err(e),
        },
        None => None,
    };
    let base = match a.partition {
        Some(n) => match partition_base(&mut *src, n) {
            Ok(b) => b,
            Err(e) => return fs_err(e),
        },
        None => 0,
    };
    let mut view = PartitionView { src: &mut *src, base };
    let layout = match ntfs_layout_for(&mut view) {
        Ok(l) => l,
        Err(e) => return fs_err(e),
    };
    let w = match walk_mft(&mut view, map.as_ref(), &layout, a.max_entries) {
        Ok(w) => w,
        Err(e) => return fs_err(e),
    };
    let ch = children_map(&w);
    let Some(root) = root_index(&w) else {
        return fs_err("no se encontró el directorio raíz en la $MFT".into());
    };
    println!("fuente: {label} · $MFT en sector {} ({} sectores) · {} registros leídos, {} no parseables", w.mft_start, w.mft_sectors, w.records_scanned, w.records_bad);
    let Some(target) = resolve_path(&w, &ch, root, &a.path) else {
        return fs_err(format!("ruta no encontrada: {}", a.path));
    };
    fn print_tree(
        w: &MftWalk,
        ch: &HashMap<u64, Vec<usize>>,
        idx: usize,
        depth: usize,
        seen: &mut std::collections::HashSet<usize>,
    ) {
        if depth > 32 || !seen.insert(idx) {
            return;
        }
        let e = &w.entries[idx];
        println!(
            "{}{}{:<44} {:<10}{}{}{}",
            "  ".repeat(depth),
            if e.is_directory { "[d] " } else { "    " },
            e.name,
            e.map_status,
            if e.deleted { " [borrado]" } else { "" },
            if e.fixups_ok { "" } else { " [fixups dudosos]" },
            if e.is_directory { String::new() } else { format!("  {} B", e.real_size) }
        );
        if let Some(kids) = ch.get(&e.number) {
            for &k in kids {
                print_tree(w, ch, k, depth + 1, seen);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    print_tree(&w, &ch, target, 0, &mut seen);
    if w.truncated {
        eprintln!("aviso: recorrido truncado en {} entradas (--max-entries)", w.entries.len());
    }
    ExitCode::SUCCESS
}

fn partition_base(src: &mut dyn SectorSource, n: usize) -> Result<u64, String> {
    let s0 = read_sectors(src, 0, 1).ok_or("no se pudo leer el sector 0")?;
    if src.sector_count() > 1 {
        if let Some(h) = read_sectors(src, 1, 1) {
            if h[0..8] == *b"EFI PART" {
                let num = u32::from_le_bytes(h[80..84].try_into().unwrap()) as usize;
                let esz = u32::from_le_bytes(h[84..88].try_into().unwrap()) as usize;
                let elba = u64::from_le_bytes(h[72..80].try_into().unwrap());
                let entries = read_sectors(src, elba, num * esz / SECTOR_SIZE + 1);
                if let Some(g) = parse_gpt(&h, entries.as_deref().unwrap_or(&[])) {
                    return g
                        .partitions
                        .get(n.wrapping_sub(1))
                        .map(|p| p.start_lba)
                        .ok_or(format!("no hay partición GPT {n}"));
                }
            }
        }
    }
    if let Some(m) = Mbr::parse(&s0) {
        return m
            .entries
            .get(n.wrapping_sub(1))
            .filter(|e| e.sectors > 0)
            .map(|e| e.start_lba)
            .ok_or(format!("no hay entrada MBR {n}"));
    }
    Err(format!("no hay tabla de particiones para elegir {n}"))
}

fn cmd_report(a: &FsArgs) -> ExitCode {
    let Some(map_path) = &a.map else {
        return fs_err("fs report necesita --map (la fuente de verdad del daño es el mapa)".into());
    };
    let map = match MapFile::load(map_path) {
        Ok(m) => m,
        Err(e) => return fs_err(e),
    };
    let mut src = match open_ro(a) {
        Ok((_, s)) => s,
        Err((e, _)) => return fs_err(e),
    };
    let base = match a.partition {
        Some(n) => match partition_base(&mut *src, n) {
            Ok(b) => b,
            Err(e) => return fs_err(e),
        },
        None => 0,
    };
    let mut view = PartitionView { src: &mut *src, base };
    let layout = match ntfs_layout_for(&mut view) {
        Ok(l) => l,
        Err(e) => return fs_err(e),
    };
    let w = match walk_mft(&mut view, Some(&map), &layout, a.max_entries) {
        Ok(w) => w,
        Err(e) => return fs_err(e),
    };
    match &a.out {
        Some(p) => {
            let file = match std::fs::File::create(p) {
                Ok(f) => f,
                Err(e) => return fs_err(format!("no se puede escribir {}: {e}", p.display())),
            };
            let mut buf = std::io::BufWriter::new(file);
            match write_report(&w, &layout, &map, &mut buf) {
                Ok(()) => {
                    if buf.flush().is_err() {
                        return fs_err("error al cerrar el informe".into());
                    }
                    eprintln!(
                        "informe escrito en {}: {} registros escaneados, {} entradas nombradas",
                        p.display(),
                        w.records_scanned,
                        w.entries.len()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => fs_err(format!("error escribiendo informe: {e}")),
            }
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            match write_report(&w, &layout, &map, &mut lock) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fs_err(format!("error escribiendo informe: {e}")),
            }
        }
    }
}

pub fn cli_fs(args: &[String]) -> ExitCode {
    let Some(op) = args.first() else { return fs_usage() };
    if matches!(op.as_str(), "help" | "--help" | "-h") {
        return fs_usage();
    }
    let a = match parse_fs_args(&args[1..]) {
        Ok(a) => a,
        Err(e) => return fs_err(format!("error: {e}")),
    };
    match op.as_str() {
        "status" => cmd_status(&a),
        "ls" => cmd_ls(&a),
        "report" => cmd_report(&a),
        _ => fs_usage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapfile::MapFile;

    fn sector0_blank() -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    // ---- firmas ----------------------------------------------------------

    #[test]
    fn detecta_ntfs_fat32_fat16() {
        let mut ntfs = sector0_blank();
        ntfs[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(detect_boot(&ntfs), BootKind::Ntfs);

        let mut fat32 = sector0_blank();
        fat32[82..90].copy_from_slice(b"FAT32   ");
        assert_eq!(detect_boot(&fat32), BootKind::Fat32);

        let mut fat16 = sector0_blank();
        fat16[54..62].copy_from_slice(b"FAT16   ");
        assert_eq!(detect_boot(&fat16), BootKind::Fat16);

        let vacio = sector0_blank();
        assert_eq!(detect_boot(&vacio), BootKind::Unknown);
        assert!(!has_boot_marker(&[0u8; 511]));
        assert!(has_boot_marker(&sector0_blank()));
    }

    // ---- MBR --------------------------------------------------------------

    #[test]
    fn parsea_mbr_con_protector_gpt() {
        let mut s = sector0_blank();
        // entrada 0: protector GPT 0xEE cubriendo el disco
        s[446] = 0x00;
        s[446 + 4] = 0xEE;
        s[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        s[446 + 12..446 + 16].copy_from_slice(&4096u32.to_le_bytes());
        // entrada 1: vacía; entrada 2: arranqueable 0x07
        s[446 + 2 * 16] = 0x80;
        s[446 + 2 * 16 + 4] = 0x07;
        s[446 + 2 * 16 + 8..446 + 2 * 16 + 12].copy_from_slice(&2048u32.to_le_bytes());
        s[446 + 2 * 16 + 12..446 + 2 * 16 + 16].copy_from_slice(&2048u32.to_le_bytes());
        let mbr = Mbr::parse(&s).expect("MBR con marca debe parsear");
        assert!(mbr.is_gpt_protective());
        assert_eq!(mbr.entries[0].kind, 0xEE);
        assert_eq!(mbr.entries[1].kind, 0x00);
        assert!(mbr.entries[2].bootable);
        assert_eq!(mbr.entries[2].start_lba, 2048);
        assert_eq!(mbr.entries[2].sectors, 2048);
        assert!(!mbr.entries[0].bootable);
    }

    #[test]
    fn mbr_sin_marca_y_sin_tabla_no_parsea() {
        let mut s = vec![0u8; 512];
        s[3] = b'x';
        assert!(Mbr::parse(&s).is_none());
    }

    // ---- GPT ---------------------------------------------------------------

    fn gpt_fixture() -> (Vec<u8>, Vec<u8>) {
        // Disco sintético: header en LBA 1, entradas en LBA 2, datos 6..26,
        // backup del header en LBA 33. Una partición de datos + entrada vacía.
        let mut h = vec![0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revisión
        h[12..16].copy_from_slice(&92u32.to_le_bytes()); // tamaño header
        h[24..32].copy_from_slice(&1u64.to_le_bytes()); // my_lba
        h[32..40].copy_from_slice(&33u64.to_le_bytes()); // alternate_lba
        h[40..48].copy_from_slice(&6u64.to_le_bytes()); // first_usable
        h[48..56].copy_from_slice(&26u64.to_le_bytes()); // last_usable
        h[56..72].copy_from_slice(&[7u8; 16]); // GUID disco
        h[72..80].copy_from_slice(&2u64.to_le_bytes()); // entradas LBA
        h[80..84].copy_from_slice(&2u32.to_le_bytes()); // nº entradas
        h[84..88].copy_from_slice(&128u32.to_le_bytes()); // tamaño entrada

        let mut entries = vec![0u8; 256];
        // GUID tipo «basic data» EBD0A0A2-B9E5-4433-87C0-68B6B72699C7
        entries[0..4].copy_from_slice(&0xEBD0_A0A2u32.to_le_bytes());
        entries[4..6].copy_from_slice(&0xB9E5u16.to_le_bytes());
        entries[6..8].copy_from_slice(&0x4433u16.to_le_bytes());
        entries[8..16].copy_from_slice(&[0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7]);
        entries[16..32].copy_from_slice(&[0xAAu8; 16]); // GUID único de partición
        entries[32..40].copy_from_slice(&6u64.to_le_bytes()); // start_lba
        entries[40..48].copy_from_slice(&25u64.to_le_bytes()); // end_lba
        // nombre UTF-16 «DATOS» en 56..
        for (i, c) in "DATOS".encode_utf16().enumerate() {
            entries[56 + i * 2..58 + i * 2].copy_from_slice(&c.to_le_bytes());
        }

        let arr_crc = crc32(&entries);
        h[88..92].copy_from_slice(&arr_crc.to_le_bytes());
        let hdr_crc = crc32(&h[..92]); // campo CRC a 0 durante el cálculo
        h[16..20].copy_from_slice(&hdr_crc.to_le_bytes());
        (h, entries)
    }

    #[test]
    fn gpt_valido_con_crcs() {
        let (h, e) = gpt_fixture();
        let g = parse_gpt(&h, &e).expect("fixture GPT debe parsear");
        assert!(g.header_crc_ok);
        assert!(g.entries_crc_ok);
        assert_eq!(g.my_lba, 1);
        assert_eq!(g.alternate_lba, 33);
        assert_eq!(g.first_usable, 6);
        assert_eq!(g.last_usable, 26);
        assert_eq!(g.entries_lba, 2);
        assert_eq!(g.partitions.len(), 1);
        let p = &g.partitions[0];
        assert_eq!(p.type_guid, "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7");
        assert_eq!(p.unique_guid, "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        assert_eq!(p.name, "DATOS");
        assert_eq!(p.start_lba, 6);
        assert_eq!(p.end_lba, 25);
    }

    #[test]
    fn gpt_header_corrupto_se_detecta() {
        let (mut h, e) = gpt_fixture();
        h[24] ^= 0xFF; // my_lba alterado tras calcular el CRC
        let g = parse_gpt(&h, &e).expect("parsea pero marca el CRC roto");
        assert!(!g.header_crc_ok);
        assert!(g.entries_crc_ok);
    }

    #[test]
    fn gpt_array_corrupto_se_detecta() {
        let (h, mut e) = gpt_fixture();
        e[0] ^= 0xFF; // type_guid alterado
        let g = parse_gpt(&h, &e).expect("parsea pero marca el CRC roto");
        assert!(g.header_crc_ok);
        assert!(!g.entries_crc_ok);
    }

    #[test]
    fn gpt_sin_firma_no_parsea() {
        let (mut h, e) = gpt_fixture();
        h[0] = b'X';
        assert!(parse_gpt(&h, &e).is_none());
    }

    // ---- BPB NTFS ----------------------------------------------------------

    fn ntfs_bpb_fixture() -> Vec<u8> {
        // Mini-volumen NTFS: 4096 sectores de 512 (sectors_nbr=4095, el último
        // es el arranque de respaldo), 8 sectores/cluster, $MFT en cluster 2
        // → sector 16; MFTMirr en cluster 6 → sector 48.
        // clusters_per_mft_record = -10 (0xF6, valor de formato real) → registro
        // de 2^10 = 1024 bytes por la regla canónica (<0 ⇒ 2^|n| bytes).
        let mut s = sector0_blank();
        s[3..11].copy_from_slice(b"NTFS    ");
        s[11..13].copy_from_slice(&512u16.to_le_bytes());
        s[13] = 8; // sectores por cluster
        // 14..16 reserved = 0; campos FAT a cero ya están.
        s[40..48].copy_from_slice(&4095u64.to_le_bytes()); // sectors_nbr
        s[0x30..0x38].copy_from_slice(&2u64.to_le_bytes()); // mft_lcn
        s[0x38..0x40].copy_from_slice(&6u64.to_le_bytes()); // mftmirr_lcn
        s[0x40] = 0xF6; // clusters_per_mft_record = -10 → 1024 bytes
        s[0x44] = 4; // clusters_per_index_record = 4 clusters
        s
    }

    #[test]
    fn bpb_ntfs_deriva_posiciones() {
        let s = ntfs_bpb_fixture();
        let b = parse_ntfs_bpb(&s).expect("BPB NTFS sintético debe parsear");
        assert_eq!(b.sector_size, 512);
        assert_eq!(b.sectors_per_cluster, 8);
        assert_eq!(b.cluster_size, 4096);
        assert_eq!(b.mft_lcn, 2);
        assert_eq!(b.mft_sector, 16);
        assert_eq!(b.mftmirr_sector, 48);
        assert_eq!(b.backup_boot_sector, 4095);
        assert_eq!(b.cluster_count, 4095 / 8);
        assert_eq!(b.mft_record_size, 1024); // regla canónica: -10 ⇒ 2^10
        assert_eq!(b.clusters_per_index_record, 4);
    }

    #[test]
    fn bpb_ntfs_rechaza_incoherentes() {
        // firma presente pero campos FAT sucios
        let mut s = ntfs_bpb_fixture();
        s[16] = 2; // nº de FATs ≠ 0 → no es NTFS coherente
        assert!(parse_ntfs_bpb(&s).is_none());
        // sectors_nbr = 0
        let mut s2 = ntfs_bpb_fixture();
        s2[40..48].copy_from_slice(&0u64.to_le_bytes());
        assert!(parse_ntfs_bpb(&s2).is_none());
        // spc no potencia de 2
        let mut s3 = ntfs_bpb_fixture();
        s3[13] = 3;
        assert!(parse_ntfs_bpb(&s3).is_none());
        // sin marca de arranque
        let mut s4 = ntfs_bpb_fixture();
        s4[510] = 0;
        assert!(parse_ntfs_bpb(&s4).is_none());
    }

    #[test]
    fn spc_derivado_del_par_espejo() {
        // MFT en sector 16 (LCN 2), MFTMirr en sector 48 (LCN 6):
        // Δsector 32 / Δlcn 4 = 8.
        assert_eq!(derive_spc_from_mft_pair(16, 2, 48, 6), Some(8));
        assert_eq!(derive_spc_from_mft_pair(48, 6, 16, 2), Some(8));
        // Δsector 8 / Δlcn 4 = 2 (potencia de 2) → aceptado
        assert_eq!(derive_spc_from_mft_pair(40, 2, 48, 6), Some(2));
        // Δ no divisible → None
        assert_eq!(derive_spc_from_mft_pair(17, 2, 48, 6), None);
        // 36/4 = 9, no potencia de 2 → None
        assert_eq!(derive_spc_from_mft_pair(12, 2, 48, 6), None);
        // mismo LCN (Δ 0) → None
        assert_eq!(derive_spc_from_mft_pair(16, 2, 48, 2), None);
    }

    // ---- BPB FAT32 ----------------------------------------------------------

    #[test]
    fn bpb_fat32_deriva_data_start() {
        let mut s = sector0_blank();
        s[82..90].copy_from_slice(b"FAT32   ");
        s[11..13].copy_from_slice(&512u16.to_le_bytes());
        s[13] = 4; // spc
        s[14..16].copy_from_slice(&32u16.to_le_bytes()); // reservados
        s[16] = 2; // nº FATs
        s[32..36].copy_from_slice(&1_048_576u32.to_le_bytes()); // sectores totales
        s[36..40].copy_from_slice(&1024u32.to_le_bytes()); // tamaño FAT
        s[44..48].copy_from_slice(&2u32.to_le_bytes()); // cluster raíz
        s[48..50].copy_from_slice(&1u16.to_le_bytes()); // fsinfo
        s[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot offset
        let b = parse_fat32_bpb(&s).expect("BPB FAT32 sintético debe parsear");
        assert_eq!(b.sectors_per_cluster, 4);
        assert_eq!(b.num_fats, 2);
        assert_eq!(b.fat_size, 1024);
        assert_eq!(b.data_start, 32 + 2 * 1024);
        assert_eq!(b.total_sectors, 1_048_576);
        assert_eq!(b.backup_boot_offset, 6);
        // firma ausente → None
        s[82] = b'X';
        assert!(parse_fat32_bpb(&s).is_none());
    }

    // ---- estado de rango desde mapfile --------------------------------------

    fn mapa_fixture() -> MapFile {
        // dominio 100: [0,10)+  [20,30)?  [40,50)- ; huecos = pendientes
        let mut m = MapFile::new(100);
        m.set(0, 10, MapStatus::Good, 0);
        m.set(20, 10, MapStatus::Scraped, 3);
        m.set(40, 10, MapStatus::Failed, 0);
        m
    }

    #[test]
    fn range_status_cubre_huecos_y_bordes() {
        let m = mapa_fixture();
        // rango completamente bueno
        assert_eq!(range_status(&m, 2, 5), (5, 0, 0));
        // hueco sin mapear cuenta como pendiente
        assert_eq!(range_status(&m, 10, 10), (0, 10, 0));
        // agotado
        assert_eq!(range_status(&m, 20, 10), (0, 0, 10));
        // mixto: cruza + (0..10), hueco (10..20), ? (20..30)
        assert_eq!(range_status(&m, 5, 25), (5, 10, 10));
        // fuera de dominio: topado
        assert_eq!(range_status(&m, 95, 10), (0, 5, 0));
        // rango más allá del dominio → sin dato
        assert_eq!(range_status(&m, 100, 5), (0, 0, 0));
        // etiqueta
        assert_eq!(status_label(5, 0, 0), "terminado");
        assert_eq!(status_label(0, 10, 0), "pendiente");
        assert_eq!(status_label(0, 0, 10), "agotado");
        assert_eq!(status_label(5, 10, 10), "mixto");
        assert_eq!(status_label(0, 0, 0), "sin dato");
    }

    // ---- mapping pairs ------------------------------------------------------

    #[test]
    fn runlists_golden() {
        // un tramo: longitud 1 byte, delta 1 byte.
        let r = decode_runs(&[0x11, 0x20, 0x02, 0x00]).unwrap();
        assert_eq!(r, vec![Run { clusters: 32, lcn: 2, hole: false }]);
        // dos tramos con delta acumulativo desde el LCN anterior.
        let r = decode_runs(&[0x11, 0x10, 0x05, 0x11, 0x08, 0x10, 0x00]).unwrap();
        assert_eq!(
            r,
            vec![Run { clusters: 16, lcn: 5, hole: false }, Run { clusters: 8, lcn: 21, hole: false }]
        );
        // delta negativo (complemento a 2).
        let r = decode_runs(&[0x11, 0x08, 0x05, 0x11, 0x04, 0xFF, 0x00]).unwrap();
        assert_eq!(
            r,
            vec![Run { clusters: 8, lcn: 5, hole: false }, Run { clusters: 4, lcn: 4, hole: false }]
        );
        // agujero sparse: nibble alto 0 con longitud > 0.
        let r = decode_runs(&[0x11, 0x05, 0x03, 0x01, 0x0A, 0x11, 0x02, 0x20, 0x00]).unwrap();
        assert_eq!(
            r,
            vec![
                Run { clusters: 5, lcn: 3, hole: false },
                Run { clusters: 10, lcn: 0, hole: true },
                Run { clusters: 2, lcn: 35, hole: false }, // 3 + 0x20
            ]
        );
        // longitudes multibyte (len 2, delta 3).
        let r = decode_runs(&[0x32, 0x34, 0x12, 0x21, 0x00, 0x01, 0x00]).unwrap();
        assert_eq!(r, vec![Run { clusters: 0x1234, lcn: 0x010021, hole: false }]);
        // truncado o sin terminador → None.
        assert!(decode_runs(&[0x21, 0x34]).is_none());
        assert!(decode_runs(&[0x11, 0x01, 0x01]).is_none());
        assert!(decode_runs(&[0x91, 0x01, 0x01, 0x01, 0x01]).is_none()); // delta de 9 bytes: inválido
        let big = decode_runs(&[0x81, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00]).unwrap();
        assert_eq!(big[0].clusters, 1);
        assert_eq!(big[0].lcn, 0x0101_0101_0101_0101);
    }

    // ---- registros MFT -------------------------------------------------------

    fn fn_attr(name: &str, parent: u64, dir: bool, real: u64) -> Vec<u8> {
        let nb: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let vlen = 0x42 + nb.len();
        let alen = (24 + vlen + 7) / 8 * 8;
        let mut a = vec![0u8; alen];
        a[0..4].copy_from_slice(&0x30u32.to_le_bytes());
        a[4..8].copy_from_slice(&(alen as u32).to_le_bytes());
        a[16..20].copy_from_slice(&(vlen as u32).to_le_bytes());
        a[20..24].copy_from_slice(&24u32.to_le_bytes()); // value offset
        let v = 24usize;
        a[v..v + 8].copy_from_slice(&parent.to_le_bytes());
        a[v + 48..v + 56].copy_from_slice(&real.to_le_bytes());
        if dir {
            a[v + 56] = 0x02;
        }
        a[v + 64] = (nb.len() / 2) as u8;
        a[v + 65] = 2; // namespace WIN32
        a[v + 66..v + 66 + nb.len()].copy_from_slice(&nb);
        a
    }

    fn data_attr(runs: &[u8], real: u64) -> Vec<u8> {
        let rl_pad = (runs.len() + 7) / 8 * 8;
        let alen = 64 + rl_pad;
        let mut a = vec![0u8; alen];
        a[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[4..8].copy_from_slice(&(alen as u32).to_le_bytes());
        a[8] = 1; // no residente
        a[32..34].copy_from_slice(&64u16.to_le_bytes()); // runlist offset
        a[40..48].copy_from_slice(&real.to_le_bytes()); // asignado
        a[48..56].copy_from_slice(&real.to_le_bytes()); // real
        a[56..64].copy_from_slice(&real.to_le_bytes()); // inicializado
        a[64..64 + runs.len()].copy_from_slice(runs);
        a
    }

    fn index_root_attr() -> Vec<u8> {
        let mut a = vec![0u8; 32];
        a[0..4].copy_from_slice(&0x90u32.to_le_bytes());
        a[4..8].copy_from_slice(&32u32.to_le_bytes());
        a
    }

    fn bitmap_attr() -> Vec<u8> {
        let mut a = vec![0u8; 40];
        a[0..4].copy_from_slice(&0xC0u32.to_le_bytes());
        a[4..8].copy_from_slice(&40u32.to_le_bytes());
        a
    }

    fn rec_bytes(flags: u16, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&16u16.to_le_bytes()); // usa_offset
        r[6..8].copy_from_slice(&3u16.to_le_bytes()); // usa_count
        r[8..10].copy_from_slice(&0xBEEFu16.to_le_bytes()); // USA[0]
        r[0x1C..0x1E].copy_from_slice(&flags.to_le_bytes());
        r[0x1E..0x20].copy_from_slice(&56u16.to_le_bytes()); // attrs_offset
        let mut pos = 56usize;
        for a in attrs {
            assert!(pos + a.len() <= r.len(), "fixture desborda el registro");
            r[pos..pos + a.len()].copy_from_slice(a);
            pos += a.len();
        }
        // colas de bloque con USA[0] (registro ya fixupeado).
        r[510..512].copy_from_slice(&0xBEEFu16.to_le_bytes());
        r[1022..1024].copy_from_slice(&0xBEEFu16.to_le_bytes());
        r
    }

    #[test]
    fn registros_mft_se_parsean() {
        let rec0 = rec_bytes(1, &[fn_attr("$MFT", 0, false, 0), data_attr(&[0x11, 0x08, 0x02, 0x00], 32_768), bitmap_attr()]);
        let mft = parse_mft_record(0, &rec0).expect("$MFT sintética parseable");
        assert_eq!(mft.name.as_deref(), Some("$MFT"));
        assert_eq!(mft.parent, Some(0));
        assert!(!mft.deleted);
        assert!(mft.fixups_ok);
        assert!(mft.has_bitmap);
        assert_eq!(mft.data_runs, vec![Run { clusters: 8, lcn: 2, hole: false }]);

        assert!(parse_mft_record(1, &[7u8; 1024]).is_none(), "basura sin FILE → None");

        let rec2 = rec_bytes(1, &[fn_attr("hola.txt", 5, false, 1234), data_attr(&[0x11, 0x04, 0x0A, 0x00], 2048)]);
        let f = parse_mft_record(2, &rec2).unwrap();
        assert_eq!(f.name.as_deref(), Some("hola.txt"));
        assert_eq!(f.parent, Some(5));
        assert_eq!(f.real_size, 2048); // el max de $FILE_NAME e $DATA inicializado
        assert_eq!(f.data_runs, vec![Run { clusters: 4, lcn: 10, hole: false }]);
        assert!(!f.is_directory);

        let rec3 = rec_bytes(1, &[fn_attr("datos", 5, true, 0), index_root_attr()]);
        let d = parse_mft_record(3, &rec3).unwrap();
        assert!(d.is_directory);

        let rec4 = rec_bytes(0, &[fn_attr("borrado.txt", 5, false, 99)]);
        let del = parse_mft_record(4, &rec4).unwrap();
        assert!(del.deleted);
        assert_eq!(del.name.as_deref(), Some("borrado.txt"));

        let rec5 = rec_bytes(1, &[fn_attr(".", 5, true, 0), index_root_attr()]);
        let root = parse_mft_record(5, &rec5).unwrap();
        assert!(root.is_directory);
        assert_eq!(root.parent, Some(5));
    }

    #[test]
    fn fixups_se_aplican_o_se_marcan() {
        // caso ntfs-3g: cola del bloque 0 == USA[1] (array tras USA[0]).
        let mut r = rec_bytes(1, &[fn_attr("x", 5, false, 0)]);
        r[10..12].copy_from_slice(&0x1111u16.to_le_bytes()); // USA[1]
        r[510..512].copy_from_slice(&0x1111u16.to_le_bytes()); // cola sin fixup
        assert!(apply_fixups(&mut r));
        assert_eq!(u16::from_le_bytes([r[510], r[511]]), 0xBEEF);
        // caso desalineado: se marca pero no se toca nada.
        let mut r2 = rec_bytes(1, &[fn_attr("x", 5, false, 0)]);
        r2[10..12].copy_from_slice(&0x1111u16.to_le_bytes());
        r2[510..512].copy_from_slice(&0x2222u16.to_le_bytes());
        assert!(!apply_fixups(&mut r2));
        assert_eq!(u16::from_le_bytes([r2[510], r2[511]]), 0x2222);
    }

    // ---- caminante de $MFT e informes (fuente en memoria) ------------------

    struct Mem {
        data: Vec<u8>,
    }

    impl crate::source::SectorSource for Mem {
        fn sector_count(&self) -> u64 {
            self.data.len() as u64 / 512
        }
        fn read_sector(&mut self, lba: u64, buf: &mut [u8; 512]) -> io::Result<()> {
            let at = lba as usize * 512;
            if at + 512 > self.data.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "fuera de rango"));
            }
            buf.copy_from_slice(&self.data[at..at + 512]);
            Ok(())
        }
        fn write_sector(&mut self, _lba: u64, _buf: &[u8; 512]) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "solo lectura"))
        }
    }

    /// Mini-imagen NTFS: BPB del fixture, $MFT en el sector 16 con 6 registros
    /// (1024 B): $MFT, basura, hola.txt, datos/, borrado.txt (borrado) y raíz.
    fn mini_imagen() -> (Mem, NtfsBpb) {
        let mut img = vec![0u8; 4096 * 512];
        img[0..512].copy_from_slice(&ntfs_bpb_fixture());
        let layout = parse_ntfs_bpb(&img[0..512]).unwrap();
        let recs: [Vec<u8>; 6] = [
            rec_bytes(1, &[fn_attr("$MFT", 0, false, 0), data_attr(&[0x11, 0x08, 0x02, 0x00], 32_768), bitmap_attr()]),
            vec![7u8; 1024],
            rec_bytes(1, &[fn_attr("hola.txt", 5, false, 1234), data_attr(&[0x11, 0x04, 0x0A, 0x00], 2048)]),
            rec_bytes(1, &[fn_attr("datos", 5, true, 0), index_root_attr()]),
            rec_bytes(0, &[fn_attr("borrado.txt", 5, false, 99)]),
            rec_bytes(1, &[fn_attr(".", 5, true, 0), index_root_attr()]),
        ];
        for (i, r) in recs.iter().enumerate() {
            let at = (16 + i * 2) * 512;
            img[at..at + 1024].copy_from_slice(r);
        }
        (Mem { data: img }, layout)
    }

    fn mini_mapa() -> MapFile {
        // $MFT terminada hasta el registro herido; hola pendiente; datos agotado.
        let mut m = MapFile::new(4096);
        m.set(16, 4, MapStatus::Good, 0);
        m.set(20, 2, MapStatus::Failed, 0);
        m.set(22, 2, MapStatus::Scraped, 3);
        m
    }

    #[test]
    fn caminante_lista_sin_abortar_y_marca_estado() {
        let (mut mem, layout) = mini_imagen();
        let map = mini_mapa();
        let w = walk_mft(&mut mem, Some(&map), &layout, 100_000).expect("recorrido");
        // registros: 6 con nombre; la basura y los ceros cuentan como no parseables.
        assert_eq!(w.entries.len(), 5);
        assert!(w.records_bad >= 27);
        assert_eq!(w.mft_start, 16);
        let find = |n: &str| w.entries.iter().position(|e| e.name == n).unwrap();
        assert_eq!(w.entries[find("hola.txt")].map_status, "pendiente");
        assert_eq!(w.entries[find("datos")].map_status, "agotado");
        assert_eq!(w.entries[find("$MFT")].map_status, "terminado");
        assert!(w.entries[find("borrado.txt")].deleted);
        // la raíz se reconoce y el árbol queda montado pese al registro herido
        let root = root_index(&w).expect("raíz");
        assert_eq!(w.entries[root].number, 5);
        let ch = children_map(&w);
        let kids = ch.get(&5u64).unwrap();
        assert!(kids.iter().any(|&k| w.entries[k].name == "hola.txt"));
        let d = resolve_path(&w, &ch, root, "/datos").expect("ruta resolvible");
        assert_eq!(w.entries[d].name, "datos");
        assert_eq!(path_of(&w, find("hola.txt")), "/hola.txt");
        // sin mapa: estado «sin dato», recorrido idéntico
        let (mut mem2, _) = mini_imagen();
        let w2 = walk_mft(&mut mem2, None, &layout, 100_000).unwrap();
        assert_eq!(w2.entries.len(), 5);
        assert_eq!(w2.entries[find("hola.txt")].map_status, "sin dato");
    }

    #[test]
    fn pending_ranges_fusion_huecos() {
        let m = mini_mapa();
        let r = pending_ranges(&m);
        // hueco 0..16 (cerrado por el Good), corrida 20..fin de dominio.
        assert_eq!(r[0], (0, 16, "pendiente"));
        assert_eq!(r[1].0, 20);
        assert_eq!(r[1].1, 4096 - 20);
        assert_eq!(r[1].2, "mixto"); // hay pendientes y también agotados en la corrida
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn report_jsonl_golden() {
        let (mut mem, layout) = mini_imagen();
        let map = mini_mapa();
        let w = walk_mft(&mut mem, Some(&map), &layout, 100_000).unwrap();
        let mut out: Vec<u8> = Vec::new();
        write_report(&w, &layout, &map, &mut out).unwrap();
        let txt = String::from_utf8(out).unwrap();
        let lineas: Vec<&str> = txt.lines().collect();
        assert_eq!(lineas.len(), 3, "{txt}"); // dos tramos + resumen
        assert!(lineas[0].contains("\"tramo\":[0,16]"));
        assert!(lineas[0].contains("\"entradas\":[]"));
        assert!(lineas[1].contains("\"entradas_total\":6"), "línea tramo: {}", lineas[1]); // $MFT cuenta su registro y sus clusters; hola, registro y datos
        assert!(lineas[1].contains("\"ref\":0"));
        assert!(lineas[1].contains("\"nombre\":\"hola.txt\""));
        assert!(lineas[1].contains("\"path\":\"/hola.txt\""));
        assert!(lineas[1].contains("\"nombre\":\"borrado.txt\""));
        assert!(lineas[2].contains("\"tramos_con_metadatos\":1"));
        assert!(lineas[2].contains("\"registros_no_parseables\":27"));
    }

    #[test]
    fn parse_args_flags_consumen_valores() {
        let s = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<String>>();
        let a = parse_fs_args(&s(&[
            "img.raw", "--map", "m.map", "--partition", "2", "--path", "/datos", "--max-entries", "500",
        ]))
        .expect("flags con valores deben consumir su valor");
        assert_eq!(a.image.as_deref(), Some("img.raw"));
        assert_eq!(a.map, Some(PathBuf::from("m.map")));
        assert_eq!(a.partition, Some(2));
        assert_eq!(a.path, "/datos");
        assert_eq!(a.max_entries, 500);
        assert!(parse_fs_args(&s(&["x", "--map"])).is_err());
        assert!(parse_fs_args(&s(&["x", "--nope"])).is_err());
        assert!(parse_fs_args(&s(&["x", "y"])).is_err());
    }

    #[test]
    fn spc_derivable_del_propio_record() {        // $MFT física en el sector 16, su run dice lcn 2 → spc 8.
        assert_eq!(spc_from_self(16, 2), Some(8));
        // en un volumen real del mismo patrón: 6.291.456 / 786.432 = 8.
        assert_eq!(spc_from_self(6_291_456, 786_432), Some(8));
    }
}
