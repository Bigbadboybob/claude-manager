use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::api::{ApiClient, Task};
use crate::config::Config;

/// Commands sent from the main thread to the background thread.
pub enum BackendCommand {
    Refresh,
    UpdateTask {
        id: String,
        fields: HashMap<String, serde_json::Value>,
    },
    DeleteTask {
        id: String,
    },
    Shutdown,
}

/// Events sent from the background thread to the main thread.
pub enum BackendEvent {
    TasksUpdated(Vec<Task>),
    ApiError(String),
    Connected,
    Disconnected,
}

/// Handle to the background polling thread.
pub struct BackendHandle {
    pub cmd_tx: mpsc::Sender<BackendCommand>,
    pub event_rx: mpsc::Receiver<BackendEvent>,
    thread: Option<JoinHandle<()>>,
}

impl BackendHandle {
    pub fn spawn(config: &Config) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
        let (event_tx, event_rx) = mpsc::channel::<BackendEvent>();

        let client = ApiClient::new(config);

        let thread = thread::spawn(move || {
            backend_loop(client, cmd_rx, event_tx);
        });

        BackendHandle {
            cmd_tx,
            event_rx,
            thread: Some(thread),
        }
    }

    /// Request an immediate refresh.
    pub fn refresh(&self) {
        let _ = self.cmd_tx.send(BackendCommand::Refresh);
    }

    /// Send a task update command.
    pub fn update_task(&self, id: String, fields: HashMap<String, serde_json::Value>) {
        let _ = self.cmd_tx.send(BackendCommand::UpdateTask { id, fields });
    }

    /// Send a task delete command.
    pub fn delete_task(&self, id: String) {
        let _ = self.cmd_tx.send(BackendCommand::DeleteTask { id });
    }

    /// Shut down the background thread.
    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(BackendCommand::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn backend_loop(
    client: ApiClient,
    cmd_rx: mpsc::Receiver<BackendCommand>,
    event_tx: mpsc::Sender<BackendEvent>,
) {
    let mut was_connected = false;

    // Immediate first fetch.
    do_refresh(&client, &event_tx, &mut was_connected);

    loop {
        // Wait for a command or timeout after 5 seconds (periodic refresh).
        match cmd_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(BackendCommand::Shutdown) => break,
            Ok(BackendCommand::Refresh) => {
                do_refresh(&client, &event_tx, &mut was_connected);
            }
            Ok(BackendCommand::UpdateTask { id, fields }) => {
                match client.update_task(&id, &fields) {
                    Ok(_) => {
                        // Refresh to get the updated state.
                        do_refresh(&client, &event_tx, &mut was_connected);
                    }
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::DeleteTask { id }) => {
                match client.delete_task(&id) {
                    Ok(_) => {
                        do_refresh(&client, &event_tx, &mut was_connected);
                    }
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic refresh.
                do_refresh(&client, &event_tx, &mut was_connected);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Main thread dropped the sender — shut down.
                break;
            }
        }
    }
}

fn do_refresh(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    was_connected: &mut bool,
) {
    match client.list_tasks(None) {
        Ok(tasks) => {
            if !*was_connected {
                let _ = event_tx.send(BackendEvent::Connected);
                *was_connected = true;
            }
            let _ = event_tx.send(BackendEvent::TasksUpdated(tasks));
        }
        Err(e) => {
            if *was_connected {
                let _ = event_tx.send(BackendEvent::Disconnected);
                *was_connected = false;
            }
            let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
        }
    }
}
