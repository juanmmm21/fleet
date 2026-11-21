# 0001 — Alcance, crates y API entre `fleetd` y sus clientes

**Fecha:** 2026-11-21
**Estado:** aceptada

## Contexto

fleet nace del dossier «Fleet: orquestador de agentes y worktrees» (vault, 2026-11-20) y de lo que Palo ya hace por dentro: encargos en worktrees efímeros que solo llegan a `main` en verde (ADR 0023 de Palo) y el CI que se arregla solo (ADR 0030 de Palo). Juan decidió el 2026-11-20 que sea un proyecto propio y no una parte de Palo. Hace falta fijar qué hace fleet, cómo se parte en crates y qué contrato tiene la API, porque Palo (su Fase 59, `FLEET_URL`), la CLI, la TUI y la web van a hablar con ella.

## Decisión

### Alcance

*   **Qué es una tarea.** Un repo, una instrucción y un agente. fleet la lleva de principio a fin: crea el worktree, lanza el agente, pasa las pruebas del repo en el worktree (pre-flight) y, si se lo piden, integra en la rama por defecto.
*   **Qué no hace.** No escribe código por sí mismo, no decide qué tareas lanzar (eso es de quien llama: Juan o Palo), no llama a APIs de LLM de pago y no abre nada a internet.
*   **Estados.** `provisioning` → `agent` → `preflight` → `ready` → `merged`, con `waiting` (el agente pregunta algo y espera respuesta), `failed` y `dropped` (descartada a propósito). Las transiciones las comprueba `fleet-core`; cualquier otra es un error, no un cambio silencioso.
*   **Worktrees.** Rama `fleet/<id>` desde `origin/<rama por defecto>` recién traída, carpeta en `<estado>/worktrees/<repo>-<id>`, fuera del checkout de Juan. El worktree y la rama solo se borran si la rama está integrada o si la tarea se descarta a propósito.
*   **Integración.** Rebase sobre `origin`, pruebas otra vez y `git push` sin forzar. En rojo o con conflicto, la rama se queda y la tarea pasa a `failed` con el motivo.
*   **Un nodo antes que dos.** Todo funciona en una máquina antes de repartir entre el PC y el Mac.

### Crates

*   **`fleet-core`** (biblioteca): tipos (`Task`, `TaskState`, `AgentProvider`, `PreflightResult`), la máquina de estados, el ciclo de vida de los worktrees y el pre-flight. Lanza procesos (`git`, pruebas) pero no abre sockets. Errores con `thiserror`.
*   **`fleetd`** (binario): el demonio. Guarda el estado en SQLite, lanza los agentes y sirve la API HTTP y la web. Es el único que escribe estado.
*   **`fleet`** (binario): la CLI y la TUI. No toca git ni la base de datos: todo lo hace a través de la API de `fleetd`, igual que Palo.

### API

*   **Dónde.** HTTP/1.1 con JSON en `127.0.0.1:7171` (configurable). Nunca en `0.0.0.0`. Para el resto de la tailnet, `tailscale serve`, que pone HTTPS y las cabeceras de identidad de Tailscale.
*   **Versión en la ruta.** Todo bajo `/v1`. Un cambio incompatible es `/v2`; dentro de `/v1` solo se añaden campos.
*   **Rutas.**
    *   `GET /v1/health` — versión y nodo.
    *   `POST /v1/tasks` — crea una tarea (`repo`, `prompt`, `agent`, y opcionalmente `base` y `node`). Responde `201` con la tarea.
    *   `GET /v1/tasks` y `GET /v1/tasks/{id}` — listado y detalle (estado, rama, worktree, ficheros tocados, resultado del pre-flight).
    *   `GET /v1/tasks/{id}/log` y `GET /v1/tasks/{id}/diff` — salida del agente y diff contra la base.
    *   `POST /v1/tasks/{id}/answer` — respuesta a una tarea en `waiting`.
    *   `POST /v1/tasks/{id}/cancel`, `/merge` y `/drop` — cortar el agente, integrar y descartar.
    *   `GET /v1/events` — eventos en directo por SSE (cambios de estado, líneas del agente, avisos de choque).
*   **Errores.** Código HTTP con sentido (`400`, `404`, `409` para una transición no permitida) y cuerpo `{"error": {"code": "...", "message": "..."}}` con un `code` estable para quien programe contra ella.
*   **Identificadores.** Cada tarea tiene un id corto, ordenable por tiempo y válido como nombre de rama y de carpeta.

## Porqué

*   **La CLI va por la API y no por la base de datos:** un solo escritor evita carreras con SQLite y obliga a que la API sirva para todo lo que hace Juan; si a la CLI le basta, a Palo también.
*   **`fleet-core` sin red:** la lógica delicada (estados, worktrees, borrado seguro) se prueba con repos de git de verdad en carpetas temporales, sin levantar el demonio.
*   **HTTP + SSE y no gRPC:** Palo es Python, la web es un navegador y `curl` sirve para depurar. SSE basta para eventos de un solo sentido.
*   **Solo `127.0.0.1` y `tailscale serve`:** es la misma postura que Palo; no hace falta inventar autenticación propia y nada queda abierto aunque el cortafuegos falle.
*   **`/v1` desde el principio:** Palo dependerá de esta API y no se puede romper por debajo.

## Consecuencias

*   El reparto entre PC y Mac (Fase 6) será un `fleetd` por máquina que habla con el otro por la tailnet con esta misma API; no hay un coordinador central.
*   Sin `fleetd` en marcha, la CLI no hace nada útil: dirá que no encuentra el demonio y cómo arrancarlo.
*   Las rutas exactas de estado (`<estado>`) dependen del sistema: `$XDG_STATE_HOME/fleet` o `~/.local/state/fleet` en Linux y macOS.
