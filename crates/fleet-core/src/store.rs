//! Estado de las tareas en SQLite (`fleet.sqlite3`), para que un reinicio del demonio
//! retome las que estaban en marcha.
//!
//! Cada fila guarda la [`Task`] entera como JSON y, aparte, las columnas por las que se
//! filtra y ordena. Así un campo nuevo de `Task` no obliga a migrar el esquema.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::task::{Task, TaskId, TaskState};

/// Migraciones en orden; la versión del esquema es cuántas se han aplicado
/// (`PRAGMA user_version`). Solo se añaden al final, nunca se editan.
const MIGRATIONS: &[&str] = &["CREATE TABLE tasks (
        id TEXT PRIMARY KEY NOT NULL,
        state TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        task TEXT NOT NULL
    ) STRICT;
    CREATE INDEX tasks_state ON tasks (state);"];

/// Cuánto espera una escritura si otra conexión tiene el fichero bloqueado.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot (de)serialize task {id}: {source}")]
    Json {
        id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("the database at {path} has schema version {found}; this fleet knows up to {known}")]
    SchemaTooNew {
        path: PathBuf,
        found: i64,
        known: i64,
    },
    #[error("the database at {path} has an invalid schema version {found}")]
    InvalidSchema { path: PathBuf, found: i64 },
    #[error("task {id} has an unknown state `{state}` in the database")]
    UnknownState { id: String, state: String },
    #[error("task {0} already exists")]
    Duplicate(TaskId),
    #[error("task {0} does not exist")]
    NotFound(TaskId),
    #[error("task {id} is {stored} in the database and cannot be saved as {to}")]
    StaleUpdate {
        id: TaskId,
        stored: TaskState,
        to: TaskState,
    },
    #[error("timestamp {0} does not fit in sqlite")]
    Timestamp(u64),
    #[error("the database connection was poisoned by a panic")]
    Poisoned,
    #[error("the database worker died: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Acceso al fichero de estado. Es barato de clonar: todas las copias comparten la conexión.
#[derive(Debug, Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Abre (o crea) la base de datos y la lleva a la última versión del esquema.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        let conn = tokio::task::spawn_blocking(move || open_blocking(&path)).await??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Guarda una tarea nueva; falla si ya hay una con ese id.
    pub async fn insert(&self, task: &Task) -> Result<(), StoreError> {
        let row = Row::from_task(task)?;
        self.with_conn(move |conn| {
            let inserted = conn.execute(
                "INSERT INTO tasks (id, state, created_at_ms, updated_at_ms, task)
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (id) DO NOTHING",
                params![
                    row.id.as_str(),
                    row.state.as_str(),
                    row.created_at_ms,
                    row.updated_at_ms,
                    row.json
                ],
            )?;
            if inserted == 0 {
                return Err(StoreError::Duplicate(row.id));
            }
            Ok(())
        })
        .await
    }

    /// Guarda los cambios de una tarea que ya existe. Cada cambio de estado se guarda en cuanto
    /// ocurre: lo guardado y lo nuevo están en el mismo estado o a una transición válida.
    /// Así una copia vieja de la tarea no pisa un estado más reciente (ni resucita una tarea
    /// terminada) y un reinicio no pierde pasos.
    pub async fn update(&self, task: &Task) -> Result<(), StoreError> {
        let row = Row::from_task(task)?;
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let stored: Option<String> = tx
                .query_row(
                    "SELECT state FROM tasks WHERE id = ?1",
                    [row.id.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(stored) = stored else {
                return Err(StoreError::NotFound(row.id));
            };
            let stored = parse_state(row.id.as_str(), &stored)?;
            let allowed = if stored.is_terminal() {
                false
            } else {
                stored == row.state || stored.can_transition(row.state)
            };
            if !allowed {
                return Err(StoreError::StaleUpdate {
                    id: row.id,
                    stored,
                    to: row.state,
                });
            }
            tx.execute(
                "UPDATE tasks SET state = ?2, updated_at_ms = ?3, task = ?4 WHERE id = ?1",
                params![
                    row.id.as_str(),
                    row.state.as_str(),
                    row.updated_at_ms,
                    row.json
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn get(&self, id: &TaskId) -> Result<Option<Task>, StoreError> {
        let id = id.clone();
        self.with_conn(move |conn| {
            let json: Option<String> = conn
                .query_row("SELECT task FROM tasks WHERE id = ?1", [id.as_str()], |r| {
                    r.get(0)
                })
                .optional()?;
            json.map(|json| decode(id.as_str(), &json)).transpose()
        })
        .await
    }

    /// Todas las tareas, de la más antigua a la más nueva.
    pub async fn list(&self) -> Result<Vec<Task>, StoreError> {
        self.query_tasks("SELECT id, task FROM tasks ORDER BY created_at_ms, id")
            .await
    }

    /// Las que no están integradas ni descartadas: lo que el demonio retoma al arrancar.
    pub async fn live(&self) -> Result<Vec<Task>, StoreError> {
        self.query_tasks(
            "SELECT id, task FROM tasks WHERE state NOT IN ('merged', 'dropped')
             ORDER BY created_at_ms, id",
        )
        .await
    }

    async fn query_tasks(&self, sql: &'static str) -> Result<Vec<Task>, StoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(sql)?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut tasks = Vec::new();
            for row in rows {
                let (id, json) = row?;
                tasks.push(decode(&id, &json)?);
            }
            Ok(tasks)
        })
        .await
    }

    /// SQLite bloquea el hilo: cada acceso va a un hilo de `spawn_blocking`.
    async fn with_conn<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| StoreError::Poisoned)?;
            f(&mut guard)
        })
        .await?
    }
}

