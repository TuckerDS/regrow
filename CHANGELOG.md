# Change Log

Todos los cambios notables de ReGrow se documentan en este archivo.
El formato sigue [Keep a Changelog](https://keepachangelog.com/es/1.1.0/)
y el versionado sigue [Semantic Versioning](https://semver.org/lang/es/).

## [Unreleased]

### Added

- `rescue`: controles de ritmo al estilo ddrescue. `--rate-limit B` techo de
  bytes/s copiados a la imagen (sufijos K/M; `off`); al superarlo en una
  ventana de 1 s el rescate duerme hasta cerrarla — para convivir con una
  máquina en producción. `--min-read-rate N|auto`: en la fase directa, un
  bloque que se lee más despacio de N bytes/s (con `auto`, un décimo del
  ritmo medio alcanzado) NO se copia en ese momento: se avisa, queda
  pendiente y lo retrabajan las fases lentas (división/rascado), que sí
  aceptan lecturas lentas — la pasada rápida no acampa sobre la zona mala y
  ningún dato leído se descarta nunca. `--max-error-rate B`: si los intentos
  de lectura fallidos superan B bytes/s sostenidos, el rescate se detiene
  con el mapfile guardado. La línea de estado ahora muestra el ritmo
  (KiB/s) y los bloques lentos diferidos; el resumen los contabiliza.
- `rescue --requeue`: al reanudar, los tramos que el mapa daba por agotados
  (`/` en ddrescue) vuelven a pendientes con los reintentos reiniciados, de
  modo que el algoritmo completo les dé otra pasada entera. Encaja después
  de un `repair`/remap sobre el mismo disco: lo que antes no se leía puede
  responder ahora.
- `rescue --preview [N]`: el estado muestra el hex+ASCII (1..32 líneas,
  defecto 3) del último sector leído bien — comprobación visual de lo que
  está saliendo del cabezal, al estilo del `-P` de ddrescue.
  `rescue --log-rates RUTA`: una línea numérica por cada estado (segundos,
  sector, buenos, pendientes, bloques, KiB/s), apta para graficar el rescate.

### Security

- Dispositivos crudos en Linux: las escrituras por fin llevan `O_SYNC` de
  verdad (el valor que usábamos era `O_NONBLOCK`; un test deriva ahora los
  números oficiales para no repetirlo). Todo intento de escritura abre además
  con `O_EXCL` — el kernel rechaza con mensaje claro escribir sobre un disco
  montado o retenido en exclusivo — y `BLKROGET` avisa de medios con bloqueo
  físico de escritura. Tras cada sector reescrito y al cerrar el dispositivo
  se ejecuta `BLKFLSBUF`, que invalida la caché de bloques del kernel: ningún
  lector posterior (nuestra verificación, el SO al remontar, otra
  herramienta) verá contenido anterior a nuestras reescrituras.
- La puerta «nunca escribir sobre un disco montado» es ahora universal: la
  aplica la capa de dispositivo a todo intento de escritura (CLI y menú,
  repair/refresh/regenerate/testwrite), no solo a `testwrite`.
- `--verify` (prescan/speedtest, y el prescan dentro de scan/repair): los
  bloques se comprueban con la lectura de verificación del firmware, sin
  transferir su contenido por el bus. Requiere nodo de disco ENTERO y punta
  que lo soporte; donde no está disponible el programa degrada
  automáticamente a lecturas normales y lo dice en cabecera — un medio «no
  soportado» jamás se registra como sector dañado. `--timeout S` fija el
  plazo de cada comando directo (defecto 30 s); si un medio tarda
  legítimamente lo indica y sugiere subirlo.
- Al cerrar un dispositivo donde se escribió, además del vaciado del SO se
  envía FLUSH CACHE al propio disco (solo si la sesión escribió de verdad:
  una pasada de puro rescate no le manda ninguna orden).

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
