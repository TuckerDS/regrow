// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 TuckerDS
//! Primitivas TUI de regrow: modo raw del teclado, ESC sin Enter y colores
//! ANSI — std únicamente, con `extern "C"` directos a la libc/kernel32
//! (mismo patrón que los ioctls de `source.rs`). Mismas primitivas ya
//! probadas en el port clónico (src/ raíz).
//!
//! - Unix: `tcgetattr` + `cfmakeraw` + `tcsetattr` (búfer termios opaco:
//!   nunca leemos sus campos, así el layout puede variar entre SO) y
//!   `poll(2)` para sondear ESC durante la pasada (panel vivo).
//! - Windows: `_isatty`/`_kbhit`/`_getch` (msvcrt) + `SetConsoleMode` para
//!   quitar la entrada por línea y habilitar el procesado VT (Windows 10+)
//!   que hace funcionar los colores ANSI en la consola.
//! - Sin terminal (tests por pipe, redirección): todo degrada a modo simple
//!   sin códigos de control y sin pausa por ESC.

use std::io::Write;

pub const ESC: u8 = 0x1b;

#[cfg(unix)]
mod ffi {
    use std::os::raw::{c_int, c_void};

    /// Búfer opaco para struct termios: 256 B cubre con holgura el término
    /// real de macOS (72 B) y Linux (60 B). NUNCA interpretamos sus campos;
    /// solo libc lo rellena (`tcgetattr`) y lo aplica (`tcsetattr`).
    #[repr(C, align(8))]
    #[derive(Clone, Copy)]
    pub struct RawTermios {
        pub w: [u64; 32],
    }

    #[repr(C)]
    pub struct PollFd {
        pub fd: c_int,
        pub events: i16,
        pub revents: i16,
    }

    /// struct winsize (sys/termios.h en macOS y Linux).
    #[repr(C)]
    pub struct WinSize {
        pub rows: u16,
        pub cols: u16,
        pub xpixels: u16,
        pub ypixels: u16,
    }

    extern "C" {
        pub fn isatty(fd: c_int) -> c_int;
        pub fn tcgetattr(fd: c_int, t: *mut RawTermios) -> c_int;
        pub fn tcsetattr(fd: c_int, action: c_int, t: *const RawTermios) -> c_int;
        pub fn cfmakeraw(t: *mut RawTermios);
        pub fn poll(fds: *mut PollFd, nfds: c_int, timeout: c_int) -> c_int;
        pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        pub fn ioctl(fd: c_int, request: u64, ...) -> c_int;
        pub fn signal(sig: c_int, disp: Option<extern "C" fn(c_int)>) -> Option<extern "C" fn(c_int)>;
        pub fn kill(pid: c_int, sig: c_int) -> c_int;
        pub fn getpid() -> c_int;
    }
}

// Codificación de ioctls DERIVADA de los headers de cada SO (los números
// ioctl se derivan, jamás se transcriben). Tests verifican los valores.

/// Linux `_IO(type, nr)`: sin bits de dirección ni tamaño.
pub const fn _io(type_: u8, nr: u8) -> u64 {
    (type_ as u64) << 8 | nr as u64
}

/// BSD/Darwin `_IOR(group, nr, size)`: IOC_OUT=1<<30, grupo<<8, tamaño<<16.
pub const fn _ior(group: u8, nr: u8, size: u64) -> u64 {
    (1u64 << 30) | (size << 16) | (group as u64) << 8 | nr as u64
}

/// Petición TIOCGWINSZ según el SO. Linux la define como `_IO('T', 0x13)`
/// (asm-generic/ioctls.h); BSD/Darwin como `_IOR('t', 104, struct winsize)`.
#[cfg(target_os = "linux")]
pub const TIOCGWINSZ: u64 = _io(b'T', 0x13);
#[cfg(all(unix, not(target_os = "linux")))]
pub const TIOCGWINSZ: u64 = _ior(b't', 104, 8); // sizeof(winsize) = 4×u16