/// Columnas de una tarea ya preparadas para escribirse.
struct Row {
    id: TaskId,
    state: TaskState,
    created_at_ms: i64,
    updated_at_ms: i64,
    json: String,
}

impl Row {
    fn from_task(task: &Task) -> Result<Self, StoreError> {
        let json = serde_json::to_string(task).map_err(|source| StoreError::Json {
            id: task.id.to_string(),
            source,
        })?;
        Ok(Self {
            id: task.id.clone(),
            state: task.state,
            created_at_ms: to_sql_ms(task.created_at_ms)?,
            updated_at_ms: to_sql_ms(task.updated_at_ms)?,
            json,
        })
    }
}

fn to_sql_ms(ms: u64) -> Result<i64, StoreError> {
    i64::try_from(ms).map_err(|_| StoreError::Timestamp(ms))
}

fn decode(id: &str, json: &str) -> Result<Task, StoreError> {
    serde_json::from_str(json).map_err(|source| StoreError::Json {
        id: id.to_owned(),
        source,
    })
}

fn parse_state(id: &str, raw: &str) -> Result<TaskState, StoreError> {
    TaskState::ALL
        .into_iter()
        .find(|state| state.as_str() == raw)
        .ok_or_else(|| StoreError::UnknownState {
            id: id.to_owned(),
            state: raw.to_owned(),
        })
}

fn open_blocking(path: &Path) -> Result<Connection, StoreError> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // WAL: la CLI puede leer mientras el demonio escribe
    conn.pragma_update(None, "journal_mode", "WAL")?;
    migrate(&mut conn, path)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection, path: &Path) -> Result<(), StoreError> {
    let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // Un fleet viejo no debe escribir en un esquema que no entiende
    if found > known {
        return Err(StoreError::SchemaTooNew {
            path: path.to_owned(),
            found,
            known,
        });
    }
    let Ok(applied) = usize::try_from(found) else {
        return Err(StoreError::InvalidSchema {
            path: path.to_owned(),
            found,
        });
    };
    let pending = MIGRATIONS.iter().skip(applied);
    for sql in pending {
        tx.execute_batch(sql)?;
    }
    if found < known {
        tx.pragma_update(None, "user_version", known)?;
    }
    tx.commit()?;
    Ok(())
}
