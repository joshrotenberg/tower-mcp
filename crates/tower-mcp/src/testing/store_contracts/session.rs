//! Contract suite for [`SessionStore`].

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::violated;
use crate::protocol::{
    ClientCapabilities, ElicitationCapability, ElicitationFormCapability, Implementation,
    RootsCapability, SamplingCapability, ToolIcon,
};
use crate::session_store::{SessionRecord, SessionStore};

const KIND: &str = "SessionStore";

/// Lifetime of records that must stay live for the whole check.
const LIVE: Duration = Duration::from_secs(3600);

/// Lifetime of records the expiry check lets lapse. Whole seconds, so a
/// backend that stores its TTL at second precision still accepts it.
const SHORT: Duration = Duration::from_secs(2);

/// Run the [`SessionStore`] contract against the store `make` builds.
///
/// `make` is called once per check and must return an empty store each time.
/// The suite covers:
///
/// - a record round trip through `create` and `load`, with every field
///   preserved: the protocol version, `client_info`, `client_capabilities`,
///   and the `principal` the session is bound to (`None`, a name, and the
///   empty string stay distinct, since the principal is authorization data);
/// - `create` keeps records apart when the caller's ID collides with an
///   existing session, and reports the ID it settled on in the record;
/// - `save` is an upsert that trusts the caller's ID, and an overwrite
///   replaces the whole record, including clearing optional fields;
/// - `load` answers `None` for an unknown ID;
/// - `load` answers `None` once a record's `expires_at` has passed, whether
///   or not the store has deleted it yet;
/// - `delete` removes only the named record and is idempotent.
///
/// The expiry check waits for two-second records to lapse, so the suite takes a
/// few seconds and must run in real time, not under `tokio::time::pause`.
/// Timestamps are not compared for equality: the trait does not fix their
/// storage precision, so a backend that truncates them to milliseconds is
/// still correct.
///
/// # Panics
///
/// Panics, naming the violated rule, at the first check the store fails.
///
/// # Example
///
/// ```rust
/// use tower_mcp::session_store::MemorySessionStore;
/// use tower_mcp::testing::store_contracts::session_store_contract;
///
/// # #[tokio::main]
/// # async fn main() {
/// session_store_contract(MemorySessionStore::new).await;
/// # }
/// ```
pub async fn session_store_contract<S, F>(make: F)
where
    S: SessionStore,
    F: Fn() -> S,
{
    round_trip(&make()).await;
    principal_round_trip(&make()).await;
    create_keeps_colliding_ids_apart(&make()).await;
    save_is_an_upsert(&make()).await;
    overwrite_replaces_the_record(&make()).await;
    unknown_sessions(&make()).await;
    expired_sessions(&make()).await;
    delete_is_scoped_and_idempotent(&make()).await;
}

fn record(id: &str) -> SessionRecord {
    SessionRecord::new(id, "2025-11-25", LIVE)
}

fn json_of<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("contract fixtures serialize")
}

/// A client identity that sets every field.
fn full_client_info() -> Implementation {
    Implementation {
        name: "contract-client".to_string(),
        version: "9.9.9".to_string(),
        title: Some("Contract Client".to_string()),
        description: Some("Exercises every field".to_string()),
        icons: Some(vec![ToolIcon {
            src: "https://example.com/icon.png".to_string(),
            mime_type: Some("image/png".to_string()),
            sizes: Some(vec!["48x48".to_string()]),
            theme: None,
        }]),
        website_url: Some("https://example.com".to_string()),
        meta: Some(json!({"io.example/build": "42"})),
    }
}

/// Client capabilities that set several distinct fields.
fn full_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        roots: Some(RootsCapability {
            list_changed: true,
            deprecated: None,
        }),
        sampling: Some(SamplingCapability::default()),
        elicitation: Some(ElicitationCapability {
            form: Some(ElicitationFormCapability::default()),
            url: None,
        }),
        experimental: Some(HashMap::from([("contract".to_string(), json!({"n": 1}))])),
        extensions: Some(HashMap::from([(
            "io.modelcontextprotocol/tasks".to_string(),
            json!({}),
        )])),
        ..Default::default()
    }
}

