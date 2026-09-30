//! Graph's change notifications (issue #54): the drive's Socket.IO endpoint is opened with
//! konedrive's own client (`konedrived::drive::socket`), one small file is written in the run
//! folder, and the check waits for the `notification` event the daemon's live task relies on.
//! It reports how long the event took, the Engine.IO timings the service sent, and whether the
//! endpoint said when it expires (limitations log: without it, the daemon renews after an
//! hour). The endpoint is a `GET` through the guard; the socket itself only reads.

use std::time::{Duration, Instant, SystemTime};

use konedrived::drive::socket::NotificationSocket;

use super::{show, Outcome, Run, T0};
use Outcome::{Fail, Pass};

/// How long the event is waited for after the write.
const WAIT: Duration = Duration::from_secs(60);

impl Run {
    /// The notification check alone (`--only notifications`).
    pub async fn notifications(&mut self) -> Vec<(&'static str, Outcome)> {
        let mut done = Vec::new();
        if self.guard.refused().is_none() {
            let outcome = self.notification_of_a_write().await;
            show(NOTIFICATION, &outcome);
            done.push((NOTIFICATION, outcome));
        }
        done
    }

    /// A write in the drive is followed by a `notification` on its socket.
    pub(super) async fn notification_of_a_write(&mut self) -> Outcome {
        let endpoint = step!("the notification endpoint", self.drive.socket_endpoint().await);
        let expiry = match endpoint.expiry_from_service {
            true => match endpoint.expires_at.duration_since(SystemTime::now()) {
                Ok(left) => format!("expirationDateTime came, {} min ahead", left.as_secs() / 60),
                Err(_) => "expirationDateTime came, already past".into(),
            },
            false => "no expirationDateTime (the daemon renews after an hour)".into(),
        };
        let mut socket = step!(format!("opening the socket at {}", endpoint.host()), NotificationSocket::connect(&endpoint.notification_url).await);
        let timings = format!("pingInterval {:?}, pingTimeout {:?}", socket.ping_interval(), socket.ping_timeout());
        let written = Instant::now();
        step!("writing notification.txt", self.put("notification.txt", b"notification".to_vec(), T0).await);
        let outcome = match tokio::time::timeout(WAIT, socket.notification()).await {
            Ok(Ok(())) => Pass(format!(
                "a notification {:.1} s after the write finished; {timings}; {expiry}; host {}",
                written.elapsed().as_secs_f64(),
                endpoint.host()
            )),
            Ok(Err(end)) => Fail(format!("the socket ended before any notification: {end}; {timings}; {expiry}")),
            Err(_) => Fail(format!("no notification within {} s of the write; {timings}; {expiry}", WAIT.as_secs())),
        };
        socket.close().await;
        outcome
    }
}

pub const NOTIFICATION: &str = "a write in the drive sends a notification on its socket";
