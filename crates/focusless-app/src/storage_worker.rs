use std::{
    path::PathBuf,
    thread::{self, JoinHandle},
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased, unbounded};
use focusless_core::ProjectDocument;
use focusless_storage::{StorageError, save_project};

struct SaveRequest {
    path: PathBuf,
    document: ProjectDocument,
    previous_project_path: Option<PathBuf>,
}

enum Command {
    Manual(Box<SaveRequest>),
    Shutdown,
}

pub enum SaveEvent {
    AutosaveCompleted {
        path: PathBuf,
        result: Result<(), StorageError>,
    },
    ManualSaveCompleted {
        path: PathBuf,
        previous_project_path: Option<PathBuf>,
        result: Result<(), StorageError>,
    },
}

/// Serializes durable writes away from the UI thread.
///
/// Autosaves use a one-item latest-value queue so a slow filesystem cannot
/// accumulate stale snapshots. Manual saves have their own reliable queue.
pub struct StorageWorker {
    command_tx: Sender<Command>,
    autosave_tx: Sender<SaveRequest>,
    autosave_rx_for_replacement: Receiver<SaveRequest>,
    event_rx: Receiver<SaveEvent>,
    join: Option<JoinHandle<()>>,
}

impl StorageWorker {
    pub fn start() -> Self {
        let (command_tx, command_rx) = unbounded();
        let (autosave_tx, autosave_rx) = bounded(1);
        let autosave_rx_for_replacement = autosave_rx.clone();
        let (event_tx, event_rx) = unbounded();
        let join = thread::Builder::new()
            .name("focusless-storage".into())
            .spawn(move || worker_loop(command_rx, autosave_rx, event_tx))
            .expect("failed to create storage worker");
        Self {
            command_tx,
            autosave_tx,
            autosave_rx_for_replacement,
            event_rx,
            join: Some(join),
        }
    }

    pub fn autosave(&self, path: PathBuf, document: ProjectDocument) {
        let request = SaveRequest {
            path,
            document,
            previous_project_path: None,
        };
        match self.autosave_tx.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(request)) => {
                let _ = self.autosave_rx_for_replacement.try_recv();
                let _ = self.autosave_tx.try_send(request);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    pub fn save_manual(
        &self,
        path: PathBuf,
        document: ProjectDocument,
        previous_project_path: Option<PathBuf>,
    ) {
        while self.autosave_rx_for_replacement.try_recv().is_ok() {}
        let _ = self.command_tx.send(Command::Manual(Box::new(SaveRequest {
            path,
            document,
            previous_project_path,
        })));
    }

    #[must_use]
    pub fn try_event(&self) -> Option<SaveEvent> {
        self.event_rx.try_recv().ok()
    }
}

impl Drop for StorageWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(Command::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(
    command_rx: Receiver<Command>,
    autosave_rx: Receiver<SaveRequest>,
    event_tx: Sender<SaveEvent>,
) {
    loop {
        select_biased! {
            recv(command_rx) -> command => match command {
                Ok(Command::Manual(request)) => {
                    let result = save_project(&request.path, &request.document);
                    let _ = event_tx.send(SaveEvent::ManualSaveCompleted {
                        path: request.path,
                        previous_project_path: request.previous_project_path,
                        result,
                    });
                }
                Ok(Command::Shutdown) | Err(_) => break,
            },
            recv(autosave_rx) -> request => match request {
                Ok(request) => {
                    let result = save_project(&request.path, &request.document);
                    let _ = event_tx.send(SaveEvent::AutosaveCompleted {
                        path: request.path,
                        result,
                    });
                }
                Err(_) => break,
            }
        }
    }
}
