use std::sync::Arc;
use std::time::Duration;

/// The fake's notification socket (issue #54): Engine.IO v4 over a local websocket, as
/// Graph's Socket.IO endpoint speaks it. Every connection gets the open packet, has its
/// namespace joins answered, gets a ping every [`PING_INTERVAL`], and a `notification` event
/// in the namespace `/notifications` whenever the drive changes.
pub struct FakeSockets {
    /// Bumped with every change of the drive.
    changes: tokio::sync::watch::Sender<u64>,
    /// Bumped to drop every open connection, as a network failure would.
    drops: tokio::sync::watch::Sender<u64>,
    /// While set, a connection is closed as soon as it is accepted.
    refuse: std::sync::atomic::AtomicBool,
    /// What a connection gets right after the open packet ([`Early`] as a number).
    early: std::sync::atomic::AtomicU8,
    /// Connections accepted (refused ones too), and open now.
    accepted: std::sync::atomic::AtomicUsize,
    open: std::sync::atomic::AtomicUsize,
}

/// What the fake's socket does right after the open packet, to play a service that
/// accepts a connection and drops it at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Early {
    /// Nothing: the connection goes on as usual.
    Nothing = 0,
    /// Refuses the namespace (`44/notifications,{…}`).
    Refuse = 1,
    /// Closes the websocket.
    Close = 2,
}

/// The fake's Engine.IO ping interval; its `pingTimeout` is the same.
pub const PING_INTERVAL: Duration = Duration::from_secs(25);

impl FakeSockets {
    pub(super) fn new() -> Self {
        Self {
            changes: tokio::sync::watch::channel(0).0,
            drops: tokio::sync::watch::channel(0).0,
            refuse: false.into(),
            early: 0.into(),
            accepted: 0.into(),
            open: 0.into(),
        }
    }

    pub(super) fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    /// Every open connection is dropped, without a close.
    pub fn drop_all(&self) {
        self.drops.send_modify(|n| *n += 1);
    }

    /// From now on (`true`), a connection is closed as soon as it opens; the endpoint still
    /// answers.
    pub fn refuse(&self, refuse: bool) {
        self.refuse.store(refuse, std::sync::atomic::Ordering::SeqCst);
    }

    /// From now on, what a connection gets right after the open packet.
    pub fn early(&self, early: Early) {
        self.early.store(early as u8, std::sync::atomic::Ordering::SeqCst);
    }

    /// Connections accepted so far.
    pub fn accepted(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Connections open now.
    pub fn open(&self) -> usize {
        self.open.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(super) async fn listen(self: Arc<Self>, listener: tokio::net::TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(Arc::clone(&self).serve(stream));
        }
    }

    async fn serve(self: Arc<Self>, stream: tokio::net::TcpStream) {
        use futures_util::{SinkExt, StreamExt};
        use std::sync::atomic::Ordering::SeqCst;
        use tokio_tungstenite::tungstenite::Message;
        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
        self.accepted.fetch_add(1, SeqCst);
        if self.refuse.load(SeqCst) {
            let _ = ws.close(None).await;
            return;
        }
        let (mut changes, mut drops) = (self.changes.subscribe(), self.drops.subscribe());
        changes.mark_unchanged();
        drops.mark_unchanged();
        let ms = PING_INTERVAL.as_millis();
        let open = format!(r#"0{{"sid":"fake","upgrades":[],"pingInterval":{ms},"pingTimeout":{ms},"maxPayload":1000000}}"#);
        if ws.send(Message::text(open)).await.is_err() {
            return;
        }
        match self.early.load(SeqCst) {
            1 => {
                // The client leaves: read until it has.
                let _ = ws.send(Message::text(r#"44/notifications,{"message":"not now"}"#)).await;
                while let Some(Ok(_)) = ws.next().await {}
                return;
            }
            2 => {
                let _ = ws.close(None).await;
                return;
            }
            _ => {}
        }
        self.open.fetch_add(1, SeqCst);
        let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
        loop {
            let out = tokio::select! {
                message = ws.next() => match message {
                    // A namespace joined: `40` or `40/<namespace>`.
                    Some(Ok(Message::Text(text))) if text.starts_with("40") => {
                        let namespace = text.as_str()[2..].to_owned();
                        let comma = if namespace.is_empty() { "" } else { "," };
                        Message::text(format!(r#"40{namespace}{comma}{{"sid":"s"}}"#))
                    }
                    Some(Ok(_)) => continue,
                    _ => break,
                },
                changed = changes.changed() => match changed {
                    Ok(()) => Message::text(r#"42/notifications,["notification","{\"clientState\":null}"]"#),
                    Err(_) => break,
                },
                _ = drops.changed() => break,
                _ = ping.tick() => Message::text("2"),
            };
            if ws.send(out).await.is_err() {
                break;
            }
        }
        self.open.fetch_sub(1, SeqCst);
    }
}
