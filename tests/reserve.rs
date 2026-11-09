// Integration tests for port reservations: ownership, expiry and persistence across reloads.

use std::time::Duration;

use uswctl::auth::{Refusal, Role};
use uswctl::reserve::ReservationStore;

const SWITCH: &str = "xg10";
const PORT: u8 = 3;
const MINUTE: Duration = Duration::from_secs(60);

fn store_temp() -> (tempfile::TempDir, ReservationStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = ReservationStore::reservation_store_open(dir.path().join("reservations.json"));
    (dir, store)
}

fn agent(owner: &str) -> Role {
    Role::Agent {
        owner: Some(owner.to_string()),
    }
}

#[test]
fn reserve_then_other_owner_refused() {
    let (_dir, store) = store_temp();
    store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    let err = store
        .reservation_check(SWITCH, PORT, &agent("agent-y"))
        .unwrap_err();
    match err {
        Refusal::Reserved { owner, .. } => assert_eq!(owner, "agent-x"),
        other => panic!("expected Reserved, got {other:?}"),
    }
}

#[test]
fn owner_allowed() {
    let (_dir, store) = store_temp();
    store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    assert_eq!(
        store.reservation_check(SWITCH, PORT, &agent("agent-x")),
        Ok(())
    );
}

#[test]
fn agent_without_owner_is_refused_on_a_reserved_port() {
    let (_dir, store) = store_temp();
    store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    let role = Role::Agent { owner: None };
    assert!(matches!(
        store.reservation_check(SWITCH, PORT, &role),
        Err(Refusal::Reserved { .. })
    ));
}

#[test]
fn user_always_passes_the_check() {
    let (_dir, store) = store_temp();
    store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    assert_eq!(store.reservation_check(SWITCH, PORT, &Role::User), Ok(()));
}

#[test]
fn unreserved_port_is_open() {
    let (_dir, store) = store_temp();
    assert_eq!(
        store.reservation_check(SWITCH, PORT, &Role::Agent { owner: None }),
        Ok(())
    );
}

#[test]
fn expired_reservation_ignored() {
    let (_dir, store) = store_temp();
    store
        .reservation_add(SWITCH, PORT, "agent-x", Duration::ZERO)
        .unwrap();
    assert_eq!(
        store.reservation_check(SWITCH, PORT, &Role::Agent { owner: None }),
        Ok(())
    );
    assert!(store.reservation_list().is_empty());
    // The port is open again: a second owner can take the expired reservation's place.
    store
        .reservation_add(SWITCH, PORT, "agent-y", MINUTE)
        .unwrap();
    assert_eq!(
        store.reservation_check(SWITCH, PORT, &agent("agent-y")),
        Ok(())
    );
}

#[test]
fn user_releases_any() {
    let (_dir, store) = store_temp();
    let r = store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    store.reservation_release(r.id, &Role::User).unwrap();
    assert!(store.reservation_list().is_empty());
}

#[test]
fn agent_cannot_release_other() {
    let (_dir, store) = store_temp();
    let r = store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    let err = store
        .reservation_release(r.id, &agent("agent-y"))
        .unwrap_err();
    assert_eq!(err, Refusal::NotOwner);
    assert_eq!(store.reservation_list().len(), 1);
}

#[test]
fn agent_without_owner_cannot_release() {
    let (_dir, store) = store_temp();
    let r = store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    let err = store
        .reservation_release(r.id, &Role::Agent { owner: None })
        .unwrap_err();
    assert_eq!(err, Refusal::NoOwner);
    assert_eq!(store.reservation_list().len(), 1);
}

#[test]
fn agent_releases_own() {
    let (_dir, store) = store_temp();
    let r = store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    store.reservation_release(r.id, &agent("agent-x")).unwrap();
    assert!(store.reservation_list().is_empty());
}

#[test]
fn release_unknown_id_not_found() {
    let (_dir, store) = store_temp();
    let err = store.reservation_release(999, &Role::User).unwrap_err();
    assert_eq!(err, Refusal::NotFound(999));
}

#[test]
fn second_add_same_port_refused() {
    let (_dir, store) = store_temp();
    store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    let err = store
        .reservation_add(SWITCH, PORT, "agent-y", MINUTE)
        .unwrap_err();
    match err {
        Refusal::Reserved { owner, .. } => assert_eq!(owner, "agent-x"),
        other => panic!("expected Reserved, got {other:?}"),
    }
    assert_eq!(store.reservation_list().len(), 1);
}

#[test]
fn extend_by_owner_updates_expiry() {
    let (_dir, store) = store_temp();
    let r = store
        .reservation_add(SWITCH, PORT, "agent-x", Duration::from_secs(1))
        .unwrap();
    let extended = store.reservation_extend(r.id, "agent-x", MINUTE).unwrap();
    assert!(extended.expires > r.expires);
}

#[test]
fn extend_by_other_owner_refused() {
    let (_dir, store) = store_temp();
    let r = store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    let err = store
        .reservation_extend(r.id, "agent-y", MINUTE)
        .unwrap_err();
    assert_eq!(err, Refusal::NotOwner);
}

#[test]
fn extend_unknown_id_not_found() {
    let (_dir, store) = store_temp();
    let err = store
        .reservation_extend(999, "agent-x", MINUTE)
        .unwrap_err();
    assert_eq!(err, Refusal::NotFound(999));
}

#[test]
fn corrupt_file_gives_storage_error_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reservations.json");
    std::fs::write(&path, b"not valid json").unwrap();
    let store = ReservationStore::reservation_store_open(path.clone());

    let err = store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap_err();
    assert!(matches!(err, Refusal::StorageError(_)), "got {err:?}");

    // The store itself is still usable: `load` returns a `Refusal` instead of panicking, so
    // the mutex is never poisoned, and a later call on the same `ReservationStore` works once
    // the file is fixed.
    std::fs::write(&path, b"{\"next_id\":1,\"reservations\":[]}").unwrap();
    let r = store
        .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
        .unwrap();
    assert_eq!(r.owner, "agent-x");
}

#[test]
fn list_on_corrupt_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reservations.json");
    std::fs::write(&path, b"not valid json").unwrap();
    let store = ReservationStore::reservation_store_open(path);
    assert_eq!(store.reservation_list(), Vec::new());
}

#[test]
fn store_survives_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reservations.json");
    {
        let store = ReservationStore::reservation_store_open(path.clone());
        store
            .reservation_add(SWITCH, PORT, "agent-x", MINUTE)
            .unwrap();
    }
    let reloaded = ReservationStore::reservation_store_open(path);
    let list = reloaded.reservation_list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].owner, "agent-x");
    assert_eq!(list[0].switch, SWITCH);
    assert_eq!(list[0].port, PORT);
    // The id counter also survives: a new reservation does not reuse id 1.
    let second = reloaded
        .reservation_add(SWITCH, 4, "agent-y", MINUTE)
        .unwrap();
    assert_eq!(second.id, 2);
}