async fn create<S: SessionStore>(
    store: &S,
    mut record: SessionRecord,
    rule: &str,
) -> SessionRecord {
    ok!(rule, store.create(&mut record).await);
    record
}

async fn load<S: SessionStore>(store: &S, id: &str, rule: &str) -> SessionRecord {
    match ok!(rule, store.load(id).await) {
        Some(record) => record,
        None => violated(
            KIND,
            rule,
            format_args!("load({id:?}) returned None for a record that must be present"),
        ),
    }
}

async fn round_trip<S: SessionStore>(store: &S) {
    let rule = "a created record loads back with every field intact";
    let mut original = record("round-trip");
    original.protocol_version = "2026-07-28".to_string();
    original.client_info = Some(full_client_info());
    original.client_capabilities = Some(full_capabilities());
    original.principal = Some("alice".to_string());

    let created = create(store, original.clone(), rule).await;
    let loaded = load(store, &created.id, rule).await;

    check!(
        loaded.id == created.id,
        rule,
        "load returned id {:?}, expected {:?}",
        loaded.id,
        created.id
    );
    check!(
        loaded.protocol_version == "2026-07-28",
        rule,
        "protocol_version is {:?}",
        loaded.protocol_version
    );
    check!(
        json_of(&loaded.client_info) == json_of(&original.client_info),
        rule,
        "client_info changed: {}",
        json_of(&loaded.client_info)
    );
    check!(
        json_of(&loaded.client_capabilities) == json_of(&original.client_capabilities),
        rule,
        "client_capabilities changed: {}",
        json_of(&loaded.client_capabilities)
    );
    check!(
        loaded.principal.as_deref() == Some("alice"),
        rule,
        "principal is {:?}",
        loaded.principal
    );
    check!(
        !loaded.is_expired(),
        rule,
        "a record with a one hour lifetime loaded as already expired"
    );

    // A record that carries nothing optional comes back carrying nothing.
    let rule = "a record without optional fields loads back without them";
    let bare = create(store, record("bare"), rule).await;
    let loaded = load(store, &bare.id, rule).await;
    check!(
        loaded.client_info.is_none()
            && loaded.client_capabilities.is_none()
            && loaded.principal.is_none(),
        rule,
        "load invented client_info {:?}, client_capabilities {:?}, or principal {:?}",
        loaded.client_info,
        loaded.client_capabilities,
        loaded.principal
    );
}

async fn principal_round_trip<S: SessionStore>(store: &S) {
    // The principal decides which requests may use the session, so a store that
    // drops it, or folds `Some("")` into `None`, changes who is authorized.
    let rule = "the principal is persisted unchanged";
    for principal in [None, Some("alice"), Some("bob"), Some("")] {
        let mut original = record(&format!("principal-{principal:?}"));
        original.principal = principal.map(str::to_string);
        let created = create(store, original, rule).await;
        let loaded = load(store, &created.id, rule).await;
        check!(
            loaded.principal.as_deref() == principal,
            rule,
            "created with {principal:?}, loaded as {:?}",
            loaded.principal
        );
    }

    // Saving a new value replaces the old one rather than keeping it.
    let rule = "save replaces the principal";
    let mut session = create(store, record("rebind"), rule).await;
    session.principal = Some("alice".to_string());
    ok!(rule, store.save(&session).await);
    session.principal = Some("bob".to_string());
    ok!(rule, store.save(&session).await);
    let loaded = load(store, &session.id, rule).await;
    check!(
        loaded.principal.as_deref() == Some("bob"),
        rule,
        "saved as bob, loaded as {:?}",
        loaded.principal
    );
}

async fn create_keeps_colliding_ids_apart<S: SessionStore>(store: &S) {
    let rule = "create keeps a colliding record apart from the existing session";
    let mut first = record("collide");
    first.principal = Some("alice".to_string());
    let first = create(store, first, rule).await;

    let mut second = record("collide");
    second.principal = Some("bob".to_string());
    let second = create(store, second, rule).await;

    check!(
        first.id != second.id,
        rule,
        "both records ended up with id {:?}",
        first.id
    );
    let loaded_first = load(store, &first.id, rule).await;
    check!(
        loaded_first.principal.as_deref() == Some("alice"),
        rule,
        "the first session was overwritten: its principal is now {:?}",
        loaded_first.principal
    );
    let loaded_second = load(store, &second.id, rule).await;
    check!(
        loaded_second.principal.as_deref() == Some("bob"),
        rule,
        "the second session loaded with principal {:?}",
        loaded_second.principal
    );
}