/// Ancho real de la terminal en columnas; 0 si no se puede averiguar.
pub fn term_cols() -> usize {
    #[cfg(unix)]
    {
        let mut ws = ffi::WinSize {
            rows: 0,
            cols: 0,
            xpixels: 0,
            ypixels: 0,
        };
        let r = unsafe { ffi::ioctl(1, TIOCGWINSZ, &mut ws as *mut _ as *mut core::ffi::c_void) };
        if r == 0 && ws.cols > 0 {
            return ws.cols as usize;
        }
        // Fallback: $COLUMNS (algunos entornos sin ioctl útil).
        std::env::var("COLUMNS")
            .ok()
            .and_then(|c| c.trim().parse::<usize>().ok())
            .filter(|c| *c > 0)
            .unwrap_or(0)
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct Coord {
            x: i16,
            y: i16,
        }
        #[repr(C)]
        struct SmallRect {
            left: i16,
            top: i16,
            right: i16,
            bottom: i16,
        }
        #[repr(C)]
        struct Info {
            cursor: Coord,
            attributes: u16,
            window: SmallRect,
            maximum_window_size: Coord,
        }
        extern "C" {
            fn GetConsoleScreenBufferInfo(
                handle: *mut core::ffi::c_void,
                info: *mut Info,
            ) -> i32;
        }
        let h = unsafe { ffi::GetStdHandle(-11) }; // STD_OUTPUT_HANDLE
        let mut info = Info {
            cursor: Coord { x: 0, y: 0 },
            attributes: 0,
            window: SmallRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            maximum_window_size: Coord { x: 0, y: 0 },
        };
        if unsafe { GetConsoleScreenBufferInfo(h, &mut info) } != 0 {
            let w = info.window.right.saturating_sub(info.window.left) + 1;
            if w > 0 {
                return w as usize;
            }
        }
        0
    }
    #[cfg(not(any(unix, windows)))]
    {
        0
    }
}

#[cfg(windows)]
mod ffi {
    use std::os::raw::c_int;

    extern "C" {
        pub fn _isatty(fd: c_int) -> c_int;
        pub fn _kbhit() -> c_int;
        pub fn _getch() -> c_int;
        pub fn GetStdHandle(which: c_int) -> *mut core::ffi::c_void;
        pub fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: u32) -> i32;
        pub fn signal(sig: c_int, disp: Option<extern "C" fn(c_int)>) -> Option<extern "C" fn(c_int)>;
    }
}

