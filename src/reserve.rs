// Port reservations (spec section 6.8): one owner at a time per switch port, kept in a JSON
// file on the daemon host so they survive a daemon restart.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::auth::{Refusal, Role};

const STORE_MODE: u32 = 0o600;
const DEFAULT_FILE_NAME: &str = "reservations.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    pub id: u64,
    pub switch: String,
    pub port: u8,
    pub owner: String,
    // Serialized through serde's own `SystemTime` support (seconds and nanoseconds since the
    // epoch), so the file holds no custom date format.
    pub created: SystemTime,
    pub expires: SystemTime,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    next_id: u64,
    reservations: Vec<Reservation>,
}

fn store_file_empty() -> StoreFile {
    StoreFile {
        next_id: 1,
        reservations: Vec::new(),
    }
}

pub struct ReservationStore {
    path: PathBuf,
    // Serializes the read-modify-write sequence of every method below, so that two calls in
    // the same process cannot both read the same `next_id` or overwrite each other's write.
    lock: Mutex<()>,
}

impl ReservationStore {
    pub fn reservation_store_open(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    // A read or write failure here is a disk fault on the daemon host, not a caller
    // mistake, so it becomes `Refusal::StorageError` rather than one of the request-shaped
    // variants. It must not panic while the mutex guard below is held: a panicking lock
    // holder poisons the `Mutex`, and every later call would then panic too until the
    // daemon restarts.
    fn load(&self) -> Result<StoreFile, Refusal> {
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                let mut file: StoreFile = serde_json::from_str(&text).map_err(|e| {
                    Refusal::StorageError(format!("parse {}: {e}", self.path.display()))
                })?;
                let now = SystemTime::now();
                file.reservations.retain(|r| r.expires > now);
                Ok(file)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(store_file_empty()),
            Err(e) => Err(Refusal::StorageError(format!(
                "read {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn save(&self, file: &StoreFile) -> Result<(), Refusal> {
        let text = serde_json::to_string_pretty(file).map_err(|e| {
            Refusal::StorageError(format!("serialize {}: {e}", self.path.display()))
        })?;
        let file_name = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(DEFAULT_FILE_NAME);
        let tmp_path = self.path.with_file_name(format!("{file_name}.tmp"));
        let mut tmp = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(STORE_MODE)
            .open(&tmp_path)
            .map_err(|e| Refusal::StorageError(format!("create {}: {e}", tmp_path.display())))?;
        tmp.write_all(text.as_bytes())
            .map_err(|e| Refusal::StorageError(format!("write {}: {e}", tmp_path.display())))?;
        drop(tmp);
        fs::rename(&tmp_path, &self.path).map_err(|e| {
            Refusal::StorageError(format!(
                "rename {} to {}: {e}",
                tmp_path.display(),
                self.path.display()
            ))
        })?;
        Ok(())
    }

    /// Reserves `port` on `switch` for `owner` for `ttl`. Refused with the current holder's
    /// name and expiry if the port already has a live reservation (ruling 5; the caller name
    /// to record is `owner`, decided by the API, not by this store).
    pub fn reservation_add(
        &self,
        switch: &str,
        port: u8,
        owner: &str,
        ttl: Duration,
    ) -> Result<Reservation, Refusal> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.load()?;
        if let Some(existing) = file
            .reservations
            .iter()
            .find(|r| r.switch == switch && r.port == port)
        {
            return Err(Refusal::Reserved {
                owner: existing.owner.clone(),
                expires: existing.expires,
            });
        }
        let now = SystemTime::now();
        let reservation = Reservation {
            id: file.next_id,
            switch: switch.to_string(),
            port,
            owner: owner.to_string(),
            created: now,
            expires: now + ttl,
        };
        file.next_id += 1;
        file.reservations.push(reservation.clone());
        self.save(&file)?;
        Ok(reservation)
    }

    /// Sets a new expiry on reservation `id`, counted from now. Only its owner may extend it.
    pub fn reservation_extend(
        &self,
        id: u64,
        owner: &str,
        ttl: Duration,
    ) -> Result<Reservation, Refusal> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.load()?;
        let Some(r) = file.reservations.iter_mut().find(|r| r.id == id) else {
            return Err(Refusal::NotFound(id));
        };
        if r.owner != owner {
            return Err(Refusal::NotOwner);
        }
        r.expires = SystemTime::now() + ttl;
        let updated = r.clone();
        self.save(&file)?;
        Ok(updated)
    }

    /// Releases reservation `id`. Its owner may release it; so may `Role::User`, from any
    /// switch (spec 6.8: "The user can release any reservation from the web page").
    pub fn reservation_release(&self, id: u64, role: &Role) -> Result<(), Refusal> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.load()?;
        let Some(pos) = file.reservations.iter().position(|r| r.id == id) else {
            return Err(Refusal::NotFound(id));
        };
        match role {
            Role::User => {}
            Role::Agent { owner: None } => return Err(Refusal::NoOwner),
            Role::Agent { owner: Some(name) } if *name != file.reservations[pos].owner => {
                return Err(Refusal::NotOwner);
            }
            Role::Agent { .. } => {}
        }
        file.reservations.remove(pos);
        self.save(&file)?;
        Ok(())
    }

    /// All live reservations, across every switch. A storage error is logged and reported as
    /// an empty list rather than an error, because the fixed return type has no room for one;
    /// a caller that needs to tell "no reservations" from "store unreadable" should use
    /// `reservation_check` or another call that does return `Result`.
    pub fn reservation_list(&self) -> Vec<Reservation> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        match self.load() {
            Ok(file) => file.reservations,
            Err(e) => {
                tracing::warn!("reservation store {}: {e:?}", self.path.display());
                Vec::new()
            }
        }
    }

    /// Whether `role` may change or act on `port` of `switch` right now (ruling 5):
    /// `Role::User` always passes; an agent passes only as the reservation's own owner;
    /// a port with no reservation is open to every caller.
    pub fn reservation_check(&self, switch: &str, port: u8, role: &Role) -> Result<(), Refusal> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let file = self.load()?;
        let Some(r) = file
            .reservations
            .iter()
            .find(|r| r.switch == switch && r.port == port)
        else {
            return Ok(());
        };
        match role {
            Role::User => Ok(()),
            Role::Agent { owner: Some(name) } if *name == r.owner => Ok(()),
            _ => Err(Refusal::Reserved {
                owner: r.owner.clone(),
                expires: r.expires,
            }),
        }
    }
}
