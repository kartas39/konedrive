//! A private session bus for tests. Requires `dbus-daemon` (package `dbus-daemon`).

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

pub struct TestBus {
    child: Child,
    address: String,
}

impl TestBus {
    pub fn start() -> Self {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("cannot start dbus-daemon (dnf install dbus-daemon)");
        let mut address = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut address)
            .expect("cannot read the bus address");
        Self { child, address: address.trim().to_owned() }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn builder(&self) -> zbus::connection::Builder<'static> {
        zbus::connection::Builder::address(self.address.as_str()).expect("valid bus address")
    }

    pub async fn connect(&self) -> zbus::Connection {
        self.builder().build().await.expect("cannot connect to the test bus")
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
