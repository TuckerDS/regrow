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
images a failing disk to a fault-tolerant file with resumable progress, and
stress-tests the write path of every sector it touches. Written in Rust using
only the standard library: one binary, zero dependencies, for macOS, Linux and
Windows.

---

## La idea

ReGrow quiere ser una sola herramienta libre que cubra el ciclo entero de
recuperación de un disco con sectores dañados:

1. **Diagnosticar** — leer los sectores crudos y separar los sanos de los lentos
   y los ilegibles, con medida real de latencia (bandas T1–T4) y battery
   multi-patrón de la ruta de escritura.
2. **Recuperar el medio** — provocar la reasignación (`remap`) de los sectores
   problemáticos para que el disco los abandone, respaldando antes su contenido.
3. **Extraer los datos** — volcar el disco a una imagen tolerante a fallos,
   reintentando las zonas malas y reanudando donde se quedó, con mapa de
   progreso compatible con los mapfiles de texto de GNU ddrescue.
4. **Recuperar los datos** — firmas de sistemas de archivos y particiones, y
   posterior *file carving* (en camino).

Está **inspirado en las utilidades clásicas de regeneración de sectores (HDD
Regenerator, DRevitalize, Victoria, badblocks)** y en las herramientas libres de
rescate (ddrescue, TestDisk/PhotoRec), con el objetivo de **unificar esas
escuelas en un solo programa** en lugar de repartir el trabajo entre cuatro
utilidades distintas.

## Principios

- **No empeorar el disco.** Todo modo destructivo es explícito: por defecto los
  sectores ilegibles **no** se reescriben, y cada sector problemático puede
  respaldarse antes de tocarlo. Lo ya recuperado es intocable: un rescate solo
  puede mejorar, nunca repetir ni borrar.
- **Sin disquete de arranque ni bootloader.** Trabaja desde macOS, Linux o
  Windows sobre el dispositivo crudo (`/dev/rdiskN`, `/dev/sdX`,
  `\\.\PhysicalDriveN`) o sobre una imagen de archivo.
- **Un binario, cero dependencias.** Escrito en Rust usando solo la biblioteca
  estándar (sin *crates*): ni OpenSSL, ni libclang, ni Python. Se compila para
  las tres plataformas desde un único código.
- **Progreso que no se pierde.** Cada pasada deja un registro y un punto de
  reanudación (checkpoint y mapfile reescritos al menos cada 60 s, aun con el
  medio extremadamente lento); Ctrl+C y `kill` abortan por sector, sin perder
  lo avanzado.

## Estado actual

Versión **0.2.0** — en uso sobre operaciones de recuperación reales.

| Ya disponible | En camino |
|---|---|
| `scan` / `repair` / `refresh` / `regenerate` / `prescan` / `speedtest` / `smart` / `selftest` | panel TUI interactivo mejorado |
| `rescue`: disco→imagen por fases (bloques directos, división binaria, rascado sector a sector, pasada inversa) con sidecar de respaldo previo | *file carving* |
| Mapfile binario propio (atómico, con CRC32) + importación/exportación de mapfiles de texto compatibles con ddrescue | firmas de particiones y sectores de arranque |
| Pasada quirúrgica `--import-map` en `scan/repair/refresh/regenerate/testwrite`: solo los tramos pendientes, sin releer ni reescribir lo bueno | límite de tasa y requeue estilo ddrescue |
| `testwrite`: battery multi-patrón (00/AA/55/FF + restauración verificada) con flags `W` (escritura mala) y `M` (marginal) | |
| `surfmap`: mapa de superficie HTML (calor sector a sector, zoom, y dibujado desde un mapfile) | |
| Menú interactivo con panel en vivo; **`regrow` a secas abre el menú** | |
| Detección de dañados y **latencia clasificada por bandas T1–T4** (`--latency 5,20,50,200`) | |
| Reanudación automática (`DISKn.state`, `--resume`) y Ctrl+C/`kill` limpios por sector | |
| Registros `DISKn.LOG` + `disklist.cfg` con rotación `.001–.005` | |
| Soporte de discos 4Kn (bloques de 4096 B) y 512e/512n | |
| HDD vs SSD/NVMe: regenerar y battery sobre páginas sanas de un sólido consume ciclos de vida, bloqueado salvo `--ssd-wear-ok`; `testwrite` nunca corre sobre un dispositivo montado | |

## Uso rápido

```bash
# Arranque pelado: abre el menú interactivo (pregunta unidad, modo y rango)
regrow

# Escanear sin reparar, de principio a fin, escribiendo registros
sudo regrow scan --raw 2

# Reparar solo los dañados, respaldando antes de reescribir
sudo regrow repair --raw 2 --backup /tmp/respaldo

# Rescatar el disco a una imagen tolerante a fallos (reanudable; el mapa
# entiende los mapfiles de texto de ddrescue)
sudo regrow rescue imagen.raw --raw 2 --export-map imagen.map.log

# Pasada quirúrgica: solo los tramos que un mapa ajeno dejó pendientes
sudo regrow testwrite --raw 2 --import-map imagen.map.log --backup /tmp/respaldo

# Battery de escritura sobre un rango (protege con sidecar y restaura)
sudo regrow testwrite --raw 2 --start 0 --end 4096 --backup /tmp/respaldo

# Mapa de superficie HTML a partir de los registros de la pasada
regrow surfmap --id 2 --out mapa.html

# Ajustar los umbrales de las bandas de latencia en ms (o 'off')
sudo regrow scan --raw 2 --latency 5,20,50,200

# Reanudar la pasada interrumpida
sudo regrow scan --raw 2 --resume
```

El menú ofrece reanudar sola una pasada interrumpida cuando la unidad tiene un
checkpoint pendiente: no hace falta saber en qué sector se cortó. Para ayuda
completa: `regrow --help`.

## Compilar y binarios

```bash
cargo build --release                  # nativo, sin dependencias
scripts/dist.sh                        # trío publicable en build/dist/:
                                       #   regrow-macos, regrow-linux (estático),
                                       #   regrow-windows-x64.exe
```

`scripts/dist.sh` usa mingw-w64 (Windows) y un enlazador musl (Linux estático,
para que el binario no quede atado a la glibc de la máquina de build); quien no
los tenga simplemente no genera ese binario. Requiere Rust 2021. No hay *crates*
en `Cargo.toml`: el acceso de bajo nivel se hace con llamadas directas a la libc
y a la API de Windows.

## Registros

ReGrow escribe su propia cadena de registros interoperable con el formato
histórico de este tipo de herramientas: `diskstats/DISK<n>.LOG` con líneas
`sector=FLAG<repeticiones>` (S inicio, L cierre, B dañado, R recuperado,
T/U/V/X para las bandas de latencia T1–T4, W escritura mala y M marginal) y
`disklist.cfg` con las unidades vistas. La rotación conserva las últimas cinco
pasadas como `.001` … `.005`.

## Advertencia

Manipular sectores crudos puede **destruir datos**. Prueba primero con un disco
del que no necesites nada, y haz una copia de seguridad antes de usar `repair`,
`regenerate`, `testwrite` o `--allow-data-loss`. Ningún programa recupera un
disco cuyo cabezal o electrónica ha fallado físicamente.

## Licencia

GPL-3.0-or-later, © 2026 TuckerDS. ReGrow es software libre: puedes usarlo,
estudiarlo, modificarlo y redistribuirlo bajo los mismos términos. No tiene
garantía de idoneidad.
