//! A download in one stream: the file's bytes written in place, in order, and checked.
//!
//! - The version of the first answer is the one the file must end up as. A later answer for
//!   another version means the file changed in the cloud mid-download: the download starts
//!   over, once.
//! - A checkpoint is continued only for the same version, and only when that version has a
//!   quickXorHash, after its bytes are read back into the hash (`guards::resume`); anything
//!   else starts from zero. A version without a hash is never checkpointed either, and a
//!   failed download of it keeps nothing.
//! - A read error or a short stream is a break: the next fetch asks from where the bytes
//!   stopped. Three breaks and the download gives up.
//! - A hash that does not match starts the download over, once; a second mismatch is `EIO`,
//!   and drops whatever checkpoint the second download made. Nothing unverified is ever
//!   committed when the source gave a hash.
//!
//! Each read of the stream is followed by one blocking section: the bytes written at their
//! offset and, when one is due, the checkpoint.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::time::SystemTime;

use konedrive_fs::placeholder::{write_progress, Progress};
use konedrive_graph::quickxor::QuickXor;
use tokio::io::AsyncReadExt;

use super::guards::{check_answer, errno_of, open, resume, same_version, verify, Breaks, End, StartOver, Tuning, BUFFER};
use super::target::Target;
use super::{ContentSource, Version};

/// What a download produced, before any of it is committed.
pub(crate) struct Downloaded {
    pub size: u64,
    pub mtime: SystemTime,
    pub version: Option<Version>,
}

/// A download into a file nobody else can see — the replacement of a changed file: no
/// checkpoints (an `O_TMPFILE` does not survive a crash) and no size guard (there is no
/// placeholder whose size it could contradict). What to commit, or an errno.
pub(crate) async fn download_into(file: &File, item_id: &str, source: &dyn ContentSource) -> Result<Downloaded, i32> {
    // A second descriptor of the same open file: a section outlives a dropped download,
    // and the caller's descriptor is the caller's to close.
    let target = Target::new(file.try_clone().map_err(|e| errno_of(&e))?);
    let tuning = Tuning { checkpoint_every: None, ..Tuning::default() };
    download(&target, item_id, 0, source, None, &tuning).await
}

/// Streams the file's bytes into `target`, from `resume` if there is one, and checks them:
/// what to commit, or the errno to answer with (`guards::End::Fail`).
pub(super) async fn download(
    target: &Target,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    mut resume: Option<Progress>,
    tuning: &Tuning,
) -> Result<Downloaded, i32> {
    // The breaks are the download's, not an attempt's: starting over forgives none.
    let mut breaks = Breaks::new(tuning);
    let mut start_over = StartOver::default();
    loop {
        match attempt(target, item_id, original_size, source, resume.take(), tuning, &mut breaks).await {
            Ok(downloaded) => return Ok(downloaded),
            Err(end) => start_over.after(end, target, item_id, tuning).await?,
        }
    }
}

async fn attempt(
    target: &Target,
    item_id: &str,
    original_size: u64,
    source: &dyn ContentSource,
    mut checkpoint: Option<Progress>,
    tuning: &Tuning,
    breaks: &mut Breaks,
) -> Result<Downloaded, End> {
    let mut buffer = vec![0u8; BUFFER];
    let mut written = checkpoint.as_ref().map_or(0, |p| p.bytes);
    let mut last_checkpoint = written;
    let mut hasher = QuickXor::new();
    // The version the bytes on disk belong to, once the first answer says.
    let mut expected: Option<Option<Version>> = None;

    loop {
        let fetched = open(source, item_id, written, None, breaks).await?;
        check_answer(item_id, &fetched, written, original_size)?;
        let version = match &expected {
            None => {
                if let Some(progress) = checkpoint.take() {
                    hasher = resume(target, item_id, &progress, &fetched.version, fetched.size).await?;
                }
                expected.insert(fetched.version.clone()).clone()
            }
            Some(version) if !same_version(version, &fetched.version) => return Err(End::Changed),
            Some(version) => version.clone(),
        };
        let mut stream = fetched.stream;
        // A read error mid-stream is a dropped connection, and is continued like a short
        // stream, not answered `EIO` on the spot.
        let broke = loop {
            let read = match stream.read(&mut buffer).await {
                Ok(0) => break None,
                Ok(read) => read,
                Err(e) => break Some(e.to_string()),
            };
            hasher.update(&buffer[..read]);
            let at = written;
            written += read as u64;
            // The bytes first, durably, then the count that says they are there — never a
            // count ahead of the data it vouches for. Only for a version with a hash: no
            // resume would trust any other.
            let checkpoint = match (&version, tuning.checkpoint_every) {
                (Some(Version { ctag, quick_xor: Some(_) }), Some(every)) if written - last_checkpoint >= every => {
                    Some(Progress { ctag: ctag.clone(), bytes: written })
                }
                _ => None,
            };
            if checkpoint.is_some() {
                last_checkpoint = written;
            }
            buffer = target
                .alone(move |file| {
                    // Positioned: `pwrite`, never `write`. The event fd is a descriptor
                    // the *application* is about to use, and it shares its file offset
                    // with the suspended `open()`.
                    file.write_all_at(&buffer[..read], at)?;
                    if let Some(progress) = checkpoint {
                        file.sync_data()?;
                        write_progress(file, &progress)?;
                    }
                    Ok::<_, io::Error>(buffer)
                })
                .await?;
        };
        match broke {
            None if written >= fetched.size => {
                verify(item_id, &version, &hasher)?;
                // More bytes than the file has are not its content, whatever they hash to.
                if written != fetched.size && version.as_ref().is_some_and(|v| v.quick_xor.is_some()) {
                    return Err(End::Mismatch);
                }
                return Ok(Downloaded { size: fetched.size, mtime: fetched.mtime, version });
            }
            // A break, or a short stream: continue from where the bytes stopped.
            None => breaks.again(item_id, &format!("the stream ended after {written} bytes")).await?,
            Some(why) => breaks.again(item_id, &format!("the download broke after {written} bytes: {why}")).await?,
        }
    }
}
