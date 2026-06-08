//! JSON persistence roundtrip for saved connections (no live DB / keyring
//! assertions — keyring calls degrade gracefully when no secret service runs).

use sqweel::store::{self, SavedConnection};

#[test]
fn json_roundtrip() {
    let dir = std::env::temp_dir().join(format!("sqweel-store-test-{}", std::process::id()));
    // SAFETY: single-threaded test; set before any glib config-dir lookup.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &dir);
    }
    let _ = std::fs::remove_dir_all(&dir);

    let sc = SavedConnection {
        name: "marwa@db.sqweel.io/analytics".into(),
        driver_id: "postgres".into(),
        host: "db.sqweel.io".into(),
        port: 5432,
        database: "analytics".into(),
        username: "marwa".into(),
        ssl: true,
    };

    store::upsert(sc.clone(), "secret");
    let list = store::load();
    let found = list.iter().find(|c| c.name == sc.name).expect("saved");
    assert_eq!(found.host, "db.sqweel.io");
    assert_eq!(found.port, 5432);
    assert!(found.ssl);
    assert_eq!(found.subtitle(), "marwa@db.sqweel.io:5432/analytics");

    // Upsert is idempotent on name.
    store::upsert(sc.clone(), "secret");
    assert_eq!(store::load().iter().filter(|c| c.name == sc.name).count(), 1);

    store::remove(&sc.name);
    assert!(store::load().iter().all(|c| c.name != sc.name));

    let _ = std::fs::remove_dir_all(&dir);
}
