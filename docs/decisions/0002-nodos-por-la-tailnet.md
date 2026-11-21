# 0002 — Varios nodos por la tailnet: descubrimiento, confianza y reparto

**Fecha:** 2026-11-21
**Estado:** aceptada

## Contexto

Con las Fases 1 a 5 un nodo funciona de punta a punta: `fleetd` lleva las tareas de su máquina y la CLI, la TUI y la web hablan con él por la API del ADR 0001. La Fase 6 reparte las tareas entre el PC (WSL2) y el Mac de Juan, que están en la misma tailnet de Tailscale. Hay que decidir cómo se encuentran los nodos, cómo se fía uno de las peticiones que le llegan de otro (o del iPhone) sin abrir nada a internet ni usar claves de API, cómo se nombra un repo que vive en rutas distintas en cada máquina y qué pasa cuando el Mac está dormido.

Datos de partida, vistos en el PC con `tailscale status --json` (Tailscale 1.104): el PC (`excaliburwsl`, Linux), el Mac (`macbook-de-juan`, macOS), Windows y el iPhone, todos del mismo usuario y ninguno con etiquetas. `tailscale serve` pone HTTPS con certificado válido en `https://<máquina>.<tailnet>.ts.net`, reenvía a `http://127.0.0.1:<puerto>`, añade `Tailscale-User-Login` y `Tailscale-User-Name` para peticiones de dispositivos sin etiqueta y quita las que traiga el cliente (no se pueden falsificar desde fuera).

## Decisión

### Cada máquina, su demonio y su estado

*   Cada máquina corre su `fleetd`, solo en loopback (ADR 0001), con su SQLite y sus worktrees. No hay un nodo maestro ni estado compartido: una tarea vive entera en la máquina donde corre.
*   Cada `fleetd` se comparte con la tailnet con `tailscale serve --bg --https=443 http://127.0.0.1:7171`. Es la misma URL para el navegador (también el del iPhone) y para los otros nodos.
*   El nombre de un nodo es el que da su `GET /v1/health` (`node`, el nombre de la máquina). Una tarea lleva el nodo en el que corre.

### Descubrimiento sin claves

*   `fleetd` mira la tailnet con `tailscale status --json`, que habla con el socket local de `tailscaled`: sin API de Tailscale ni claves. Cada 30 s y al pedir la lista de nodos si la última tiene más de 5 s.
*   Candidatos: pares en línea, del mismo usuario que esta máquina, sin etiquetas y con sistema `linux` o `macOS`. A cada uno se le pide `https://<DNSName>/v1/health` con plazo corto (2 s); si responde con el JSON de fleet, es un nodo. Lo que no responde (Windows, una máquina sin `fleetd`) no se reintenta hasta la siguiente vuelta.
*   `--peer URL` (repetible) añade nodos a mano, para cuando la detección no sirva (otro sistema, otro puerto).
*   Sin Tailscale en la máquina (o parado), fleet sigue funcionando como un solo nodo: la lista de nodos es solo él mismo.

### Confianza

*   Por loopback (`Host` de loopback) se confía como hasta ahora: es Juan o un proceso suyo en la máquina. La defensa de `guard.rs` frente a otras webs del navegador se queda.
*   Por la tailnet (`Host` `*.ts.net` o de `--allow-host`) la petición tiene que traer `Tailscale-User-Login` y ser el login del dueño de esta máquina (el de `Self` en `tailscale status --json`) o uno de `--allow-login`. Sin esa cabecera no ha pasado por `tailscale serve` o viene de un dispositivo etiquetado: `403`. Así otro usuario de la tailnet, o un nodo compartido, no puede lanzar agentes en las máquinas de Juan.
*   Las peticiones de un `fleetd` a otro van por `https://<DNSName>` del otro: las pasa su `tailscale serve`, que pone el login del dueño de la máquina que llama. No hay tokens propios.

### Repos por nombre