async fn save_is_an_upsert<S: SessionStore>(store: &S) {
    // `save` trusts the caller's ID, so it inserts a record `create` never saw.
    let rule = "save inserts a record under the caller's id";
    let mut fresh = record("chosen-by-caller");
    fresh.principal = Some("alice".to_string());
    ok!(rule, store.save(&fresh).await);
    let loaded = load(store, "chosen-by-caller", rule).await;
    check!(
        loaded.id == "chosen-by-caller" && loaded.principal.as_deref() == Some("alice"),
        rule,
        "load returned id {:?} with principal {:?}",
        loaded.id,
        loaded.principal
    );

    // Saving twice leaves one record holding the second write.
    let rule = "save overwrites an existing record";
    let mut updated = fresh.clone();
    updated.protocol_version = "2026-07-28".to_string();
    ok!(rule, store.save(&updated).await);
    let loaded = load(store, "chosen-by-caller", rule).await;
    check!(
        loaded.protocol_version == "2026-07-28",
        rule,
        "protocol_version is still {:?} after a second save",
        loaded.protocol_version
    );
}

async fn overwrite_replaces_the_record<S: SessionStore>(store: &S) {
    let rule = "an overwrite replaces the whole record, including clearing optional fields";
    let mut session = record("overwritten");
    session.client_info = Some(full_client_info());
    session.client_capabilities = Some(full_capabilities());
    session.principal = Some("alice".to_string());
    let mut session = create(store, session, rule).await;

    session.client_info = None;
    session.client_capabilities = None;
    session.principal = None;
    ok!(rule, store.save(&session).await);

    let loaded = load(store, &session.id, rule).await;
    check!(
        loaded.client_info.is_none(),
        rule,
        "client_info survived being cleared: {:?}",
        loaded.client_info
    );
    check!(
        loaded.client_capabilities.is_none(),
        rule,
        "client_capabilities survived being cleared: {:?}",
        loaded.client_capabilities
    );
    check!(
        loaded.principal.is_none(),
        rule,
        "the principal survived being cleared: {:?}",
        loaded.principal
    );
}

async fn unknown_sessions<S: SessionStore>(store: &S) {
    let rule = "load answers None for an unknown id";
    check!(
        ok!(rule, store.load("no-such-session").await).is_none(),
        rule,
        "load returned a record"
    );
    check!(
        ok!(rule, store.load("").await).is_none(),
        rule,
        "load(\"\") returned a record"
    );
}

async fn expired_sessions<S: SessionStore>(store: &S) {
    let rule = "load answers None for an expired record";
    let created = create(
        store,
        SessionRecord::new("lapsing", "2025-11-25", SHORT),
        rule,
    )
    .await;
    let saved = SessionRecord::new("lapsing-saved", "2025-11-25", SHORT);
    ok!(rule, store.save(&saved).await);
    let kept = create(store, record("outlives"), rule).await;

    // Both short-lived records are present before they lapse.
    load(store, &created.id, rule).await;
    load(store, &saved.id, rule).await;

    tokio::time::sleep(SHORT + Duration::from_millis(200)).await;

    for id in [&created.id, &saved.id] {
        check!(
            ok!(rule, store.load(id).await).is_none(),
            rule,
            "load({id:?}) returned a record whose expires_at has passed"
        );
    }
    load(store, &kept.id, rule).await;
}

async fn delete_is_scoped_and_idempotent<S: SessionStore>(store: &S) {
    let rule = "delete removes the named record and only that record";
    let doomed = create(store, record("doomed"), rule).await;
    let kept = create(store, record("kept"), rule).await;

    ok!(rule, store.delete(&doomed.id).await);
    check!(
        ok!(rule, store.load(&doomed.id).await).is_none(),
        rule,
        "the record was still loadable after delete"
    );
    load(store, &kept.id, rule).await;

    let rule = "delete is idempotent";
    ok!(rule, store.delete(&doomed.id).await);
    ok!(rule, store.delete("never-existed").await);
    load(store, &kept.id, rule).await;
}
