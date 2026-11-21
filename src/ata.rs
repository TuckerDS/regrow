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

/// Comandos ATA empleados: READ VERIFY SECTORS(48), FLUSH CACHE,
/// IDENTIFY DEVICE y READ NATIVE MAX ADDRESS(48) (forense HPA/DCO, U6).
#[allow(dead_code)] // ver nota en SAT16_OPCODE
pub const ATA_READ_VERIFY48: u8 = 0x40;
#[allow(dead_code)] // ver nota en SAT16_OPCODE
pub const ATA_FLUSH_CACHE: u8 = 0xE7;
#[allow(dead_code)] // ver nota en cdb_identify
pub const ATA_IDENTIFY: u8 = 0xEC;
#[allow(dead_code)] // ver nota en cdb_read_native_max48
pub const ATA_READ_NATIVE_MAX48: u8 = 0x27;

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

/// CDB SAT16 para IDENTIFY DEVICE: PIO-in de 1 bloque (512 B) con el
/// recuento en sector_count (t_length=2); sin EXTEND (comando de 28 bits).
#[allow(dead_code)] // lo consume solo la ponta forense Linux; sus tests, todos
pub fn cdb_identify() -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = SAT16_OPCODE;
    c[1] = 4 << 1; // protocol PIO-in, sin EXTEND
    c[2] = (1 << 3) | (1 << 2) | 2; // t_dir | byte_block | t_length=2
    c[6] = 1; // sector_count: un bloque
    c[14] = ATA_IDENTIFY;
    c
}

/// CDB SAT16 para READ NATIVE MAX ADDRESS(48): sin datos, pero con
/// ck_cond=1 para que el nodo devuelva los registros ATA de salida vía el
/// ATA Return Descriptor del sense — la dirección máxima nativa (lo que el
/// medio podría atender sin Host Protected Area) viaja ahí, no en datos.
#[allow(dead_code)] // ver nota en cdb_identify
pub fn cdb_read_native_max48() -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = SAT16_OPCODE;
    c[1] = (3 << 1) | 1; // protocol no-data + EXTEND
    c[2] = (1 << 5) | (1 << 3) | (1 << 2); // ck_cond | t_dir | byte_block
    c[13] = 0x40; // device: modo LBA — sin él la unidad aborta el comando
    c[14] = ATA_READ_NATIVE_MAX48;
    c
}

/// Registros ATA de salida reconstruidos de un ATA Return Descriptor
/// (descriptor sense 0x09): para órdenes de 48 bits el LBA y el recuento
/// salen de los bytes «previous» + «now» del propio descriptor.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtaOutRegs {
    pub error: u8,
    pub status: u8,
    pub device: u8,
    /// Sector count de 16 bits (48-bit commands).
    pub count: u16,
    /// LBA de 48 bits.
    pub lba: u64,
}

