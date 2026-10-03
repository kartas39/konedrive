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