*   `POST /v1/tasks` acepta en `repo` una ruta absoluta (como hasta ahora) o un nombre de repo (`fleet`, `Palo`): se resuelve a `<raíz de repos>/<nombre>` en el nodo que la ejecuta. La raíz es `--repos-root` y por defecto `~/Desarrollo`, la misma convención en el PC y el Mac. Un nombre solo admite `[A-Za-z0-9._-]` y no puede ser `.` ni `..`.
*   La tarea guarda la ruta ya resuelta: lo que se ve en la lista es dónde está de verdad.

### Hablar con otro nodo

*   `GET /v1/nodes`: este nodo y los descubiertos, con nombre, URL, versión, si está en línea y cuándo se vio por última vez.
*   `/v1/nodes/{node}/...` reenvía la petición tal cual (método, cuerpo, `text/event-stream` incluido) a `/v1/...` de ese nodo. Para el propio nodo no reenvía: atiende directamente. Así la CLI, la TUI, la web y Palo hablan siempre con su `fleetd` local y llegan a todos.
*   `POST /v1/tasks` con `node` distinto de este reenvía la creación a ese nodo (deja de dar `unknown_node` salvo que el nodo no exista). Con `"node": "auto"` decide el reparto (abajo).
*   Un nodo apagado o dormido (sin respuesta, o `Online: false`) responde `503` con `code: "node_offline"`. No se encola nada para más tarde: quien llama decide si espera o lo lanza en otro sitio.
*   Los listados no se agregan en el servidor: el cliente que quiera ver todo pide `/v1/nodes` y luego `/v1/nodes/{node}/tasks` de cada uno. Cada nodo sigue siendo la fuente de verdad de sus tareas.

### Reparto (`"node": "auto"`)

Por orden, entre los nodos en línea:

1.  Lo que necesita Xcode (el repo tiene `*.xcodeproj`, `*.xcworkspace` o un `Package.swift` con plataformas de Apple) va a un nodo macOS.
2.  Un agente de Ollama va al nodo con GPU para Ollama (`--gpu` en su `fleetd`; el PC).
3.  El resto, al nodo con menos tareas vivas (`provisioning`, `agent`, `preflight`); a igualdad, el propio.

Si ningún nodo en línea cumple la regla 1 o 2, la creación falla con `409 no_capable_node` y el motivo, en vez de lanzarla donde no puede salir bien. El repo tiene que estar clonado en el nodo elegido; si no, falla como cualquier ruta que no existe.

### Mac dormido

*   Una tarea en marcha en el Mac se congela con él y sigue al despertar; nada de eso pasa por la red. Si `fleetd` se reinicia, la recuperación del ADR 0001 vale igual.
*   Lo que deja de funcionar mientras duerme es verlo desde fuera: `/v1/nodes` lo marca fuera de línea y lo que se le reenvía da `node_offline`. Al volver aparece en la siguiente vuelta del descubrimiento.

## Consecuencias

*   `fleetd` necesita un cliente HTTPS asíncrono (`rustls`, sin OpenSSL) para hablar con los otros nodos y reenviar flujos SSE.
*   `fleet-core` lee el JSON de `tailscale status` (sin red: solo analiza) y decide el reparto con datos que le pasan; `fleetd` lanza `tailscale` y hace las peticiones.
*   Una petición por la tailnet pasa a necesitar `Tailscale-User-Login`; abrir la web por la tailnet sigue funcionando porque `tailscale serve` la pone.
*   Probarlo de punta a punta pide `tailscale serve` en el PC y en el Mac, que cambia la configuración de Tailscale de cada máquina (se deshace con `tailscale serve reset`).

## Alternativas descartadas

*   **Un nodo maestro con todo el estado.** Un punto único de fallo y el Mac dormido rompería más cosas; con un `fleetd` por máquina cada uno sigue solo.
*   **La API de Tailscale con una clave.** Pide una clave de API y sale a internet; el socket local da lo mismo sin nada de eso.
*   **Escuchar en la IP de la tailnet con tokens propios.** Rompe la regla de solo loopback y obliga a guardar y rotar secretos; `tailscale serve` ya da HTTPS e identidad.
*   **Agregar las tareas de todos los nodos en el servidor.** Mezcla estados de dos fuentes y complica los errores parciales; el cliente lo hace mejor con `/v1/nodes`.
