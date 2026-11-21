//! Capa de acceso bajo ATA vía SCSI `SG_IO` (U5b).
//!
//! Comandos directos sin arrastrar datos por el bus — el READ VERIFY del
//! firmware — y FLUSH CACHE tras las reescrituras, con timeout por comando.
//! El diseño de los CDB SAT16 (ATA PASS-THROUGH(16), opcode 0x85) está
//! espejado del formato que smartmontools envía a las pontas SCSI↔ATA desde
//! hace años (lib/scsiata.cpp), y los números de ioctl contra las cabeceras
//! del kernel (`scsi/sg.h`); ambos los fijan los tests de este módulo.
//!
//! Los constructores de CDB son puros y compilan en todos los SO; la
//! ejecución `SG_IO` existe solo en Linux. Donde no hay paso directo (nodos
//! de partición, macOS, Windows, medios virtuales) la verificación degrada a
//! lecturas normales en la capa `SectorSource` — nunca se malinterpreta un
//! «no soportado» como sector dañado.

// Solo la ejecución SG_IO (Linux) los consume; en otros SO quedan sin uso.
#[allow(unused_imports)]
use std::io;
#[allow(unused_imports)]
use std::time::Duration;

/// Opcodes SAT: ATA PASS-THROUGH(16) y (12).
#[allow(dead_code)] // los constructores que los emplean corren en todos los
// SO (sus tests también); solo la ejecución SG_IO es exclusiva de Linux
pub const SAT16_OPCODE: u8 = 0x85;
#[allow(dead_code)] // reservado por si una ponta antigua solo entiende SAT12
pub const SAT12_OPCODE: u8 = 0xA1;

/// Comandos ATA empleados: READ VERIFY SECTORS(48) y FLUSH CACHE.
#[allow(dead_code)] // ver nota en SAT16_OPCODE
pub const ATA_READ_VERIFY48: u8 = 0x40;
#[allow(dead_code)] // ver nota en SAT16_OPCODE
pub const ATA_FLUSH_CACHE: u8 = 0xE7;

/// CDB SAT16 para READ VERIFY SECTORS(48): el firmware comprueba
/// `count` sectores a partir de `lba` sin transferir su contenido.
/// protocol = 3 (no-data) con EXTEND; el LBA de 48 bits usa el reparto ATA-6
/// de registros «previous» y el byte device con 0x40 (modo LBA) + LBA(27:24).
#[allow(dead_code)] // lo consume solo el backend Linux; sus tests, todos
pub fn cdb_read_verify48(lba: u64, count: u16) -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = SAT16_OPCODE;
    c[1] = (3 << 1) | 1; // protocol << 1 | EXTEND
    c[2] = (1 << 3) | (1 << 2); // t_dir | byte_block; t_length = 0 (sin datos)
    c[5] = (count >> 8) as u8; // sector count (15:8)
    c[6] = count as u8; // sector count (7:0)
    c[8] = (lba & 0xff) as u8; // LBA (7:0)
    c[10] = ((lba >> 8) & 0xff) as u8; // LBA (15:8)
    c[12] = ((lba >> 16) & 0xff) as u8; // LBA (23:16)
    c[7] = ((lba >> 24) & 0xff) as u8; // previous LBA Low (31:24)
    c[9] = ((lba >> 32) & 0xff) as u8; // previous LBA Mid (39:32)
    c[11] = ((lba >> 40) & 0xff) as u8; // previous LBA High (47:40)
    c[13] = 0x40 | ((lba >> 24) & 0x0f) as u8; // device: modo LBA + LBA (27:24)
    c[14] = ATA_READ_VERIFY48;
    c
}

/// CDB SAT16 para FLUSH CACHE (28 bits, no-data): vacía la caché de
/// escritura del propio disco. Complementa O_SYNC (bandera de apertura) y
/// BLKFLSBUF (caché del kernel): los tres puntos del camino quedan vacíos.
#[allow(dead_code)] // lo consume solo el backend Linux; sus tests, todos
pub fn cdb_flush_cache() -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = SAT16_OPCODE;
    c[1] = 3 << 1; // protocol << 1, sin EXTEND
    c[2] = (1 << 3) | (1 << 2); // t_dir | byte_block (inocuo: sin datos)
    c[14] = ATA_FLUSH_CACHE;
    c
}

// ----------------------------- ejecución SG_IO (Linux) ---------------------

/// Ioctls SCSI generic (scsi/sg.h): SG_GET_VERSION_NUM = _IO(0x22,130),
/// SG_IO = _IO(0x22,133).
#[cfg(target_os = "linux")]
pub const SG_GET_VERSION_NUM: u64 = 0x2282;
#[cfg(target_os = "linux")]
pub const SG_IO: u64 = 0x2285;

#[cfg(target_os = "linux")]
extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

