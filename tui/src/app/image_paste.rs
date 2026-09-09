//! Explicit clipboard pastes keep their original attachment as the target.
use super::*;
use alacritty_terminal::event_loop::{EventLoopSender, Msg};
use std::borrow::Cow;
use std::sync::mpsc::{self, Receiver, TryRecvError};

pub(super) struct ImagePasteFlight {
    uid: String,
    target: EventLoopSender,
    result: Receiver<std::io::Result<Option<crate::clipboard::PasteResult>>>,
}

impl App {
    pub(super) fn start_image_paste(&mut self) -> bool {
        let Some((_, ts)) = self.active_session() else {
            return false;
        };
        if !matches!(ts.session_type.as_str(), "codex" | "claude")
            || ts.session.exited
            || self.reconnecting_sessions.contains(&ts.uid)
        {
            return false;
        }
        if self.image_pastes.iter().any(|p| p.uid == ts.uid) {
            self.set_status_msg("Clipboard paste is still in progress");
            return true;
        }
        let Some(host) = self.hosts.find(&ts.host_id) else {
            return false;
        };
        let transport = host.transport.clone();
        let uid = ts.uid.clone();
        let target = ts.session.sender.clone();
        let (tx, result) = mpsc::channel();
        let spawn = std::thread::Builder::new()
            .name("cm-image-paste".into())
            .spawn(move || {
                let _ = tx.send(crate::clipboard::prepare_paste(&transport));
            });
        match spawn {
            Ok(_) => {
                self.image_pastes.push(ImagePasteFlight {
                    uid,
                    target,
                    result,
                });
                self.set_status_msg("Reading clipboard…");
            }
            Err(e) => self.set_status_msg(&format!("Clipboard paste: {e}")),
        }
        true
    }

    pub(super) fn image_paste_pending_for_active(&self) -> bool {
        self.active_session()
            .is_some_and(|(_, ts)| self.image_pastes.iter().any(|p| p.uid == ts.uid))
    }

    pub(crate) fn drain_image_pastes(&mut self) {
        let mut i = 0;
        while i < self.image_pastes.len() {
            let result = match self.image_pastes[i].result.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Err(std::io::Error::other("clipboard worker disconnected"))
                }
            };
            let paste = self.image_pastes.remove(i);
            match result {
                Ok(Some(result)) => {
                    let bytes = format!("\x1b[200~{}\x1b[201~", result.text).into_bytes();
                    if paste.target.send(Msg::Input(Cow::Owned(bytes))).is_ok() {
                        if let Some(ts) = self
                            .workspaces
                            .iter_mut()
                            .flat_map(|w| &mut w.sessions)
                            .find(|ts| ts.uid == paste.uid)
                        {
                            ts.last_write_at = Some(Instant::now());
                        }
                        self.set_status_msg(if result.image {
                            "Image pasted"
                        } else {
                            "Clipboard pasted"
                        });
                    } else {
                        self.set_status_msg(
                            "Paste target disconnected; paste again after reconnecting",
                        );
                    }
                }
                Ok(None) => {
                    self.set_status_msg("No clipboard image or text found");
                }
                Err(e) => self.set_status_msg(&format!("Clipboard paste: {e}")),
            }
        }
    }
}