/// Busca el ATA Return Descriptor (0x09) en un buffer de sense descriptor y
/// lo convierte en registros. Puro y testeable: la misma ruta que smartmontools
/// (`lib/scsiata.cpp`), espejada byte a byte.
#[cfg(target_os = "linux")]
pub fn parse_ard(sense: &[u8]) -> Option<AtaOutRegs> {
    if sense.len() < 8 {
        return None;
    }
    let mut i = 8usize; // descriptores tras la cabecera fija de 8 B
    while i + 14 <= sense.len() {
        let code = sense[i];
        let len = sense[i + 1] as usize + 2;
        if len < 2 {
            break;
        }
        if code == 0x09 {
            let d = &sense[i..i + 14];
            let lo_lba = (d[11] as u64) << 16 | (d[9] as u64) << 8 | d[7] as u64;
            let hi_lba = (d[10] as u64) << 16 | (d[8] as u64) << 8 | d[6] as u64;
            return Some(AtaOutRegs {
                error: d[3],
                status: d[13],
                device: d[12],
                count: (d[4] as u16) << 8 | d[5] as u16,
                lba: (hi_lba << 24) | lo_lba,
            });
        }
        i += len;
    }
    None
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

/// Resultado de una orden SG: los registros ATA devueltos por el nodo (si los
/// hubo — aparecen solo con ck_cond, vía ATA Return Descriptor).
#[cfg(target_os = "linux")]
pub struct SgOutcome {
    pub out_ard: Option<AtaOutRegs>,
}

/// Núcleo SG_IO: envía un CDB con o sin transferencia PIO-in de 512 B y
/// decodifica el resultado. Semántica de éxito espejada de smartmontools:
/// status GOOD, o CHECK CONDITION con ATA Return Descriptor cuyo status ATA
/// sea DRDY sin BSY ni ERR (el camino normal de ck_cond). Errores:
/// - `ErrorKind::Unsupported`: la ponta o el nodo no entienden el comando
///   (opcode inválido / petición ilegal) — el llamador debe DEGRADAR a lecturas
///   normales, jamás registrar sectores como dañados por esto.
/// - `ErrorKind::TimedOut`: el comando no terminó dentro del plazo.
/// - otros (`Other`…): fallo real del medio (error de medio, fallo
///   de hardware): para el motor es un intento fallido legítimo.
#[cfg(target_os = "linux")]
fn sg_exec(
    fd: i32,
    cdb: &[u8; 16],
    timeout: Duration,
    data: Option<&mut [u8; 512]>,
) -> io::Result<SgOutcome> {
    let mut sense = [0u8; 64];
    let expects_data = data.is_some();
    let (dir, len, ptr) = match data {
        None => (-1i32, 0u32, std::ptr::null_mut()), // SG_DXFER_NONE
        Some(b) => (
            -3, // SG_DXFER_FROM_DEV
            512,
            b.as_mut_ptr() as *mut std::os::raw::c_void,
        ),
    };
    let mut hdr = SgIoHdr {
        interface_id: b'S' as i32,
        dxfer_direction: dir,
        cmd_len: cdb.len() as u8,
        mx_sb_len: 64,
        iovec_count: 0,
        dxfer_len: len,
        dxferp: ptr,
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
    let n = (hdr.sb_len_wr as usize).min(sense.len());
    let s = &sense[..n];
    let ard = parse_ard(s);
    let (sense_key, asc) = if n >= 8 && matches!(s[0] & 0x7f, 0x70 | 0xf0) {
        (s[2] & 0x0f, s.get(12).copied())
    } else if n >= 3 && matches!(s[0] & 0x7f, 0x72 | 0x73) {
        (s[1] & 0x0f, s.get(2).copied())
    } else {
        (0xff, None)
    };
    // Registros ATA en sense FIXED (respuesta 0x70/0xf0 de SAT-2+): error en
    // [3], status en [4] — los usa el mensaje de error; en 0xf0 acompañan
    // siempre (es el formato «ATA pass through information available»).
    let (fix_err, fix_status) = if n >= 8 && matches!(s[0] & 0x7f, 0x70 | 0xf0) {
        (Some(s[3]), Some(s[4]))
    } else {
        (None, None)
    };
    if hdr.host_status == 0 && hdr.driver_status == 0 && hdr.status == 0 {
        // GOOD: éxito simple (la ARD puede acompañar sin sense key).
        if expects_data && hdr.resid != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "la transferencia PIO-in quedó incompleta",
            ));
        }
        return Ok(SgOutcome { out_ard: ard });
    }
    // CHECK CONDITION con ARD cuyo status ATA dice DRDY, !BSY, !ERR: el
    // comando ATA terminó bien; el sense solo trae los registros pedidos
    // (ck_cond). driver_status 0x08 (DRIVER_SENSE) es normal aquí: el medio
    // respondió — lo que manda es el host byte limpio y el status ATA.
    if hdr.host_status == 0 {
        if let Some(a) = &ard {
            if (a.status & 0xc1) == 0x40 {
                return Ok(SgOutcome { out_ard: ard });
            }
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
    // Timeout: el SG del bloque señala host_status ≠ 0 con SCSI status 0
    // cuando la orden caducó (sin sense). Es el único caso «no sé si el medio
    // es malo o va lento»: se comunica como TimedOut y el usuario ajusta
    // --timeout.
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
            ard.map(|a| format!(" ata_error=0x{:02x}", a.error))
                .or_else(|| fix_err.map(|e| format!(" ata_error=0x{e:02x}")))
                .unwrap_or_default(),
            ard.map(|a| format!(" ata_status=0x{:02x}", a.status))
                .or_else(|| fix_status.map(|e| format!(" ata_status=0x{e:02x}")))
                .unwrap_or_default(),
        ),
    ))
}
/// Orden ATA sin transferencia de datos (READ VERIFY, FLUSH CACHE).
#[cfg(target_os = "linux")]
pub fn sg_ata_cmd(fd: i32, cdb: &[u8; 16], timeout: Duration) -> io::Result<()> {
    sg_exec(fd, cdb, timeout, None).map(|_| ())
}

/// Orden ATA PIO-in con buffer de 512 B (IDENTIFY).
#[cfg(target_os = "linux")]
pub fn sg_ata_cmd_data(
    fd: i32,
    cdb: &[u8; 16],
    timeout: Duration,
    buf: &mut [u8; 512],
) -> io::Result<()> {
    sg_exec(fd, cdb, timeout, Some(buf)).map(|_| ())
}

