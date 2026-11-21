# 0003 — API estable para clientes: agente externo y etiqueta de cliente

**Fecha:** 2026-11-21
**Estado:** aceptada

## Contexto

Palo es el primer cliente de fleet (Fase 59 de Palo, `FLEET_URL`). Hoy hace sus encargos en worktrees efímeros (ADR 0023 de Palo) y lanza él mismo `claude` con lo que solo tiene Palo: sus servidores MCP (`ask_juan` para preguntar a Juan por Telegram, el navegador, la pantalla), su prompt de sistema, el control del cupo y el paso a Gemini cuando se acaba. Su Fase 59 dice qué pasa a fleet (el ciclo de vida del worktree, las pruebas antes de integrar, el aviso de choques) y qué se queda en Palo (los encargos, las preguntas a Juan, los avisos y el cupo).

Con la API del ADR 0001, fleet lanza siempre el agente él mismo (`claude`, `agy` u Ollama). Si Palo la usara tal cual, perdería todo lo anterior o habría que meterlo en fleet, que es justo lo que no queremos (fleet no sabe nada de Telegram ni de Juan). Además, Palo se reinicia a menudo y tiene que encontrar sus tareas después sin guardar aparte qué id de fleet corresponde a cada encargo.

## Decisión

*   **Agente externo.** `"agent": {"kind": "external"}`. fleet crea el worktree y deja la tarea en `agent` sin lanzar nada. El cliente trabaja en `worktree.path` (lo da la tarea) con su propio agente, hace sus commits en la rama `fleet/<id>` y al acabar llama a `POST /v1/tasks/{id}/done` (`202`). fleet mira el diff, pasa el pre-flight y la deja en `ready` o en `failed` con el resultado, como con un agente propio. Desde ahí todo es igual: `merge` integra con rebase, pruebas y `push` sin forzar, `drop` descarta, y la tarea cuenta para los choques.
*   **Reintento.** `done` vale también desde `failed` (un pre-flight rojo, un corte): la tarea vuelve a `agent`, luego al pre-flight. Es lo que hace Palo cuando sus pruebas salen en rojo y le pide a Claude que lo arregle en el mismo worktree.
*   **Cancelar.** Una tarea externa en `agent` no tiene trabajo de fondo en fleet: `cancel` la pasa a `failed` y se quedan su rama y su worktree. Cortar el proceso del agente es cosa del cliente.
*   **Reinicio de `fleetd`.** Una tarea externa en `agent` se queda en `agent`: su agente no corría dentro del demonio y puede seguir trabajando. Las demás se recuperan como hasta ahora (ADR 0001, `recovery`).
*   **Etiqueta de cliente.** `"client"` opcional al crear (hasta 64 caracteres de `[A-Za-z0-9._:/-]`, p. ej. `palo:<encargo>`), guardado en la tarea y filtrable con `GET /v1/tasks?client=...`. fleet no la interpreta. Viaja con la tarea al nodo que la crea.
*   **Contrato.** La referencia de la API en inglés está en `docs/api.md`. Los campos de una tarea quedan fijados por un test (`TASK_FIELDS` en `crates/fleetd/tests/api.rs`): dentro de `/v1` solo se añaden.
*   **Errores.** `done` sobre una tarea con agente propio → `400 invalid_request`; en otro estado → `409 invalid_transition`; sin worktree → `409 no_worktree`. `cancel` de una tarea que no existe pasa a ser `404 not_found` (antes daba `409 not_running`, que era un error).

## Porqué

*   **El agente fuera y el resto dentro:** lo que Palo hace distinto (MCP, preguntas, cupo) está en cómo lanza el agente; lo que hace igual que fleet (worktree, pruebas, rebase, choques) es exactamente lo que fleet ya tiene probado con repos de verdad. Partir por ahí evita duplicar ninguna de las dos cosas.
*   **`done` explícito y no adivinar:** fleet no puede saber cuándo ha acabado un proceso que no es suyo; un aviso del cliente es lo más simple y no depende de vigilar ficheros.
*   **Etiqueta y no tabla de correspondencias en Palo:** con el filtro, Palo puede reconstruir tras un reinicio qué tareas son suyas preguntando a fleet, la única fuente de verdad del estado.

## Consecuencias

*   Una tarea externa en otro nodo es posible (el worktree está en esa máquina), pero entonces el cliente tiene que poder trabajar allí. Palo la usará en el PC; para el Mac (iOS) puede usar un agente propio de fleet (`claude`), que no necesita a Palo en esa máquina.
*   La web y la TUI muestran estas tareas como las demás (agente `external`); no se crean desde ahí, porque no hay agente que lanzar.
*   Lo que quede del lado de Palo (caída a `palo/worktrees/` si fleet falla, avisos, cupo) es su ADR 0056.
