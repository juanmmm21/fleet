# fleetd HTTP API (v1)

`fleetd` is the only process that writes fleet's state. The CLI, the TUI, the web UI and any other client (such as an assistant that delegates coding work) talk to it through this API.

## Basics

- **Address.** `http://127.0.0.1:7171` by default (`fleetd --listen`). The daemon refuses to bind anything but loopback. Other machines reach it through `tailscale serve` at `https://<machine>.<tailnet>.ts.net`.
- **Trust.** Requests over loopback are trusted. Requests that come through the tailnet must carry the `Tailscale-User-Login` header that `tailscale serve` adds, and it must be the machine owner's login (or one given with `--allow-login`); otherwise `403 forbidden`. A `POST` whose `Origin` does not match `Host` is rejected (`403 forbidden`), so other websites open in a browser cannot drive the daemon.
- **Format.** JSON in and out, except `/log` and `/diff` (plain text) and `/v1/events` (server-sent events). Timestamps are Unix milliseconds.
- **Versioning.** Everything lives under `/v1`. Within `/v1`, fields are only ever added, never renamed or removed; clients should ignore fields they do not know. An incompatible change would be `/v2`.

## Errors

Every error has an HTTP status and a body with a stable `code`:

```json
{ "error": { "code": "invalid_transition", "message": "cannot go from ready to agent" } }
```

- `400`: `invalid_request`, `invalid_id`
- `403`: `forbidden`
- `404`: `not_found`, `unknown_node`
- `409`: `invalid_transition`, `busy`, `blocked`, `not_running`, `no_worktree`, `no_capable_node`
- `500`: `internal`
- `502`: `node_error`
- `503`: `node_offline`

Program against `code`, not against `message`.

## Tasks

A task is one repository, one prompt and one agent, carried from a fresh worktree to an integrated commit.

### States

```
provisioning → agent → preflight → ready → merged
                 ↕         ↓          ↓
              waiting    failed     failed
```

- `provisioning`: the worktree is being created on branch `fleet/<id>` from a freshly fetched `origin/<base>`.
- `agent`: the agent is working in the worktree.
- `waiting`: the agent asked a question (`question`) and waits for `answer`.
- `preflight`: the repository's tests run in the worktree.
- `ready`: tests are green; `merge` integrates.
- `merged`: rebased, tested again and pushed to `origin/<base>` without force; the worktree is gone.
- `failed`: with a `failure` reason. The branch and the worktree stay.
- `dropped`: discarded on purpose; the worktree and the branch are deleted.

Any task that is not `merged` or `dropped` can be dropped.

### The task object

```json
{
  "id": "00mwqmr69x-lj7t",
  "repo": "/home/juan/Desarrollo/fleet",
  "prompt": "Add a --json flag to `fleet ls`",
  "agent": { "kind": "claude" },
  "base": "main",
  "branch": "fleet/00mwqmr69x-lj7t",
  "node": "pc",
  "client": "assistant:job-42",
  "worktree": {
    "repo": "/home/juan/Desarrollo/fleet",
    "path": "/home/juan/.local/state/fleet/worktrees/fleet-00mwqmr69x-lj7t",
    "branch": "fleet/00mwqmr69x-lj7t",
    "base": "main",
    "base_commit": "725d0543d7f92dc8f14a085eb947b72ed7718095",
    "copied": [".env"],
    "linked": ["target", "AGENTS.md"]
  },
  "state": "ready",
  "failure": null,
  "question": null,
  "preflight": {
    "command": ["cargo", "test"],
    "exit_code": 0,
    "timed_out": false,
    "duration_ms": 40558,
    "output_tail": "test result: ok. ..."
  },
  "session_id": "cec6f6cb-669c-410b-ba5f-52b4f3c6f5b0",
  "diff": {
    "commits": 1,
    "uncommitted": false,
    "files": [{ "path": "crates/fleet/src/args.rs", "kind": "modified" }]
  },
  "created_at_ms": 1795278399605,
  "updated_at_ms": 1795278449988
}
```

- `agent.kind` is `claude`, `agy`, `ollama` (with a required local `model`) or `external`. `claude` and `agy` take an optional `model`.
- `client` is an optional label chosen by whoever created the task; fleet does not interpret it.
- `worktree`, `failure`, `question`, `preflight`, `session_id`, `diff` and `client` may be `null`.
- `preflight.command` is the command that decided the result. When the pre-flight chains several checks (pytest, ruff and mypy in a Python repository), `preflight.checks` lists the ones that ran, in order: the last one is `command`, the one that failed or the final one if all passed. `checks` is absent when there is a single command.
- `diff.files[].kind` is `added`, `modified`, `deleted` or `type_changed`.

### Endpoints