// ------------------------------------------------------------- interrupción --
// Sin esta capa, Ctrl+C o `kill` en una pasada de campo (nohup, sin tty) MATAN
// el proceso por defecto: se pierde el registro parcial y el checkpoint se
// queda obsoleto (hasta un poll entero). El handler solo fija una bandera
// (async-signal-safe: un AtomicBool lock-free); el motor la comprueba en cada
// tick y aborta el pase LIMPIO — registro parcial + checkpoint exacto,
// reanudable con --resume. En el panel vivo raw, cfmakeraw apaga ISIG: allí
// la tecla de abortar sigue siendo ESC (Ctrl+C llega como byte 0x03 inofensivo).
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: std::os::raw::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Instala el handler de interrupción (SIGINT=2 y SIGTERM=15 en unix; SIGINT
/// en la consola de Windows). Seguro llamarlo varias veces.
pub fn install_signals() {
    #[cfg(any(unix, windows))]
    unsafe {
        #[cfg(unix)]
        const SIGINT: std::os::raw::c_int = 2;
        #[cfg(unix)]
        const SIGTERM: std::os::raw::c_int = 15;
        #[cfg(windows)]
        const SIGINT: std::os::raw::c_int = 2;
        ffi::signal(SIGINT, Some(on_signal));
        #[cfg(unix)]
        ffi::signal(SIGTERM, Some(on_signal));
    }
}

/// ¿Se pidió abortar (Ctrl+C / kill)? El motor lo traduce en pase abortado.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Limpia la bandera al empezar cada pasada (en el menú, un Ctrl+C en una
/// pasada no debe abortar las siguientes).
pub fn clear_interrupted() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

/// ¿stdin y stdout son terminal? (los tests con Cursor devuelven false y la
/// UI cae al modo simple sin códigos de control).
pub fn is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { ffi::isatty(0) == 1 && ffi::isatty(1) == 1 }
    }
    #[cfg(windows)]
    {
        unsafe { ffi::_isatty(0) == 1 && ffi::_isatty(1) == 1 }
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Pone stdin en modo raw (teclas sin Enter, echo off) y restaura al salir de
/// scope. None si no hay terminal (no se toca nada). En regrow el raw dura
/// solo la pasada (los menús siguen siendo de línea).
pub struct RawGuard {
    #[cfg(unix)]
    saved: ffi::RawTermios,
    #[cfg(windows)]
    saved_mode: u32,
    #[cfg(windows)]
    handle: *mut core::ffi::c_void,
    active: bool,
}

impl RawGuard {
    pub fn new() -> Option<RawGuard> {
        if !is_tty() {
            return None;
        }
        #[cfg(unix)]
        {
            // saved = termios ORIGINAL para restaurar en Drop; cfmakeraw se
            // aplica sobre una COPIA (si mutáramos `saved`, Drop volvería a
            // dejar el terminal en raw). TCSANOW=0: aplicar ya — TCSADRAIN
            // puede bloquear indefinidamente en un PTY cuyo lado master no
            // drena la salida (observado en smoke 2026-11-19).
            let mut saved = ffi::RawTermios { w: [0; 32] };
            if unsafe { ffi::tcgetattr(0, &mut saved) } != 0 {
                return None;
            }
            let mut raw = saved;
            unsafe { ffi::cfmakeraw(&mut raw) };
            if unsafe { ffi::tcsetattr(0, 0, &raw) } != 0 {
                return None;
            }
            Some(RawGuard { saved, active: true })
        }
        #[cfg(windows)]
        {
            const ENABLE_LINE_INPUT: u32 = 0x2;
            const ENABLE_ECHO_INPUT: u32 = 0x4;
            const ENABLE_PROCESSED_OUTPUT: u32 = 0x1;
            const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;
            let stdin_h = unsafe { ffi::GetStdHandle(-10) }; // STD_INPUT_HANDLE
            let stdout_h = unsafe { ffi::GetStdHandle(-11) }; // STD_OUTPUT_HANDLE
            let mut mode = 0u32;
            if unsafe { ffi::GetConsoleMode(stdin_h, &mut mode) } == 0 {
                return None;
            }
            let saved_mode = mode;
            mode &= !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT);
            if unsafe { ffi::SetConsoleMode(stdin_h, mode) } == 0 {
                return None;
            }
            // ANSI en consolas Windows 10+; si el SO no lo soporta seguimos
            // sin color (Paint::on ya exige tty, y los códigos se ignoran).
            let mut omode = 0u32;
            if unsafe { ffi::GetConsoleMode(stdout_h, &mut omode) } == 0 {
                unsafe { ffi::SetConsoleMode(stdout_h, omode) };
            }
            unsafe {
                ffi::SetConsoleMode(
                    stdout_h,
                    omode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                );
            }
            Some(RawGuard {
                handle: stdin_h,
                saved_mode,
                active: true,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.active {
            unsafe { ffi::tcsetattr(0, 0, &self.saved) };
        }
        #[cfg(windows)]
        if self.active {
            unsafe { ffi::SetConsoleMode(self.handle, self.saved_mode) };
        }
    }
}

/// Tecla disponible sin bloquear (timeout_ms = 0), hasta el timeout, o
/// bloqueante (timeout_ms < 0, equivalente al getch INT21 AH=07h).
pub fn poll_key(timeout_ms: i32) -> Option<u8> {
    #[cfg(unix)]
    {
        let mut pf = ffi::PollFd { fd: 0, events: 1, revents: 0 }; // POLLIN = 1
        let r = unsafe { ffi::poll(&mut pf, 1, timeout_ms) };
        if r > 0 && (pf.revents & 1) != 0 {
            let mut b = 0u8;
            if unsafe { ffi::read(0, &mut b as *mut _ as *mut core::ffi::c_void, 1) } == 1 {
                return Some(b);
            }
        }
        None
    }
    #[cfg(windows)]
    {
        if unsafe { ffi::_kbhit() } != 0 {
            return Some(unsafe { ffi::_getch() } as u8);
        }
        if timeout_ms < 0 {
            // Bloqueante: espera una tecla (equivalente a getch).
            loop {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if unsafe { ffi::_kbhit() } != 0 {
                    return Some(unsafe { ffi::_getch() } as u8);
                }
            }
        }
        if timeout_ms > 0 {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if unsafe { ffi::_kbhit() } != 0 {
                    return Some(unsafe { ffi::_getch() } as u8);
                }
            }
        }
        None
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = timeout_ms;
        None
    }
}

/// Colores ANSI; `on` exige tty (los tests por pipe no ven códigos).
/// `cols` es el ancho real de la terminal (0 = desconocido; los consumidores
/// usan un margen por defecto). Todo lo que pinta el panel debe caber en
/// `cols`: una línea que envuelve rompe la geometría del borrado (`ESC[nA`)
/// y produce la cascada observada en campo (2026-11-19).
pub struct Paint {
    pub on: bool,
    pub cols: usize,
}

pub fn paint() -> Paint {
    Paint {
        on: is_tty(),
        cols: term_cols(),
    }
}

impl Paint {
    /// Ancho útil para el contenido (con fallback conservador y margen de
    /// 1 columna para evitar el off-by-one del envoltura automática).
    pub fn width(&self) -> usize {
        let raw = if self.cols == 0 { 80 } else { self.cols };
        raw.saturating_sub(1).max(20)
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    pub fn red(&self, text: &str) -> String {
        self.wrap("31", text)
    }
    pub fn cyan(&self, text: &str) -> String {
        self.wrap("36", text)
    }
    pub fn yellow(&self, text: &str) -> String {
        self.wrap("33", text)
    }
    pub fn green(&self, text: &str) -> String {
        self.wrap("32", text)
    }

    /// Reescribe el bloque vivo de N filas: sube `up` filas, fija la columna
    /// (`\r`), borra cada fila (`ESC[2K`) y la reimprime terminada en \n.
    /// Cada fila se recorta al ancho de la terminal (`clip`): una sola fila
    /// envuelta desalinea el `ESC[nA` y cascadearía el panel (bug de campo
    /// 2026-11-19). Sin ANSI: reimprime a secas (degradado).
    pub fn rows(&self, up: usize, lines: &[String]) {
        if self.on {
            let max = self.width();
            let mut out = String::new();
            if up > 0 {
                out.push_str(&format!("\x1b[{up}A"));
            }
            for l in lines {
                out.push_str("\r\x1b[2K");
                out.push_str(&clip(l, max));
                // \r\n en vez de \n pelado: en modo raw (ONLCR apagado) el
                // \n no resetea la columna, y esa columna sucia la heredaba
                // la primera fila del resumen al terminar la pasada (bug de
                // campo 2026-11-19). El \r explícito vale en raw y cooked.
                out.push_str("\r\n");
            }
            let _ = write!(std::io::stdout(), "{out}");
        } else {
            for l in lines {
                let _ = writeln!(std::io::stdout(), "{l}");
            }
        }
        let _ = std::io::stdout().flush();
    }
}

/// Recorta al ancho visible `max`, saltando secuencias de escape (no cuentan
/// como columna) y re-añadiendo el reset SGR si el recorte lo eliminaba.
pub fn clip(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut visible = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap());
                while let Some(n) = chars.next() {
                    out.push(n);
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if visible >= max {
            break;
        }
        out.push(c);
        visible += 1;
    }
    if text.ends_with("\x1b[0m") && !out.ends_with("\x1b[0m") {
        out.push_str("\x1b[0m");
    }
    out
}

/// Envolvimiento simple por palabras para mensajes largos (el panel y las
/// pistas de reanudación no deben depender del envoltado automático de la
/// terminal, que rompe la geometría de las filas vivas).
pub fn wrap_text(text: &str, cols: usize) -> String {
    let max = cols.saturating_sub(1).max(20);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let w = word.chars().count();
        if !cur.is_empty() && cur.chars().count() + 1 + w > max {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    /// Señal REAL (SIGINT al propio proceso) → bandera de interrupción.
    /// Prueba el camino completo: `signal()` FFI + handler async-safe +
    /// lectura atómica. Solo unix (donde existe `kill`).
    #[test]
    #[cfg(unix)]
    fn sigint_real_activa_la_bandera() {
        use super::{clear_interrupted, install_signals, interrupted};
        install_signals();
        clear_interrupted();
        unsafe {
            super::ffi::kill(super::ffi::getpid(), 2); // SIGINT
        }
        // La entrega de la señal ocurre al volver de la syscall siguiente;
        // el spin_loop mantiene el hilo vivo hasta entonces (sin dormir).
        for _ in 0..5_000_000 {
            if interrupted() {
                break;
            }
            std::hint::spin_loop();
        }
        assert!(interrupted(), "SIGINT no activó la bandera");
        clear_interrupted();
        assert!(!interrupted());
    }
}
