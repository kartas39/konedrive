//! A private session bus for tests. Requires `dbus-daemon` (package `dbus-daemon`).

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// How long a call on [`TestBus::connect`]'s connection waits for its reply. A call that gets
/// none then fails where it was made, with the test's name, instead of waiting for ever
/// (quality finding `SY13`); it is long, so that a busy machine does not run it out.
pub const METHOD_TIMEOUT: Duration = Duration::from_secs(120);

/// The bus's configuration: the session bus's own (`/usr/share/dbus-1/session.conf`) without
/// its service directories, without what it includes (the legacy `/etc/dbus-1/session.conf`,
/// `session.d`, `/etc/dbus-1/session.d`, `/etc/dbus-1/session-local.conf`, the SELinux
/// contexts) and without the two limits on starting services (`service_start_timeout`,
/// `max_pending_service_starts`). A bus that reads no service directory can start no
/// program: not an installed `konedrived`, which would run on the real `~/.config` (quality
/// finding `DB2`).
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

/// Writes [`CONFIG`] to a file of this bus's own in the temporary directory: one that was
/// not there (never a file somebody else made, nor a link), readable by its owner only,
/// under a name nobody can foresee. A name that is taken is passed over for another.
fn write_config() -> std::path::PathBuf {
    use std::hash::{BuildHasher, Hasher};
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        // A new `RandomState` has keys the system's random source seeded.
        let random = std::collections::hash_map::RandomState::new().build_hasher().finish();
        let path = std::env::temp_dir().join(format!("konedrive-test-bus-{random:016x}.conf"));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file) => {
                file.write_all(CONFIG.as_bytes()).expect("cannot write the test bus's configuration");
                return path;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot write the test bus's configuration {}: {e}", path.display()),
        }
    }
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
