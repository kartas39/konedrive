//! The tests' private bus (`konedrive_dbus::testing::TestBus`): what it must never do, and
//! the timeout a test's connection to it has.

use konedrive_dbus::testing::{TestBus, METHOD_TIMEOUT};

/// The bus knows no service file, so a call to a name nobody owns starts no program —
/// not the `konedrived` a package installed, which would run on the real `~/.config`.
#[tokio::test]
async fn the_bus_can_start_no_program() {
    let bus = TestBus::start();
    let client = bus.connect().await;
    let dbus = zbus::fdo::DBusProxy::new(&client).await.unwrap();

    let startable = dbus.list_activatable_names().await.unwrap();
    let startable: Vec<&str> = startable.iter().map(|name| name.as_str()).collect();
    assert_eq!(startable, ["org.freedesktop.DBus"], "the bus itself is the only name that needs no owner");

    let refused = client
        .call_method(Some(konedrive_dbus::SERVICE_NAME), konedrive_dbus::ACCOUNTS_PATH, Some("org.freedesktop.DBus.Peer"), "Ping", &())
        .await
        .expect_err("nobody owns the daemon's name, and nothing is started to own it");
    assert!(
        matches!(&refused, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"),
        "{refused:?}"
    );
}

/// A test's connection is made with the method timeout. That a call with no reply then
/// fails is zbus's to keep: no test here waits the timeout out.
#[tokio::test]
async fn a_tests_connection_has_the_method_timeout() {
    let bus = TestBus::start();
    assert_eq!(bus.connect().await.method_timeout(), Some(METHOD_TIMEOUT));
}
