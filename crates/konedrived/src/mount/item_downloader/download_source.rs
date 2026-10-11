//! The source one download fetches through: it lets through only answers that are of the
//! content the download was asked for, tells how far the file is written as its one
//! stream is read, for the reads that come while the file is downloaded, and keeps what
//! the source refused, typed.
//!
//! **State owned:** for one download: the refusals of the source that no later answer for
//! the same range took back, and whether an answer was of another content. The download
//! itself (`content_source::source::download_staged`) answers an errno, which cannot say a
//! sign-out from a throttle; whoever ran it asks here what the source said.
//!
//! **Answers for:** no byte of another content reaching the download's file, and so no
//! reader of it. The download itself takes the version of its FIRST answer as the one to
//! hold the rest against (`content_source::source::download`), whatever was asked for. Here
//! every answer — the first, one after a break, each part of a download in parts — is
//! held against the asked content ([`decide_answer_use`]) before its stream is handed on
//! and before the mark that reads go by is touched: an answer of another content is
//! answered to the download as a file OneDrive does not have, which ends it at once with
//! nothing written and nothing more fetched. That error is only what ends the download
//! (`content_source::source` has no other word that does); what the answer WAS is kept here,
//! typed: `DownloadRefusal::AnotherContent`, where the source's own "no such item" is
//! `DownloadRefusal::ItemGone` ([`decide_refusal`]).
//!
//! **An answer that is let through voids what was remembered of the key.** That OneDrive
//! stated another content in its answer to the download before this one stays remembered
//! while this one asks; the first answer accepted here is OneDrive's word since, and the
//! download states are told at that answer (`ItemDownloadStates::forget_another_content`),
//! not when the download is started and not when it ends.
//!
//! **A size that was not told** (`Fetched::size` is `None`) disagrees with nothing: the
//! answer is held against the asked content by its version alone, and with no version
//! either it states no other content. The download then refuses it, having no size to go
//! by (`guards::check_answer`): a failure of that one asking. A size of 0 that was told
//! is a size: against a content that has bytes it is another content, with or without a
//! version — the file was emptied in OneDrive.
//!
//! **The mark is taken back to 0** the moment the download does not trust what is on
//! disk any more — a hash that does not match, a version that changed between two
//! answers, a checkpoint it cannot go on from (`ContentSource::reject_written`, told by
//! `guards::StartOver`) — and not at the answer to its next fetch.
//!
//! **What is not proven here:** a content the row tells by neither hash nor cTag, and a
//! source that says no version, are told by the size alone; and the bytes below the mark
//! are not yet held against the hash — the download does that at its end.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use konedrive_tree::ContentVersion;

use super::{decide_is_asked_content, DownloadRefusal};
use crate::mount::item_download_states::{DownloadRun, ItemDownloadStates};
use crate::content_source::source::{ContentSource, Fetched, SourceError, Version};

/// What `error` of the content source is, by what the plan's task does about it. Pure.
/// "No such item" states no content: it is not `DownloadRefusal::AnotherContent`.
pub(super) fn decide_refusal(error: &SourceError) -> DownloadRefusal {
    match error {
        SourceError::SignedOut(_) => DownloadRefusal::SignedOut,
        SourceError::Throttled { retry_after, .. } => DownloadRefusal::Throttled { retry_after: *retry_after },
        SourceError::NotFound(_) => DownloadRefusal::ItemGone,
        SourceError::Transient(detail) => DownloadRefusal::Transient { detail: detail.clone() },
    }
}

/// What a download does with one answer of the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnswerUse {
    /// Of the asked content, the one stream of the file: its bytes are written in order,
    /// and what is on disk counts from `from` — also when the download starts over.
    WrittenInOrder { from: u64 },
    /// Of the asked content, and telling nothing of how far the file is written: one part
    /// of a download in parts, written out of order; or a stream that starts elsewhere
    /// than it was asked for, which the download refuses (`guards::check_answer`).
    NotCounted,
    /// Of another content than the one asked for: nothing of it is written or counted.
    AnotherContent,
}

/// What the download of `asked` does with an answer that says the item is `size` bytes
/// long (`None`: it tells no size) and of `version`, and serves bytes from `served_from`,
/// to a fetch from `asked_from` up to `end` (`None`: the rest of the file). Pure; the
/// content is told by [`decide_is_asked_content`].
pub(super) fn decide_answer_use(asked: &ContentVersion, size: Option<u64>, version: Option<&Version>, served_from: u64, asked_from: u64, end: Option<u64>) -> AnswerUse {
    match (decide_is_asked_content(asked, size, version), end) {
        (false, _) => AnswerUse::AnotherContent,
        (true, None) if served_from == asked_from => AnswerUse::WrittenInOrder { from: served_from },
        (true, None | Some(_)) => AnswerUse::NotCounted,
    }
}

