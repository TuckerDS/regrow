# ReGrow

**Recuperación completa de discos y datos, multiplataforma, desde el propio
sistema operativo.**

*Autor: IA local driven by TuckerDS · Copyright © 2026 TuckerDS · Licencia
GPL-3.0-or-later.*

### In English (summary)

ReGrow is a free, multi-platform tool for recovering disks and the data on
them, running from the host operating system — no boot floppy, no bootloader.
It reads raw sectors, tells healthy sectors apart from slow and unreadable
ones, forces the drive to remap the bad ones (backing their content up first),
and keeps a resumable log of every pass. Written in Rust using only the standard
library: one binary, zero dependencies, for macOS, Linux and Windows.

---

## La idea

ReGrow quiere ser una sola herramienta libre que cubra el ciclo entero de
recuperación de un disco con sectores dañados:

1. **Diagnosticar** — leer los sectores crudos y separar los sanos de los lentos
   y los ilegibles, con medida real de latencia.
2. **Recuperar el medio** — provocar la reasignación (`remap`) de los sectores
   problemáticos para que el disco los abandone, respaldando antes su contenido.
3. **Extraer los datos** — volcar el disco a una imagen tolerante a fallos,
   reintentando las zonas malas y reanudando donde se quedó.
4. **Recuperar los datos** — firmas de sistemas de archivos y particiones, y
   posterior *file carving*.

Está **inspirado en las utilidades clásicas de regeneración de sectores (HDD
Regenerator, DRevitalize, etc.)** y en las herramientas libres de rescate
(ddrescue, badblocks, TestDisk/PhotoRec, etc.), con el objetivo de **unificar
esas escuelas en un solo programa** en lugar de repartir el trabajo entre cuatro
utilidades distintas.

## Principios

- **No empeorar el disco.** Todo modo destructivo es explícito: por defecto los
  sectores ilegibles **no** se reescriben, y cada sector problemático puede
  respaldarse antes de tocarlo.
- **Sin disquete de arranque ni bootloader.** Trabaja desde macOS, Linux o
  Windows sobre el dispositivo crudo (`/dev/rdiskN`, `/dev/sdX`,
  `\\.\PhysicalDriveN`) o sobre una imagen de archivo.
- **Un binario, cero dependencias.** Escrito en Rust usando solo la biblioteca
  estándar (sin *crates*): ni OpenSSL, ni libclang, ni Python. Se compila para
  las tres plataformas desde un único código.
- **Progreso que no se pierde.** Cada pasada deja un registro y un punto de
  reanudación; si se interrumpe, se continúa donde estaba.

## Estado actual

Versión **0.1.0** — fase de validación en hardware real.

| Ya disponible | En camino |
|---|---|
| `scan` / `repair` / `refresh` / `regenerate` / `prescan` / `speedtest` / `smart` | extracción a imagen con reintentos y *mapfile* (0.2) |
| Menú interactivo con panel en vivo (barra, %, ETA) y mapa visual de unidades | self-tests SMART (0.2) |
| Detección de dañados y **latencia clasificada por bandas T1–T4** (`--latency 5,20,50,200`; flags `T/U/V/X` en el registro y `T·U·V·D` en el mapa) | patrones de bits de certificación (0.3) |
| Refresco dirigido (`refresh`): releer y reescribir los mismos bytes solo en sectores lentos | firmas de particiones y sectores de arranque (0.4) |
| Respaldo previo de cada sector problemático (`--backup`) | *file carving* (0.5) |
| Reanudación automática: checkpoint `DISKn.state`, `--resume` y oferta en el menú | |
| Registros `DISKn.LOG` + `disklist.cfg` con rotación `.001–.005` | |
| Soporte de discos 4Kn (bloques de 4096 B) y 512e/512n | |
| Comportamiento distinto para HDD y SSD/NVMe: regenerar páginas sanas de un sólido consume ciclos de vida, así que viene bloqueado salvo `--ssd-wear-ok` | |

## Uso rápido

```bash
# Menú interactivo (lista las unidades detectadas y pregunta rango y modo)
regrow menu

# Escanear sin reparar, de principio a fin, escribiendo registros
sudo regrow scan --raw 2

# Reparar solo los dañados, respaldando antes de reescribir
sudo regrow repair --raw 2 --backup /tmp/respaldo

# Refrescar solo los sectores lentos (los relee y reescribe con su propio
# contenido; los sanos no se tocan)
sudo regrow refresh --raw 2

# Ajustar los umbrales de las bandas de latencia en ms (o 'off')
sudo regrow scan --raw 2 --latency 5,20,50,200

# Reanudar la pasada interrumpida
sudo regrow scan --raw 2 --resume

# Regenerar un rango concreto (destructivo: reescribe también lo sano)
sudo regrow regenerate --raw 2 --start 0 --end 4096
```

El menú ofrece reanudar sola una pasada interrumpida cuando la unidad tiene un
checkpoint pendiente: no hace falta saber en qué sector se cortó.

## Construir

```bash
cargo build --release                  # nativo, sin dependencias
cargo build --release --target x86_64-unknown-linux-gnu
cargo build --release --target x86_64-pc-windows-msvc
```

Requiere Rust 2021. No hay *crates* en `Cargo.toml`: el acceso de bajo nivel se
hace con llamadas directas a la libc y a la API de Windows.

## Registros

ReGrow escribe su propia cadena de registros interoperable con el formato
histórico de este tipo de herramientas: `diskstats/DISK<n>.LOG` con líneas
`sector=FLAG<repeticiones>` (S inicio, L cierre, B dañado, R recuperado y
T/U/V/X para las bandas de latencia T1–T4) y `disklist.cfg` con las unidades
vistas. La rotación conserva las últimas cinco pasadas como `.001` … `.005`.

## Advertencia

Manipular sectores crudos puede **destruir datos**. Prueba primero con un disco
del que no necesites nada, y haz una copia de seguridad antes de usar `repair`,
`regenerate` o `--allow-data-loss`. Ningún programa recupera un disco cuyo
cabezal o electrónica ha fallado físicamente.

## Licencia

GPL-3.0-or-later, © 2026 TuckerDS. ReGrow es software libre: puedes usarlo,
estudiarlo, modificarlo y redistribuirlo bajo los mismos términos. No tiene
garantía de idoneidad.
