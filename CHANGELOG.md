# Change Log

Todos los cambios notables de ReGrow se documentan en este archivo.
El formato sigue [Keep a Changelog](https://keepachangelog.com/es/1.1.0/)
y el versionado sigue [Semantic Versioning](https://semver.org/lang/es/).

## [Unreleased]

## [0.2.0] - 2026-11-21

### Added

- `testwrite`: battery multi-patrón de superficie. Por cada sector objetivo
  guarda su contenido (sidecar `--backup`), escribe 0x00, 0xAA, 0x55 y 0xFF
  releyendo y comparando cada patrón, y al final restaura el contenido
  original verificándolo. Discrimina ilegibles (`B`), ruta de escritura
  defectuosa (`W`) y sectores MARGINALES (`M`): escriben y restauran bien
  pero fallan con algún patrón — sanos hoy, sospechosos mañana. Funciona
  sobre rangos completos o, con `--import-map`, solo sobre los tramos
  pendientes de un mapa (quirúrgico). Puertas: nunca sobre un dispositivo
  montado; exige `--backup` o `--allow-data-loss`; en SSD rige el mismo
  aviso de desgaste que `regenerate`. Los flags `W`/`M` aparecen en
  `DISKn.LOG`, en el mapa de calor `surfmap` y en el panel vivo.

- `rescue`: rescate disco→imagen por fases (bloques directos con saltos,
  división binaria, rascado sector a sector y pasada inversa). Destino
  disperso; el mapa binario propio (escritura atómica + CRC32) permite
  reanudar con `--resume` sin releer un solo sector bueno.
- Importación/exportación de mapfiles de texto compatibles con GNU ddrescue
  (`--import-map`, `--export-map`): continuar un rescate hecho por otra
  herramienta sin tocar nunca lo ya recuperado.
- `selftest`: lanzar autopruebas SMART (corta, extendida, conveyance) y
  leer su registro.
- `surfmap`: mapa de calor HTML sector a sector a partir de los registros
  de escaneo.
- Ctrl+C y `kill` abortan la pasada de forma limpia también sin terminal
  (SIGINT/SIGTERM): checkpoint y mapfile quedan guardados.
- `--retry-wait S`: segundos de descanso entre reintentos y tras bloque
  fallido en `rescue` (defecto 2; 0 desactiva las pausas).
- `--max-temp C` también en `rescue`: pausa al superar el umbral, reanuda
  al enfriar 2 °C, aborta con el mapa guardado si no enfría en 10 min.
- El progreso (checkpoint del escaneo y mapfile del rescate) se reescribe
  cada ≤60 s aunque el medio vaya extremadamente lento.
- `rescue` respalda cada sector ANTES de sobrescribirlo en la imagen: sidecar
  disperso `<imagen>.respaldo` al mismo LBA; si el respaldo falla, el rescate
  se detiene sin tocar más la imagen. `--backup-file R` / `--no-backup`.
- `surfmap` puede dibujar el estado completo desde un mapfile de texto
  (`surfmap --import-map mapa.log --sectors N`): mapa rescatado/pendiente/
  rascado con tooltips por celda y tabla de los tramos pendientes más grandes.
  Con ventana de zoom (`--start A --end B`) dibuja un clúster a resolución de
  megabytes y lista todos sus tramos con los sectores exactos.
- `rescue --fill-from <donante>`: relleno aditivo de los huecos pendientes de
  una imagen desde una segunda copia del origen (archivo). Con `--donor-map`
  solo se copia lo que el mapa del donante certifica como bueno; sin mapa del
  donante se filtran los sectores todo-cero (un hueco y un cero son
  indistinguibles en un archivo), con `--allow-zeros` como excepción explícita.
  Mismas protecciones que el rescate: respaldo sidecar previo, mapfile
  atómico, y los tramos ya buenos de la imagen jamás se reescriben.
- Línea de estado periódica también cuando no hay terminal.
- Pasada **quirúrgica** en `scan`/`repair`/`refresh`/`regenerate` con
  `--import-map`: la pasada recorre SOLO los tramos que un mapfile de texto
  ajeno deja pendientes (lo ya bueno no se lee ni se escribe) y anota el
  resultado en un mapfile propio (`--map`; defecto `<mapfile-ajeno>.regrow.map`):
  lo reescrito y verificado pasa a bueno, y lo que sigue malo queda pendiente
  con un reintento más contabilizado. Los tramos se guardan en `DISKn.ranges`,
  de modo que `--resume` los retoma sin volver a dar el mapfile; una reanudación
  de una pasada quirúrgica no cae nunca al barrido continuo. El porcentaje y el
  ETA se miden sobre los sectores pendientes, no sobre el disco entero.

### Fixed

- Mapfiles de ddrescue: las posiciones se leían como sectores cuando el
  formato usa bytes, y se usaba la leyenda de estados obsoleta (anterior a
  ddrescue 1.23). Ahora las unidades y los estados (`+ - ? * /`) se tratan
  como en el formato real, verificado contra ddrescue 1.29; la exportación
  emite ese mismo formato, legible por ddrescue y ddrescuelog.
- La respuesta al Ctrl+C dentro de bloques lentos (clusters de sectores
  dañados) era solo entre bloques; ahora se comprueba por sector.

## [0.1.0] - 2026-11-19

### Added

- Primera versión pública: escaneo y reparación de sectores crudos desde el
  SO anfitrión (macOS, Linux, Windows) sin disquete de arranque.
- Lectura de dispositivos nativos (`/dev/rdiskN`, `/dev/sdX`,
  `\\.\PhysicalDriveN`), registro de escaneo en formato HDDSTAT y consulta
  SMART.
