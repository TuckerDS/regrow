# Change Log

Todos los cambios notables de ReGrow se documentan en este archivo.
El formato sigue [Keep a Changelog](https://keepachangelog.com/es/1.1.0/)
y el versionado sigue [Semantic Versioning](https://semver.org/lang/es/).

## [Unreleased]

### Added

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
- Línea de estado periódica también cuando no hay terminal.

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