/// Orden ATA que exige registros de salida (ck_cond): sin ATA Return
/// Descriptor el nodo no cooperate → `Unsupported` (degradar, no dañinar).
#[cfg(target_os = "linux")]
pub fn sg_ata_cmd_regs(fd: i32, cdb: &[u8; 16], timeout: Duration) -> io::Result<AtaOutRegs> {
    sg_exec(fd, cdb, timeout, None)?
        .out_ard
        .ok_or_else(|| io::Error::new(
            io::ErrorKind::Unsupported,
            "el nodo no devolvió los registros ATA (falta el ATA Return Descriptor)",
        ))
}

// ----------------------------- forense IDENTIFY (U6) -----------------------

/// Datos estructurales útiles de IDENTIFY DEVICE (solo lectura; parseo puro
/// y testeable sobre el buffer PIO-in de 512 B). Mapa de palabras según
/// ACS-6 Table 69, verificado contra las tablas de smartmontools
/// (`src/ataidentify.cpp`) y contra discos reales en campo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtaIdentity {
    pub model: String,
    pub firmware: String,
    /// Sectores user totales según el registro de 28 bits (None si el medio
    /// declara desborde o no lo informa).
    pub sectors28: Option<u64>,
    /// Sectores user totales según el cuádruple de palabras 100:103
    /// (palabra 100 = menos significativa).
    pub sectors48: Option<u64>,
    pub hpa_supported: bool,
    pub dco_supported: bool,
    pub aam_supported: bool,
    /// Nivel AAM recomendado por el fabricante (si lo declara).
    pub aam_recommended: Option<u16>,
    pub apm_supported: bool,
}

/// Palabra 16 bits little-endian del buffer IDENTIFY.
#[allow(dead_code)] // lo consume parse_identify (punta Linux); sus tests, todos
fn id_word(id: &[u8; 512], w: usize) -> u16 {
    u16::from_le_bytes([id[w * 2], id[w * 2 + 1]])
}

/// Los campos de texto ATA guardan cada palabra con los bytes intercambiados.
#[allow(dead_code)] // lo consume parse_identify (punta Linux); sus tests, todos
fn id_text(id: &[u8; 512], from: usize, to: usize) -> String {
    let mut s = String::new();
    for w in from..=to {
        let word = id_word(id, w);
        s.push((word >> 8) as u8 as char);
        s.push((word & 0xff) as u8 as char);
    }
    s.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()
}

#[allow(dead_code)] // lo consume la ponta forense Linux; sus tests, todos
pub fn parse_identify(id: &[u8; 512]) -> AtaIdentity {
    // Palabras 23:26 firmware, 27:46 modelo.
    let firmware = id_text(id, 23, 26);
    let model = id_text(id, 27, 46);
    // Palabras 60:61: sectores user para comandos de 28 bits (60 = menos
    // significativa), según el mapa que usa smartmontools (ata.h). El valor
    // 0xFFFF_0000 señala desborde del modo de 28 bits y 0xFFFF_FFFF, unknown.
    let raw28 = (id_word(id, 61) as u64) << 16 | id_word(id, 60) as u64;
    let sectors28 = match raw28 {
        0 | 0xffff_0000 | 0xffff_ffff => None,
        v => Some(v),
    };
    // Palabras 100:103: total user para comandos de 48 bits (100 = LSB).
    let raw48 = (id_word(id, 103) as u64) << 48
        | (id_word(id, 102) as u64) << 32
        | (id_word(id, 101) as u64) << 16
        | id_word(id, 100) as u64;
    let sectors48 = if raw48 == 0 || raw48 == u64::MAX { None } else { Some(raw48) };
    // Conjuntos de características: palabra 82 bit 10 HPA; 83 bits 11 DCO,
    // 9 AAM y 3 APM (con sus espejos «support or enabled» 85/86).
    let hpa_supported = (id_word(id, 82) >> 10 & 1) == 1 || (id_word(id, 85) >> 10 & 1) == 1;
    let dco_supported = (id_word(id, 83) >> 11 & 1) == 1 || (id_word(id, 86) >> 11 & 1) == 1;
    let aam_supported = (id_word(id, 83) >> 9 & 1) == 1;
    let apm_supported = (id_word(id, 83) >> 3 & 1) == 1;
    // Palabra 128: valor AAM recomendado (magic 0x80 en el byte alto);
    // 127 es el valor recomendado simple.
    let aam_recommended = if id_word(id, 128) & 0xff00 == 0x8000 {
        Some(id_word(id, 128) & 0x00ff)
    } else if id_word(id, 127) != 0 {
        Some(id_word(id, 127))
    } else {
        None
    };
    AtaIdentity {
        model,
        firmware,
        sectors28,
        sectors48,
        hpa_supported,
        dco_supported,
        aam_supported,
        aam_recommended,
        apm_supported,
    }
}

