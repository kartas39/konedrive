//! A private session bus for tests. Requires `dbus-daemon` (package `dbus-daemon`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// How long a call on [`TestBus::connect`]'s connection waits for its reply. A call that gets
/// none then fails where it was made, with the test's name, instead of waiting for ever
/// (quality finding `SY13`); it is long, so that a busy machine does not run it out.
pub const METHOD_TIMEOUT: Duration = Duration::from_secs(120);

/// The bus's configuration: the session bus's own (`/usr/share/dbus-1/session.conf`) without
/// its service directories and without the machine's additions (`session.d`,
/// `session-local.conf`). A bus that reads no service directory can start no program: not
/// an installed `konedrived`, which would run on the real `~/.config` (quality finding
/// `DB2`).
const CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
  <limit name="max_incoming_bytes">1000000000</limit>
  <limit name="max_incoming_unix_fds">250000000</limit>
  <limit name="max_outgoing_bytes">1000000000</limit>
  <limit name="max_outgoing_unix_fds">250000000</limit>
  <limit name="max_message_size">1000000000</limit>
  <limit name="auth_timeout">240000</limit>
  <limit name="pending_fd_timeout">150000</limit>
  <limit name="max_completed_connections">100000</limit>
  <limit name="max_incomplete_connections">10000</limit>
  <limit name="max_connections_per_user">100000</limit>
  <limit name="max_names_per_connection">50000</limit>
  <limit name="max_match_rules_per_connection">50000</limit>
  <limit name="max_replies_per_connection">50000</limit>
</busconfig>
"#;

/// Writes [`CONFIG`] to a file of this bus's own in the temporary directory.
fn write_config() -> std::path::PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!("konedrive-test-bus-{}-{}.conf", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let path = std::env::temp_dir().join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("cannot write the test bus's configuration {}: {e}", path.display()));
    file.write_all(CONFIG.as_bytes()).expect("cannot write the test bus's configuration");
    path
}

pub struct TestBus {
    child: Child,
    address: String,
}

impl TestBus {
    pub fn start() -> Self {
        let config = write_config();
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("cannot start dbus-daemon (dnf install dbus-daemon)");
        let mut address = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut address)
            .expect("cannot read the bus address");
        // The bus has read its configuration by the time it prints its address.
        let _ = std::fs::remove_file(&config);
        Self { child, address: address.trim().to_owned() }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn builder(&self) -> zbus::connection::Builder<'static> {
        zbus::connection::Builder::address(self.address.as_str()).expect("valid bus address")
    }

    /// A test's own connection: every call on it gives up after [`METHOD_TIMEOUT`].
    pub async fn connect(&self) -> zbus::Connection {
        self.builder().method_timeout(METHOD_TIMEOUT).build().await.expect("cannot connect to the test bus")
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
