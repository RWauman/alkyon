//! Azure: signing in to Entra ID, and reading a storage account.

pub mod entra;
pub mod storage;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use entra::{Credential, DeviceCode, Resource, Tokens};

/// A sign-in in flight, waited on by a browser somewhere and polled by the UI.
///
/// Tickets exist so that the tokens never travel to the page. The dialogue
/// starts a sign-in, watches it finish, and then registers the source quoting a
/// ticket; the refresh token goes from here straight to the keychain without the
/// browser ever holding it.
enum Progress {
    Pending,
    Ready(Box<Tokens>),
    Failed(String),
}

struct SignIn {
    progress: Progress,
    /// What to show while it is pending, for the device-code flow.
    prompt: Option<DeviceCode>,
    started: Instant,
}

/// How long an unfinished sign-in is kept before it is swept away. Longer than
/// either flow can legitimately take.
const TICKET_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// What the UI is told while it waits.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Pending,
    Ready { account: String },
    Failed { error: String },
    /// Swept, or never issued.
    Unknown,
}

#[derive(Default)]
pub struct SignIns {
    tickets: Mutex<HashMap<String, SignIn>>,
}

impl SignIns {
    fn insert(&self, ticket: String, sign_in: SignIn) {
        let mut tickets = self.tickets.lock().unwrap();
        tickets.retain(|_, held| held.started.elapsed() < TICKET_LIFETIME);
        tickets.insert(ticket, sign_in);
    }

    fn finish(&self, ticket: &str, outcome: crate::error::Result<Tokens>) {
        let mut tickets = self.tickets.lock().unwrap();
        // Gone means swept or already redeemed; there is nothing to record.
        let Some(sign_in) = tickets.get_mut(ticket) else {
            return;
        };
        sign_in.progress = match outcome {
            Ok(tokens) => Progress::Ready(Box::new(tokens)),
            Err(e) => Progress::Failed(e.to_string()),
        };
    }

    /// Open the browser on this machine and wait for the redirect.
    pub fn start_interactive(
        self: &std::sync::Arc<Self>,
        credential: Credential,
        resource: Resource,
    ) -> String {
        let ticket = uuid::Uuid::new_v4().to_string();
        self.insert(
            ticket.clone(),
            SignIn {
                progress: Progress::Pending,
                prompt: None,
                started: Instant::now(),
            },
        );

        // Detached on purpose: the browser takes as long as it takes, and the
        // request that started it must not be the thing holding it open.
        let store = self.clone();
        let waiting = ticket.clone();
        tokio::spawn(async move {
            let outcome = entra::interactive(&credential, resource).await;
            store.finish(&waiting, outcome);
        });
        ticket
    }

    /// Ask for a code to type in elsewhere, then poll until it is used.
    pub async fn start_device(
        self: &std::sync::Arc<Self>,
        credential: Credential,
        resource: Resource,
    ) -> crate::error::Result<(String, DeviceCode)> {
        // Awaited rather than spawned: there is nothing to show the user until
        // Entra has handed over the code.
        let code = entra::device_code(&credential, resource).await?;
        let ticket = uuid::Uuid::new_v4().to_string();
        self.insert(
            ticket.clone(),
            SignIn {
                progress: Progress::Pending,
                prompt: Some(code.clone()),
                started: Instant::now(),
            },
        );

        let store = self.clone();
        let waiting = ticket.clone();
        let polling = code.clone();
        tokio::spawn(async move {
            let outcome = entra::await_device_code(&credential, &polling).await;
            store.finish(&waiting, outcome);
        });
        Ok((ticket, code))
    }

    pub fn status(&self, ticket: &str) -> Status {
        let tickets = self.tickets.lock().unwrap();
        match tickets.get(ticket) {
            None => Status::Unknown,
            Some(sign_in) => match &sign_in.progress {
                Progress::Pending => Status::Pending,
                Progress::Ready(tokens) => Status::Ready {
                    account: tokens.account.clone(),
                },
                Progress::Failed(error) => Status::Failed {
                    error: error.clone(),
                },
            },
        }
    }

    /// The tokens of a finished sign-in, left where they are.
    ///
    /// *Test* uses this: pressing it must not spend the sign-in that the save
    /// right afterwards is going to need.
    pub fn peek(&self, ticket: &str) -> Option<Tokens> {
        let tickets = self.tickets.lock().unwrap();
        match tickets.get(ticket) {
            Some(SignIn {
                progress: Progress::Ready(tokens),
                ..
            }) => Some((**tokens).clone()),
            _ => None,
        }
    }

    /// Take the tokens out. One use: a ticket is spent registering one source.
    pub fn redeem(&self, ticket: &str) -> Option<Tokens> {
        let taken = self.peek(ticket);
        if taken.is_some() {
            self.tickets.lock().unwrap().remove(ticket);
        }
        taken
    }

    /// What a pending device sign-in is waiting to be told.
    pub fn prompt(&self, ticket: &str) -> Option<DeviceCode> {
        let tickets = self.tickets.lock().unwrap();
        tickets.get(ticket).and_then(|s| s.prompt.clone())
    }
}