/// Which of `refusals` a download that failed is answered by: what was stated of the
/// content before what ends every download — an item OneDrive does not have, a
/// sign-out — before what holds them back, before what passes. Pure.
pub(super) fn find_gravest_refusal(refusals: impl IntoIterator<Item = DownloadRefusal>) -> Option<DownloadRefusal> {
    refusals.into_iter().min_by_key(|refusal| match refusal {
        DownloadRefusal::AnotherContent => 0,
        DownloadRefusal::ItemGone => 1,
        DownloadRefusal::SignedOut => 2,
        DownloadRefusal::Throttled { .. } => 3,
        DownloadRefusal::Transient { .. } => 4,
    })
}

/// The source of one download.
pub(super) struct DownloadSource<'a> {
    inner: &'a dyn ContentSource,
    /// The content the download was asked for.
    asked: ContentVersion,
    /// How far the file is written in order. A download in parts (`end` given) writes out
    /// of order and tells nothing.
    written_up_to: Arc<AtomicU64>,
    /// The download this is the source of, and the states that are told when OneDrive
    /// answered it with the asked content.
    download_states: &'a ItemDownloadStates,
    run: &'a DownloadRun,
    /// What the source last refused, by the end of the range that was asked for: a
    /// download in parts fetches several at once, and an answer for one part says nothing
    /// of the refusal of another.
    refusals: std::sync::Mutex<HashMap<Option<u64>, DownloadRefusal>>,
}

impl<'a> DownloadSource<'a> {
    /// The source of the download `run` of `asked`, which fetches through `inner` and
    /// tells `written_up_to` and `download_states`.
    pub(super) fn new(inner: &'a dyn ContentSource, asked: ContentVersion, written_up_to: Arc<AtomicU64>, download_states: &'a ItemDownloadStates, run: &'a DownloadRun) -> Self {
        DownloadSource { inner, asked, written_up_to, download_states, run, refusals: std::sync::Mutex::default() }
    }

    /// What the source refused and no later answer for the same range took back, the
    /// gravest of it ([`find_gravest_refusal`]); `None` when every range that was asked
    /// for was answered last, and when nothing was fetched. An answer of another content
    /// than the asked one is kept as `AnotherContent`.
    pub(super) fn find_refusal(&self) -> Option<DownloadRefusal> {
        find_gravest_refusal(crate::panic::lock(&self.refusals).values().cloned())
    }
}

/// The one stream of a download, counting what was read from it.
struct CountedStream {
    inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    written_up_to: Arc<AtomicU64>,
}

impl tokio::io::AsyncRead for CountedStream {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            self.written_up_to.fetch_add((buf.filled().len() - before) as u64, Ordering::SeqCst);
        }
        polled
    }
}

#[async_trait::async_trait]
impl ContentSource for DownloadSource<'_> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let mut fetched = match self.inner.fetch(item_id, from, end).await {
            Ok(fetched) => fetched,
            Err(error) => {
                crate::panic::lock(&self.refusals).insert(end, decide_refusal(&error));
                return Err(error);
            }
        };
        // Before the stream is handed on and before the mark is touched.
        match decide_answer_use(&self.asked, fetched.size, fetched.version.as_ref(), fetched.served_from, from, end) {
            AnswerUse::AnotherContent => {
                crate::panic::lock(&self.refusals).insert(end, DownloadRefusal::AnotherContent);
                // Answered as a file OneDrive does not have: the download ends at once.
                // What it was is kept above, typed; nothing reads this error's text.
                return Err(SourceError::NotFound(format!("{item_id} is of another content in OneDrive than the one that was asked for")));
            }
            AnswerUse::WrittenInOrder { from } => {
                self.written_up_to.store(from, Ordering::SeqCst);
                fetched.stream = Box::new(CountedStream { inner: fetched.stream, written_up_to: Arc::clone(&self.written_up_to) });
            }
            AnswerUse::NotCounted => {}
        }
        crate::panic::lock(&self.refusals).remove(&end);
        // OneDrive has answered, with the asked content: what its answer to the download
        // before this one stated is void from here on.
        self.download_states.forget_another_content(self.run);
        Ok(fetched)
    }

    fn progress(&self, done: u64, size: u64) {
        self.inner.progress(done, size);
    }

    fn reject_written(&self) {
        // From this moment, not from the answer to the next fetch: a read is answered
        // nothing of what the download does not trust.
        self.written_up_to.store(0, Ordering::SeqCst);
        self.inner.reject_written();
    }
}