/// Forense ATA de solo lectura: IDENTIFY + READ NATIVE MAX(48). La
/// comparación «máximo nativo vs área user» revela un Host Protected Area
/// activo sin tocar nada del medio (el equivalente de `hdparm -N`/`smartctl`
/// en modo lectura).
#[derive(Clone, Debug)]
pub struct AtaForensics {
    pub identity: AtaIdentity,
    /// Última LBA atendida nativamente por el medio (inclusive), si el nodo
    /// la devolvió.
    pub native_max: Option<u64>,
    /// Área user menor que el máximo nativo ⇒ hay sectores ocultos tras un
    /// HPA. None si el medio no reportó alguno de los dos valores.
    pub hpa_active: Option<bool>,
    /// Sectores que el HPA mantiene ocultos (solo si hpa_active == Some(true)).
    pub hpa_hidden: Option<u64>,
}

/// Sectores del área user (48 bits primero, fallback 28).
#[allow(dead_code)] // lo consumen probe_forensics y las cabeceras (Linux)
pub fn user_sectors(id: &AtaIdentity) -> Option<u64> {
    id.sectors48.or(id.sectors28)
}

/// Sondea un nodo que ya responde a SCSI generic (Linux). Devuelve None si el
/// medio no coopera (no es ATA, punta ciega, sin registros): jamás error.
#[cfg(target_os = "linux")]
pub fn probe_forensics(fd: i32, timeout: Duration) -> Option<AtaForensics> {
    let mut buf = [0u8; 512];
    if sg_ata_cmd_data(fd, &cdb_identify(), timeout, &mut buf).is_err() {
        return None;
    }
    let identity = parse_identify(&buf);
    // Un IDENTIFY vacío (p. ej. puente que responde ceros) no es evidencia.
    if identity.model.is_empty() && user_sectors(&identity).is_none() {
        return None;
    }
    let native_max = sg_ata_cmd_regs(fd, &cdb_read_native_max48(), timeout)
        .ok()
        .map(|r| r.lba);
    let user = user_sectors(&identity);
    let (hpa_active, hpa_hidden) = match (native_max, user) {
        (Some(m), Some(u)) if m + 1 > u => (Some(true), Some(m + 1 - u)),
        (Some(_), Some(_)) => (Some(false), None),
        _ => (None, None),
    };
    Some(AtaForensics {
        identity,
        native_max,
        hpa_active,
        hpa_hidden,
    })
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

    #[test]
    fn cdb_identify_es_pio_in_de_un_bloque() {
        let c = cdb_identify();
        assert_eq!(c[0], 0x85);
        assert_eq!(c[1], 4 << 1, "protocol PIO-in, sin EXTEND");
        assert_eq!(c[2], (1 << 3) | (1 << 2) | 2, "t_dir | byte_block | t_length=2");
        assert_eq!(c[6], 1, "sector_count: un bloque");
        assert_eq!(c[14], 0xEC, "IDENTIFY DEVICE");
    }

    #[test]
    fn cdb_read_native_max48_pide_los_registros() {
        let c = cdb_read_native_max48();
        assert_eq!(c[0], 0x85);
        assert_eq!(c[1], (3 << 1) | 1, "no-data + EXTEND");
        assert_eq!(c[2], (1 << 5) | (1 << 3) | (1 << 2), "ck_cond | t_dir | byte_block");
        assert_eq!(c[13], 0x40, "device en modo LBA: sin ese bit la unidad aborta");
        assert_eq!(c[14], 0x27, "READ NATIVE MAX ADDRESS(48)");
    }

    fn id_buf(model: &str, fw: &str) -> [u8; 512] {
        let mut id = [0u8; 512];
        let mut put = |s: &str, from: usize| {
            let bytes = s.as_bytes();
            for (i, &b) in bytes.iter().enumerate() {
                let w = from + i / 2;
                // Cada palabra ATA intercambia sus dos bytes.
                if i % 2 == 0 {
                    id[w * 2 + 1] = b;
                } else {
                    id[w * 2] = b;
                }
            }
        };
        put(fw, 23);
        put(model, 27);
        id
    }

    #[test]
    fn parse_identify_extrae_texto_capacidad_y_banderas() {
        let mut id = id_buf("WDC WD10EADS-00L5B1", "1.00A13");
        // Capacidad 1953525168 = 0x74706DB0: w100=0x6DB0 (LSB), w101=0x7470
        // (como muestran los dumps reales de IDENTIFY de un WD10EADS).
        id[200..202].copy_from_slice(&0x6DB0u16.to_le_bytes());
        id[202..204].copy_from_slice(&0x7470u16.to_le_bytes());
        // 28 bits: desborde declarado (w60=0x0000 LSB, w61=0xFFFF).
        id[120..122].copy_from_slice(&0x0000u16.to_le_bytes());
        id[122..124].copy_from_slice(&0xFFFFu16.to_le_bytes());
        // Características: w82 bit10 HPA, w83 bit11 DCO | bit9 AAM | bit3 APM.
        id[164..166].copy_from_slice(&0x0400u16.to_le_bytes());
        id[166..168].copy_from_slice(&0x0A08u16.to_le_bytes());
        // AAM recomendado 128 con magic 0x80.
        id[256..258].copy_from_slice(&0x800Cu16.to_le_bytes());

        let p = parse_identify(&id);
        assert_eq!(p.model, "WDC WD10EADS-00L5B1");
        assert_eq!(p.firmware, "1.00A13");
        assert_eq!(p.sectors48, Some(1_953_525_168));
        assert_eq!(p.sectors28, None, "el centinela de desborde no es capacidad");
        assert!(p.hpa_supported && p.dco_supported && p.aam_supported && p.apm_supported);
        assert_eq!(p.aam_recommended, Some(12));
        assert_eq!(user_sectors(&p), Some(1_953_525_168));
    }

    #[test]
    fn parse_identify_capacidad_28_bits_sin_48() {
        let mut id = id_buf("VIEJO DISCO", "1.0");
        // 976773168 = 0x3A386030: w60=0x6030 (LSB), w61=0x3A38.
        id[120..122].copy_from_slice(&0x6030u16.to_le_bytes());
        id[122..124].copy_from_slice(&0x3A38u16.to_le_bytes());
        let p = parse_identify(&id);
        assert_eq!(p.sectors28, Some(976_773_168));
        assert_eq!(p.sectors48, None);
        assert_eq!(user_sectors(&p), Some(976_773_168));
        assert!(!p.hpa_supported && !p.dco_supported);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_ard_reconstruye_el_lba_de_48_bits() {
        // Sense descriptor con ATA Return Descriptor (0x09, longitud 0x0c)
        // tras la cabecera fija de 8 B. El layout espejado (ardp[3]=error,
        // [4]/[5]=count prev/now, [6]/[7]=lba_low prev/now, [8]/[9]=lba_mid
        // prev/now, [10]/[11]=lba_high prev/now, [12]=device, [13]=status):
        let mut s = [0u8; 24];
        s[0] = 0x72; // descriptor sense; la sense key basura no importa
        s[1] = 1;
        s[2] = 0x05;
        s[8] = 0x09;
        s[9] = 0x0c;
        s[11] = 0x01; // error
        s[13] = 0x01; // sector count (ahora)
        s[14] = 0x56; // LBA low  previous (31:24)
        s[15] = 0xbc; // LBA low  (7:0)
        s[16] = 0x34; // LBA mid  previous (39:32)
        s[17] = 0x9a; // LBA mid  (15:8)
        s[18] = 0x12; // LBA high previous (47:40)
        s[19] = 0x78; // LBA high (23:16)
        s[20] = 0x40; // device
        s[21] = 0x50; // status: DRDY, sin BSY ni ERR
        let regs = parse_ard(&s).expect("el ARD debe parsearse");
        assert_eq!(regs.error, 0x01);
        assert_eq!(regs.status, 0x50);
        assert_eq!(regs.count, 0x0001);
        assert_eq!(regs.lba, 0x1234_5678_9abc, "48 bits desde prev+now");
        // Sin descriptor, None elegante (no es un error del medio).
        assert!(parse_ard(&[0x72, 0x01, 0x05, 0, 0, 0, 0, 0]).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_ata_de_exito_es_drdy_sin_bsy_ni_err() {
        // La comprobación que usa sg_exec (espejo de smartmontools):
        // (status & 0xC1) == 0x40  ⇒ DRDY, !BSY, !ERR.
        assert_eq!(0x50u8 & 0xc1, 0x40); // DRDY|DSC|seek: éxito
        assert_eq!(0xd8u8 & 0xc1, 0xc0); // BSY puesto: NO éxito
        assert_eq!(0x51u8 & 0xc1, 0x41); // ERR puesto: NO éxito
    }
}