- `GET /v1/health`: `{"node", "version", "os", "gpu", "live_tasks"}`.
- `GET /v1/tasks`: all tasks; `?client=<label>` keeps only that client's.
- `POST /v1/tasks`: create a task (body below). `201` with the task.
- `GET /v1/tasks/{id}`: one task.
- `GET /v1/tasks/{id}/log`: agent and test output so far, as text.
- `GET /v1/tasks/{id}/diff`: the patch against the base, as text.
- `POST /v1/tasks/{id}/answer`: `{"answer": "..."}` for a `waiting` task. `200` with the task.
- `POST /v1/tasks/{id}/done`: an external agent finished; run the tests. `202` with the task.
- `POST /v1/tasks/{id}/cancel`: stop what the task is doing; it ends `failed`. `202`.
- `POST /v1/tasks/{id}/merge`: integrate a `ready` task. `202` with the task.
- `POST /v1/tasks/{id}/drop`: discard on purpose. `200` with the task.
- `GET /v1/overlaps`: pairs of live tasks that touch the same files.
- `GET /v1/nodes`: the fleet nodes on the tailnet, this one included.
- `GET /v1/events`: live events (see below).
- `/v1/nodes/{node}/{rest}`, any method: the same API on another node, e.g. `/v1/nodes/mac/tasks/{id}` (without a second `v1`).

Creating a task:

```json
{
  "repo": "fleet",
  "prompt": "Add a --json flag to `fleet ls`",
  "agent": { "kind": "claude", "model": "opus" },
  "base": "main",
  "node": "auto",
  "client": "assistant:job-42"
}
```

- `repo` is an absolute path on the node that runs the task, or a bare repository name resolved under that node's `--repos-root` (default `~/Desarrollo`). Use names when the task may run on another machine.
- `base` defaults to the default branch of `origin`.
- `node` is a node name, `auto` (Xcode work goes to macOS, Ollama to the GPU node, the rest to the least busy node) or absent (this node).
- `client`: up to 64 characters from `A-Z a-z 0-9 . _ - : /`.

`merge` answers `409 blocked` while an older live task touches the same files; integrate that one first.

### Repository settings

The pre-flight runs the repository's own tests. fleet detects a `Makefile` with a `test` target, Cargo, npm, pytest and Go. Detected pytest projects run with `PYTHONPATH` and `MYPYPATH` pointing at the worktree's `src/` (if any) and its root, ahead of an editable install in the linked `.venv`. If the repository has a `.venv` and its `pyproject.toml` mentions them, `ruff check .` and `mypy` follow pytest with the `.venv`'s Python. mypy gets `--strict src` without a `[tool.mypy]` table, `src` with one and no arguments when that table sets `files`. Without `src/` or `files`, mypy is skipped. The checks run in order within one timeout, and the first failure stops the rest and names its check (`ruff failed: ...`). Otherwise, or to override, add a `fleet.toml` to the repository:

```toml
[preflight]
command = ["make", "test"]
timeout_secs = 900
```

## Bring your own agent

A client that needs to launch the agent itself (its own tools, prompts or quota handling) creates the task with `"agent": {"kind": "external"}`. fleet still owns the worktree, the tests, the overlap warnings and the integration:

1. `POST /v1/tasks` with `"agent": {"kind": "external"}` and a `client` label. Wait until the task is in `agent` (poll or follow `/v1/events`); `worktree.path` is where to work.
2. Run the agent in `worktree.path`. Commit on the task branch; do not push.
3. `POST /v1/tasks/{id}/done`. The task goes to `preflight`, then to `ready` or `failed` (with `preflight.output_tail`).
4. If it failed, fix it in the same worktree and call `done` again. If it is `ready`, `merge` it.
5. To give up, `cancel` (the task fails and keeps its branch) or `drop` (everything is deleted).

An external task in `agent` survives a `fleetd` restart, since its agent never ran inside the daemon. After a client restart, `GET /v1/tasks?client=<label>` finds its tasks again.

```sh
id=$(curl -s -X POST localhost:7171/v1/tasks -H 'content-type: application/json' \
  -d '{"repo":"fleet","prompt":"fix the flaky test","agent":{"kind":"external"},"client":"me:1"}' \
  | jq -r .id)
curl -s localhost:7171/v1/tasks/$id | jq -r .worktree.path   # once the state is "agent"
# ... run your agent there and commit ...
curl -s -X POST localhost:7171/v1/tasks/$id/done
```

## Events

`GET /v1/events` is a server-sent event stream. Each event's name is its type and its data is JSON:

- `task`: `{"type": "task", "task": {...}}` with the whole task, on every change.
- `output`: `{"type": "output", "id", "stream": "stdout" | "stderr", "line"}`, one line of agent output.
- `overlaps`: `{"type": "overlaps", "overlaps": [{"a", "b", "files"}]}`, the full list (possibly empty) whenever a task changes state.
- `lagged`: `{"skipped": n}` when a slow client missed events; fetch `/v1/tasks` again.

A comment is sent every 15 seconds to keep the connection alive. After opening the stream, fetch the task list once so nothing that happened in between is lost.
