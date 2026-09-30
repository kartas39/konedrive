//! The automatic hold's sources (`docs/design/writes.md` §11, issue #57): whether the
//! connection is metered, whether the machine runs on battery, and whether the power profile
//! is `power-saver`. One watcher for the daemon, on the system bus, as `sync::network` is:
//! it follows each source's `PropertiesChanged` and tells every account through the hub
//! ([`HelperHub::set_conditions`]), each of which decides by the hold's settings, one pair
//! for every account (`sync::running`).
//!
//! - **Metered**: NetworkManager's `Metered` on `/org/freedesktop/NetworkManager` is `1`
//!   (yes) or `3` (guessed yes).
//! - **On battery**: UPower's `OnBattery` on `/org/freedesktop/UPower`.
//! - **Power saver**: `ActiveProfile` of `org.freedesktop.UPower.PowerProfiles` on
//!   `/org/freedesktop/UPower/PowerProfiles`, or of the older `net.hadess.PowerProfiles` on
//!   `/net/hadess/PowerProfiles` when that is the name present.
//!
//! A source that is missing or cannot be read counts as "no reason to hold back" (not
//! metered, on mains, another profile), logged once at `info`.

use std::sync::Arc;

use futures_util::stream::{select_all, StreamExt};
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::{BusName, InterfaceName};
use zbus::zvariant::OwnedValue;

use super::hub::HelperHub;
use super::running::Conditions;

/// NetworkManager's `NM_METERED_YES` and `NM_METERED_GUESS_YES`.
const METERED_YES: u32 = 1;
const METERED_GUESS_YES: u32 = 3;

/// One property the hold follows.
#[derive(Debug, Clone, Copy)]
struct Source {
    /// What the log calls it.
    what: &'static str,
    service: &'static str,
    path: &'static str,
    interface: &'static str,
    property: &'static str,
}

const NETWORK: Source = Source {
    what: "NetworkManager",
    service: "org.freedesktop.NetworkManager",
    path: "/org/freedesktop/NetworkManager",
    interface: "org.freedesktop.NetworkManager",
    property: "Metered",
};

const BATTERY: Source = Source {
    what: "UPower",
    service: "org.freedesktop.UPower",
    path: "/org/freedesktop/UPower",
    interface: "org.freedesktop.UPower",
    property: "OnBattery",
};

const PROFILES: Source = Source {
    what: "power-profiles-daemon",
    service: "org.freedesktop.UPower.PowerProfiles",
    path: "/org/freedesktop/UPower/PowerProfiles",
    interface: "org.freedesktop.UPower.PowerProfiles",
    property: "ActiveProfile",
};

/// The older name of [`PROFILES`], used when it is the one present.
const PROFILES_OLD: Source = Source {
    what: "power-profiles-daemon",
    service: "net.hadess.PowerProfiles",
    path: "/net/hadess/PowerProfiles",
    interface: "net.hadess.PowerProfiles",
    property: "ActiveProfile",
};

/// Tells every account of `hub` what the sources say, now and at each change. For the life
/// of the daemon; never in a test (it is the system bus).
pub async fn watch(hub: Arc<HelperHub>) {
    match zbus::Connection::system().await {
        Ok(connection) => watch_on(&connection, move |conditions| hub.set_conditions(conditions)).await,
        Err(e) => tracing::info!("no system bus ({e}); no account holds back on a metered connection or on battery"),
    }
}

/// Calls `tell` with what the sources on `connection` say, once at the start and again at
/// each change. Returns when every source's signals have ended.
pub async fn watch_on(connection: &zbus::Connection, tell: impl Fn(Conditions)) {
    let profiles = match has_owner(connection, PROFILES_OLD.service).await && !has_owner(connection, PROFILES.service).await {
        true => PROFILES_OLD,
        false => PROFILES,
    };
    let sources = [NETWORK, BATTERY, profiles];
    let mut proxies = Vec::new();
    let mut streams = Vec::new();
    // Subscribed before the first read, so that no change between the two is missed.
    for (at, source) in sources.iter().enumerate() {
        match subscribe(connection, source).await {
            Ok((proxy, changes)) => {
                streams.push(changes.map(move |_| at).boxed());
                proxies.push(Some(proxy));
            }
            Err(e) => {
                tracing::info!("cannot follow {} ({e}): it gives no reason to hold back", source.what);
                proxies.push(None);
            }
        }
    }
    let mut values: Vec<Option<OwnedValue>> = Vec::new();
    for (proxy, source) in proxies.iter().zip(&sources) {
        values.push(read(proxy.as_ref(), source, true).await);
    }
    let mut told = conditions(&values);
    tell(told);
    let mut changes = select_all(streams);
    while let Some(at) = changes.next().await {
        values[at] = read(proxies[at].as_ref(), &sources[at], false).await;
        let now = conditions(&values);
        if now != told {
            tell(now);
            told = now;
        }
    }
    tracing::info!("the power and network sources' signals ended; the hold stays as it was last worked out");
}

