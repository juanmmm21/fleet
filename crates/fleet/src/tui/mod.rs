//! `fleet tui`: tareas, su estado, la salida del agente y el diff, en directo.

pub mod app;
pub mod ui;

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{Action, App};

use crate::client::{Client, ClientError, StreamEvent};

/// Cada cuánto se mira el teclado si no llega nada del demonio.
const TICK: Duration = Duration::from_millis(200);
/// Espera antes de volver a conectar con el demonio tras perderlo.
const RECONNECT: Duration = Duration::from_secs(2);

/// Lo que llega del hilo que sigue los eventos.
enum Incoming {
    Connected,
    Event(StreamEvent),
    Disconnected(String),
}

/// Abre la TUI y la deja como estaba la terminal al salir, también si algo falla.
pub fn run(client: &Client) -> Result<(), ClientError> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, client);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, client: &Client) -> Result<(), ClientError> {
    let (sender, incoming) = mpsc::channel();
    spawn_follower(client.base_url().to_owned(), sender);
    let mut app = App::new();
    let action = refresh(&mut app, client);
    perform(&mut app, client, action)?;
    loop {
        terminal
            .draw(|frame| ui::draw(frame, &app))
            .map_err(|err| ClientError::Protocol(format!("cannot draw: {err}")))?;
        drain(&mut app, client, &incoming)?;
        let ready =
            event::poll(TICK).map_err(|err| ClientError::Protocol(format!("terminal: {err}")))?;
        if !ready {
            continue;
        }
        let read =
            event::read().map_err(|err| ClientError::Protocol(format!("terminal: {err}")))?;
        if let Event::Key(key) = read
            && key.kind == KeyEventKind::Press
        {
            let action = app.on_key(key);
            if action == Action::Quit {
                return Ok(());
            }
            perform(&mut app, client, action)?;
        }
    }
}

/// Lo que haya llegado del demonio desde la última vuelta, sin esperar.
fn drain(app: &mut App, client: &Client, incoming: &Receiver<Incoming>) -> Result<(), ClientError> {
    while let Ok(message) = incoming.try_recv() {
        match message {
            Incoming::Connected => {
                app.set_connected(true);
                let action = refresh(app, client);
                perform(app, client, action)?;
            }
            Incoming::Event(event) => {
                let action = app.on_event(&event);
                perform(app, client, action)?;
            }
            Incoming::Disconnected(why) => {
                app.set_connected(false);
                app.set_status(why);
            }
        }
    }
    Ok(())
}

/// Hace contra la API lo que pide la acción. Los errores de la API se enseñan en la línea de
/// estado; solo un fallo que impide seguir sube.
fn perform(app: &mut App, client: &Client, action: Action) -> Result<(), ClientError> {
    let result = match action {
        Action::None | Action::Quit => Ok(()),
        Action::Refresh => {
            let next = refresh(app, client);
            return perform(app, client, next);
        }
        Action::LoadLog(id) => client.log(&id).map(|log| app.set_log(id, &log)),
        Action::LoadDiff(id) => client.diff(&id).map(|diff| app.set_diff(id, diff)),
        Action::Merge(id) => client
            .merge(&id)
            .map(|_| app.set_status(format!("integrating {id}: rebase, tests and push"))),
        Action::Cancel(id) => client
            .cancel(&id)
            .map(|()| app.set_status(format!("cancelling {id}"))),
        Action::Drop(id) => client
            .drop_task(&id)
            .map(|_| app.set_status(format!("dropped {id}"))),
        Action::Answer(id, answer) => client
            .answer(&id, &answer)
            .map(|_| app.set_status(format!("{id} is back with the agent"))),
    };
    if let Err(err) = result {
        app.set_status(err.to_string());
    }
    Ok(())
}

fn refresh(app: &mut App, client: &Client) -> Action {
    match client.list() {
        Ok(tasks) => {
            app.set_connected(true);
            app.set_tasks(tasks)
        }
        Err(err) => {
            app.set_connected(false);
            app.set_status(err.to_string());
            Action::None
        }
    }
}

/// Un hilo que sigue `/v1/events` y vuelve a conectar si se pierde el demonio. Acaba cuando la
/// TUI se cierra (el receptor desaparece).
fn spawn_follower(url: String, sender: Sender<Incoming>) {
    thread::spawn(move || {
        let client = Client::new(&url);
        loop {
            let opened = sender.clone();
            let events = sender.clone();
            let result = client.follow(
                move || Ok(opened.send(Incoming::Connected).is_ok()),
                move |event| events.send(Incoming::Event(event)).is_ok(),
            );
            let why = match result {
                Ok(()) => "fleetd closed the event stream".to_owned(),
                Err(err) => err.to_string(),
            };
            if sender.send(Incoming::Disconnected(why)).is_err() {
                return;
            }
            thread::sleep(RECONNECT);
        }
    });
}
