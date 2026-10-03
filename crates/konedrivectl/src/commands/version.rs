use konedrive_dbus::accounts::AccountsProxy;

/// `--version`: this build's line, then the running daemon's. A daemon that is not on the bus is
/// not started (D-Bus activation) just to be asked; that is no failure.
pub(crate) async fn print_version() {
    use konedrive_dbus::version::{COMMIT, VERSION};
    let daemon = daemon_build().await;
    print!("{}", konedrivectl::version_text(VERSION, COMMIT, &daemon));
}

async fn daemon_build() -> konedrivectl::DaemonBuild {
    use konedrivectl::DaemonBuild;
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(error) => return DaemonBuild::NotRunning(format!("no session bus: {error}")),
    };
    let running = async {
        let bus = zbus::fdo::DBusProxy::new(&connection).await?;
        bus.name_has_owner(konedrive_dbus::SERVICE_NAME.try_into()?).await.map_err(zbus::Error::from)
    };
    match running.await {
        Ok(true) => {}
        Ok(false) => return DaemonBuild::NotRunning("not on the session bus".to_owned()),
        Err(error) => return DaemonBuild::NotRunning(format!("cannot ask the session bus: {error}")),
    }
    let read = async {
        let manager = AccountsProxy::builder(&connection)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        zbus::Result::Ok((manager.version().await?, manager.commit().await?))
    };
    match read.await {
        Ok((version, commit)) => DaemonBuild::Running { version, commit },
        Err(error) => DaemonBuild::Unknown(format!("an older build, most likely: {error}")),
    }
}