async fn has_owner(connection: &zbus::Connection, name: &'static str) -> bool {
    let Ok(dbus) = DBusProxy::new(connection).await else { return false };
    let Ok(name) = BusName::try_from(name) else { return false };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

async fn subscribe(
    connection: &zbus::Connection,
    source: &Source,
) -> zbus::Result<(PropertiesProxy<'static>, zbus::fdo::PropertiesChangedStream)> {
    let proxy = PropertiesProxy::builder(connection)
        .destination(source.service)?
        .path(source.path)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let changes = proxy.receive_properties_changed().await?;
    Ok((proxy, changes))
}

/// The source's property now; `None` when it cannot be read, logged at `info` the first
/// time (`first`) and at `debug` after.
async fn read(proxy: Option<&PropertiesProxy<'static>>, source: &Source, first: bool) -> Option<OwnedValue> {
    let proxy = proxy?;
    let interface = InterfaceName::try_from(source.interface).ok()?;
    match proxy.get(interface, source.property).await {
        Ok(value) => Some(value),
        Err(e) if first => {
            tracing::info!("{} is not reachable ({e}): it gives no reason to hold back", source.what);
            None
        }
        Err(e) => {
            tracing::debug!("cannot read {} from {}: {e}", source.property, source.what);
            None
        }
    }
}

/// What the values read of the network, the battery and the profile say.
fn conditions(values: &[Option<OwnedValue>]) -> Conditions {
    let metered = values[0].as_ref().and_then(|v| u32::try_from(v).ok()).is_some_and(|m| m == METERED_YES || m == METERED_GUESS_YES);
    let on_battery = values[1].as_ref().and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
    let power_saver = values[2].as_ref().and_then(|v| <&str>::try_from(v).ok()).is_some_and(|p| p == "power-saver");
    Conditions { metered, on_battery, power_saver }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use konedrive_dbus::testing::TestBus;

    use super::*;

    struct FakeNetworkManager {
        metered: u32,
    }

    #[zbus::interface(name = "org.freedesktop.NetworkManager")]
    impl FakeNetworkManager {
        #[zbus(property)]
        fn metered(&self) -> u32 {
            self.metered
        }
    }

    struct FakeUPower {
        on_battery: bool,
    }

    #[zbus::interface(name = "org.freedesktop.UPower")]
    impl FakeUPower {
        #[zbus(property)]
        fn on_battery(&self) -> bool {
            self.on_battery
        }
    }

    struct FakeProfiles {
        profile: String,
    }

    #[zbus::interface(name = "org.freedesktop.UPower.PowerProfiles")]
    impl FakeProfiles {
        #[zbus(property)]
        fn active_profile(&self) -> String {
            self.profile.clone()
        }
    }

    struct FakeOldProfiles {
        profile: String,
    }

    #[zbus::interface(name = "net.hadess.PowerProfiles")]
    impl FakeOldProfiles {
        #[zbus(property)]
        fn active_profile(&self) -> String {
            self.profile.clone()
        }
    }

    /// Every value `watch_on` told, in order.
    fn follow(connection: zbus::Connection) -> Arc<Mutex<Vec<Conditions>>> {
        let told = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&told);
        tokio::spawn(async move { watch_on(&connection, move |c| kept.lock().unwrap().push(c)).await });
        told
    }

    async fn wait_for(told: &Mutex<Vec<Conditions>>, wanted: Conditions) {
        for _ in 0..250 {
            if told.lock().unwrap().last() == Some(&wanted) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never told {wanted:?}; told {:?}", told.lock().unwrap());
    }

    /// The sources are read at the start and followed: each change is told once, and
    /// NetworkManager's guessed yes counts as metered.
    #[tokio::test]
    async fn the_sources_are_read_and_followed() {
        let bus = TestBus::start();
        let nm = bus.builder().name("org.freedesktop.NetworkManager").unwrap().serve_at(NETWORK.path, FakeNetworkManager { metered: 4 }).unwrap().build().await.unwrap();
        let upower = bus.builder().name("org.freedesktop.UPower").unwrap().serve_at(BATTERY.path, FakeUPower { on_battery: true }).unwrap().build().await.unwrap();
        let profiles = bus
            .builder()
            .name("org.freedesktop.UPower.PowerProfiles")
            .unwrap()
            .serve_at(PROFILES.path, FakeProfiles { profile: "balanced".into() })
            .unwrap()
            .build()
            .await
            .unwrap();
        let told = follow(bus.connect().await);
        wait_for(&told, Conditions { metered: false, on_battery: true, power_saver: false }).await;

        let network = nm.object_server().interface::<_, FakeNetworkManager>(NETWORK.path).await.unwrap();
        network.get_mut().await.metered = 3;
        network.get().await.metered_changed(network.signal_emitter()).await.unwrap();
        wait_for(&told, Conditions { metered: true, on_battery: true, power_saver: false }).await;

        let profile = profiles.object_server().interface::<_, FakeProfiles>(PROFILES.path).await.unwrap();
        profile.get_mut().await.profile = "power-saver".into();
        profile.get().await.active_profile_changed(profile.signal_emitter()).await.unwrap();
        wait_for(&told, Conditions { metered: true, on_battery: true, power_saver: true }).await;

        let battery = upower.object_server().interface::<_, FakeUPower>(BATTERY.path).await.unwrap();
        battery.get_mut().await.on_battery = false;
        battery.get().await.on_battery_changed(battery.signal_emitter()).await.unwrap();
        wait_for(&told, Conditions { metered: true, on_battery: false, power_saver: true }).await;
        assert_eq!(told.lock().unwrap().len(), 4, "each change once: {:?}", told.lock().unwrap());
    }

    /// With no source on the bus, nothing is a reason to hold back.
    #[tokio::test]
    async fn no_source_is_no_reason_to_hold_back() {
        let bus = TestBus::start();
        let told = follow(bus.connect().await);
        wait_for(&told, Conditions::default()).await;
    }

    /// The older power-profiles name is followed when it is the one present.
    #[tokio::test]
    async fn the_older_power_profiles_name_is_read_when_it_is_the_one_present() {
        let bus = TestBus::start();
        let _old = bus
            .builder()
            .name("net.hadess.PowerProfiles")
            .unwrap()
            .serve_at(PROFILES_OLD.path, FakeOldProfiles { profile: "power-saver".into() })
            .unwrap()
            .build()
            .await
            .unwrap();
        let told = follow(bus.connect().await);
        wait_for(&told, Conditions { power_saver: true, ..Conditions::default() }).await;
    }
}