/// Espejo de `sg_io_hdr_t` (scsi/sg.h) con orden y tipos exactos; el test
/// `sg_io_hdr_respeta_la_cabecera_c` fija offsets y tamaño contra la
/// cabecera C en tiempo de compilación.
#[cfg(target_os = "linux")]
#[repr(C)]
pub struct SgIoHdr {
    interface_id: i32, // 'S'
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut std::os::raw::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32, // milisegundos
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut std::os::raw::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

/// ¿El nodo acepta ioctls SCSI generic? El bloque del kernel emula
/// SG_GET_VERSION_NUM para todo dispositivo de bloque SCSI/SATA/USB (incluidos
/// nodos de partición montados: es una consulta sin efecto sobre el medio).
#[cfg(target_os = "linux")]
pub fn sg_supported(fd: i32) -> bool {
    let mut version: i32 = 0;
    unsafe { ioctl(fd, SG_GET_VERSION_NUM, &mut version) == 0 }
}

/// Envía un CDB ATA por SG_IO sin transferencia de datos y decodifica el
/// resultado. Errores:
/// - `ErrorKind::Unsupported`: la ponta o el nodo no entienden el comando
///   (opcode inválido / petición ilegal) — el llamador debe DEGRADAR a lecturas
///   normales, jamás registrar sectores como dañados por esto.
/// - `ErrorKind::TimedOut`: el comando no terminó dentro del plazo.
/// - otros (`Other`…): fallo real del medio (error de medio, fallo
///   de hardware): para el motor es un intento fallido legítimo.
#[cfg(target_os = "linux")]
pub fn sg_ata_cmd(fd: i32, cdb: &[u8; 16], timeout: Duration) -> io::Result<()> {
    let mut sense = [0u8; 32];
    let mut hdr = SgIoHdr {
        interface_id: b'S' as i32,
        dxfer_direction: -1, // SG_DXFER_NONE
        cmd_len: cdb.len() as u8,
        mx_sb_len: 32,
        iovec_count: 0,
        dxfer_len: 0,
        dxferp: std::ptr::null_mut(),
        cmdp: cdb.as_ptr() as *mut u8,
        sbp: sense.as_mut_ptr(),
        timeout: timeout.as_millis().min(u32::MAX as u128) as u32,
        flags: 0,
        pack_id: 0,
        usr_ptr: std::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };
    // EINTR (Ctrl+C, señales): reintento acotado; el engine comprueba el
    // flag de interrupción en su propio bucle.
    let mut tries = 0;
    let rc = loop {
        let r = unsafe { ioctl(fd, SG_IO, &mut hdr) };
        tries += 1;
        if r == 0 || io::Error::last_os_error().raw_os_error() != Some(4) || tries >= 3 {
            break r;
        }
    };
    if rc != 0 {
        let e = io::Error::last_os_error();
        // ENOTTY/EINVAL: el nodo no expone SG_IO (p. ej. virtuales).
        if matches!(e.raw_os_error(), Some(25) | Some(22)) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("el nodo no acepta comandos SCSI directos: {e}"),
            ));
        }
        return Err(e);
    }
    if hdr.status == 0 && hdr.host_status == 0 && hdr.driver_status == 0 {
        return Ok(());
    }
    // Decodificación básica del sense: clave y, si existe, ATA Return
    // Descriptor (0x09) con los registros error/status del ATA.
    let n = (hdr.sb_len_wr as usize).min(sense.len());
    let (sense_key, asc) = if n >= 8 && matches!(sense[0] & 0x7f, 0x70 | 0x71) {
        (sense[2] & 0x0f, sense.get(12).copied())
    } else if n >= 3 && matches!(sense[0] & 0x7f, 0x72 | 0x73) {
        (sense[1] & 0x0f, sense.get(2).copied())
    } else {
        (0xff, None)
    };
    let mut ata_error = None;
    let mut ata_status = None;
    if n >= 8 {
        let mut i = 8usize; // descriptores tras la cabecera fija de 8 B
        while i + 14 <= n {
            let len = sense[i + 1] as usize + 2;
            if len < 2 {
                break;
            }
            if sense[i] == 0x09 {
                ata_error = Some(sense[i + 3]);
                ata_status = Some(sense[i + 13]);
                break;
            }
            i += len;
        }
    }
    // Opcode inválido / campo inválido / medio sin soporte del comando →
    // degrada, no malinterpreta.
    if sense_key == 0x05 || matches!(asc, Some(0x06)) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "la ponta no soporta este comando ATA directo",
        ));
    }
    // Timeout: el SG del bloque señala host_status = DID_ERROR (0x05) con
    // SCSI status 0 cuando la orden caducó (sin sense); también puede venir
    // ETIMEDOUT por errno. Es el único caso «no sé si el medio es malo o va
    // lento»: se comunica como TimedOut y el usuario ajusta --timeout.
    let host_did = hdr.host_status & 0xff; // hostbyte(cmd->result)
    if hdr.status == 0 && host_did != 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "el comando ATA no completó (host=0x{host_did:02x} driver=0x{:02x}); si este medio tarda legítimamente, sube --timeout (ahora {} s)",
                hdr.driver_status,
                timeout.as_secs()
            ),
        ));
    }
    Err(io::Error::new(
        io::ErrorKind::Other,
        format!(
            "comando ATA falló: scsi_status={} host={} driver={} sense_key={:02x}{}{}",
            hdr.status,
            hdr.host_status,
            hdr.driver_status,
            sense_key,
            ata_error.map_or(String::new(), |e| format!(" ata_error=0x{e:02x}")),
            ata_status.map_or(String::new(), |s| format!(" ata_status=0x{s:02x}")),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdb_read_verify48_sigue_el_formato_sat16() {
        // LBA 0x1234_5678_9abc (48 bits), 300 sectores.
        let c = cdb_read_verify48(0x1234_5678_9abc, 300);
        assert_eq!(c[0], 0x85);
        assert_eq!(c[1], (3 << 1) | 1, "protocol no-data + EXTEND");
        assert_eq!(c[2], (1 << 3) | (1 << 2), "t_dir | byte_block, t_length 0");
        assert_eq!([c[5], c[6]], [0x01, 0x2c], "recuento 300 (16 bits)");
        assert_eq!(c[8], 0xbc, "LBA(7:0)");
        assert_eq!(c[10], 0x9a, "LBA(15:8)");
        assert_eq!(c[12], 0x78, "LBA(23:16)");
        assert_eq!(c[7], 0x56, "previous LBA Low (31:24)");
        assert_eq!(c[9], 0x34, "previous LBA Mid (39:32)");
        assert_eq!(c[11], 0x12, "previous LBA High (47:40)");
        assert_eq!(c[13], 0x40 | 0x6, "device: modo LBA + LBA(27:24)");
        assert_eq!(c[14], 0x40, "READ VERIFY SECTORS(48)");
        assert_eq!(c[15], 0, "control SCSI");
        // Caso borde: LBA 0, recuento 1.
        let z = cdb_read_verify48(0, 1);
        assert_eq!(z[5..7], [0x00, 0x01][..], "recuento 1");
        assert_eq!(z[13], 0x40);
        for i in [7usize, 9, 11] {
            assert_eq!(z[i], 0);
        }
    }

    #[test]
    fn cdb_flush_cache_es_no_data_sin_extend() {
        let c = cdb_flush_cache();
        assert_eq!(c[0], 0x85);
        assert_eq!(c[1], 6, "protocol no-data, sin EXTEND");
        assert_eq!(c[14], 0xE7, "FLUSH CACHE");
    }

    /// Regresión: los números SG provienen de _IO(0x22, nr) con
    /// nr 130/133 según scsi/sg.h (verificados contra el kernel en vivo).
    #[cfg(target_os = "linux")]
    #[test]
    fn sg_ioctls_derivados() {
        assert_eq!((0x22u64 << 8) | 130, SG_GET_VERSION_NUM);
        assert_eq!((0x22u64 << 8) | 133, SG_IO);
    }

    /// Regresión: el espejo Rust de `sg_io_hdr_t` reproduce offsets y tamaño
    /// de la cabecera C (x86_64). Un campo mal ordenado enviaría basura al
    /// kernel con la misma llamada.
    #[cfg(target_os = "linux")]
    #[test]
    fn sg_io_hdr_respeta_la_cabecera_c() {
        use std::mem::{offset_of, size_of};
        assert_eq!(offset_of!(SgIoHdr, interface_id), 0);
        assert_eq!(offset_of!(SgIoHdr, dxfer_direction), 4);
        assert_eq!(offset_of!(SgIoHdr, cmd_len), 8);
        assert_eq!(offset_of!(SgIoHdr, mx_sb_len), 9);
        assert_eq!(offset_of!(SgIoHdr, iovec_count), 10);
        assert_eq!(offset_of!(SgIoHdr, dxfer_len), 12);
        assert_eq!(offset_of!(SgIoHdr, dxferp), 16);
        assert_eq!(offset_of!(SgIoHdr, cmdp), 24);
        assert_eq!(offset_of!(SgIoHdr, sbp), 32);
        assert_eq!(offset_of!(SgIoHdr, timeout), 40);
        assert_eq!(offset_of!(SgIoHdr, flags), 44);
        assert_eq!(offset_of!(SgIoHdr, pack_id), 48);
        assert_eq!(offset_of!(SgIoHdr, usr_ptr), 56);
        assert_eq!(offset_of!(SgIoHdr, status), 64);
        assert_eq!(offset_of!(SgIoHdr, sb_len_wr), 67);
        assert_eq!(offset_of!(SgIoHdr, host_status), 68);
        assert_eq!(offset_of!(SgIoHdr, driver_status), 70);
        assert_eq!(offset_of!(SgIoHdr, resid), 72);
        assert_eq!(offset_of!(SgIoHdr, duration), 76);
        assert_eq!(offset_of!(SgIoHdr, info), 80);
        assert_eq!(size_of::<SgIoHdr>(), 88);
    }
}
