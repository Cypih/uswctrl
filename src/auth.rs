// Caller roles from the two bearer tokens, and the safety rules for ports and switch-wide
// settings (spec sections 8 and 10).

use std::time::SystemTime;

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use serde::Serialize;

use crate::config::{Config, SwitchConfig};

const OWNER_HEADER: &str = "X-Uswctl-Owner";
const BEARER_PREFIX: &str = "Bearer ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Agent { owner: Option<String> },
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Port(u8),
    Global,
    Reboot,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Refusal {
    Unauthorized,
    UserOnly(&'static str),
    Reserved {
        owner: String,
        expires: SystemTime,
    },
    /// An Agent tried to add, extend or release a reservation with no owner name.
    NoOwner,
    /// The caller is not the owner of the reservation that it tried to extend or release.
    NotOwner,
    /// A reservation id that `ReservationStore` does not know.
    NotFound(u64),
    /// `ReservationStore` could not read or write its file (I/O error or corrupt JSON).
    StorageError(String),
}

fn bearer_token_get(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix(BEARER_PREFIX)
}

fn owner_header_get(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(OWNER_HEADER)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The caller's role from `Authorization: Bearer <token>` and, for the agent token, the
/// owner name from `X-Uswctl-Owner`. `None` if the header is missing or the token matches
/// neither of the two configured tokens.
pub fn role_from_headers(cfg: &Config, headers: &HeaderMap) -> Option<Role> {
    let token = bearer_token_get(headers)?;
    if token == cfg.user_token {
        return Some(Role::User);
    }
    if token == cfg.agent_token {
        return Some(Role::Agent {
            owner: owner_header_get(headers),
        });
    }
    None
}

/// The protected-port and switch-wide rules of spec section 8: only `Role::User` can touch a
/// protected port, a switch-wide setting, or a reboot. Reservations are a separate check
/// (`ReservationStore::reservation_check`), because they need the store, not just the config.
pub fn safety_check(role: &Role, sw: &SwitchConfig, target: Target) -> Result<(), Refusal> {
    if *role == Role::User {
        return Ok(());
    }
    match target {
        Target::Port(port) if sw.protected_ports.contains(&port) => {
            Err(Refusal::UserOnly("protected port"))
        }
        Target::Port(_) => Ok(()),
        Target::Global => Err(Refusal::UserOnly("switch-wide setting")),
        Target::Reboot => Err(Refusal::UserOnly("reboot")),
    }
}
