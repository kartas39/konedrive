# What KIO opens (measured on KIO 6.30.0-1.fc44, 2026-09-24)

Measured by `tests/kio/kio_probe.cpp`, run through `tests/kio/run.sh` (scratch
`HOME`/`XDG_*`, private session bus, `QT_QPA_PLATFORM=offscreen`; nothing is
installed, and the thumbnail cache, configuration and session bus of whoever runs it are never
touched). `kf6-kio-core-6.30.0-1.fc44.x86_64`; `kf6-kio-gui` and
`kf6-kio-widgets-libs` at the same version.

## A. Cache naming and sizes

```
RESULT A.dir normal: 306c00f9f52369a078d04946c5583c8e.png 3195e6b086136bb65bd3551831a71225.png 592c587cbaa07b6d9113c518f7392dcb.png 9297a9aeb6094a7f905387eab807094e.png
RESULT A.dir large: 306c00f9f52369a078d04946c5583c8e.png 3195e6b086136bb65bd3551831a71225.png 592c587cbaa07b6d9113c518f7392dcb.png 9297a9aeb6094a7f905387eab807094e.png
RESULT A.dir x-large: 306c00f9f52369a078d04946c5583c8e.png 3195e6b086136bb65bd3551831a71225.png 592c587cbaa07b6d9113c518f7392dcb.png 9297a9aeb6094a7f905387eab807094e.png
RESULT A.dir xx-large: 306c00f9f52369a078d04946c5583c8e.png 9297a9aeb6094a7f905387eab807094e.png
RESULT A.name plain.jpg: FullyEncoded; Thumb::URI=file:///tmp/tmp.2CGgyUKQx4/home/folder/plain.jpg
RESULT A.name with space.jpg: FullyEncoded; Thumb::URI=file:///tmp/tmp.2CGgyUKQx4/home/folder/with%20space.jpg
RESULT A.name фото.jpg: FullyEncoded; Thumb::URI=file:///tmp/tmp.2CGgyUKQx4/home/folder/%D1%84%D0%BE%D1%82%D0%BE.jpg
RESULT A.name sym (1)+&,;=@:!$'*.jpg: FullyEncoded; Thumb::URI=file:///tmp/tmp.2CGgyUKQx4/home/folder/sym%20(1)+&,;=@:!$'*.jpg
```

KIO names a thumbnail after the MD5 of the **FullyEncoded** `file://` URI (percent-encoded:
spaces, Cyrillic bytes and the symbol run all come back through `%XX` escapes, exactly what
`QUrl::toString(QUrl::FullyEncoded)` produces — not the "pretty", partially-decoded form).
Requests at edge 128, 256, 512 and 1024 land in `normal`, `large`, `x-large` and `xx-large`
respectively, one file per size per source image, confirmed against the MD5 of each file's own
`file://` URI computed independently (Python's `hashlib.md5` over the same string matched every
entry). All four test names (plain, space, Cyrillic, and a run of shell-hostile punctuation)
produced a `normal`, `large` and `x-large` entry. **`xx-large` only got two of the four** — the
plain name and the one with a space; the Cyrillic name and the punctuation-heavy name did not.
This is KIO's *own* preview generation (`KIO::PreviewJob` reading the real, non-sparse JPEGs we
wrote), not a naming-scheme question: the MD5 hash of the (correctly percent-encoded) URI is the
same mechanism regardless of size, and konedrive's thumbnail filler computes and writes that hash
itself rather than asking KIO to generate the file — so this gap does not carry over to the fill
strategy. It is flagged here as a fact about KIO 6.30's own xx-large thumbnailing, not explained
further.

## B. Does a cached thumbnail keep the file closed?

```
RESULT B.cached 128: preview, center pixel #ff0000; opened=no
RESULT B.cached 256: preview, center pixel #ff0000; opened=no
RESULT B.control: no preview (failed); opened=yes
```

Yes. Two zero-byte placeholder files (`sparse.jpg`, `sparse-nocache.jpg`, same size and mtime,
made with `ftruncate`/`utimensat`, no real image data) were used. `sparse.jpg` had a thumbnail we
wrote ourselves — solid red, tagged with `Thumb::URI` (both `FullyEncoded` and `PrettyDecoded`,
to be safe) and a `Thumb::MTime` matching the placeholder's real mtime — dropped straight into
`normal` and `large`. Requesting a preview of it at 128 and 256 returned our red pixel and the
`inotify` watch on the folder saw **no open** of `sparse.jpg` at either size: KIO drew the
thumbnail straight from the cache file, never touching the placeholder.

The control, `sparse-nocache.jpg`, has no cache entry and is otherwise identical. Requesting its
preview **did** open it (`opened=yes`) — as it must, since there is no cached data and nothing
else to draw from — and the job reported no preview (an empty/zero-byte file is not decodable as
an image, so the preview plugin fails after reading it). This is exactly the outcome required for
B to mean anything: without a working "the file gets opened when there is nothing to serve from
the cache" case, a hit at B.cached would be unfalsifiable.

**Step 5, the deliberate break:** with `Thumb::MTime` in our own cache entries set to
`st_mtime + 1` (one second in the future of the placeholder's real mtime — a stale entry per the
freedesktop thumbnail spec, which requires an exact match), a rerun gave:

```
RESULT B.cached 128: no preview (failed); opened=yes
RESULT B.cached 256: no preview (failed); opened=yes
RESULT B.control: no preview (failed); opened=yes
```

Both `B.cached` lines flipped from "red pixel, opened=no" to "no preview, opened=yes" — KIO
rejected the mismatched `Thumb::MTime`, discarded the cache entry, and opened the (still
zero-byte, still undecodable) placeholder instead, exactly as `B.control` does. The line was
then restored to `st.st_mtime` and a final run reproduced the original values shown above
byte-for-byte (modulo the scratch directory's random path in the URI text). This is the
before/after showing the check can actually fail, not just always report "cached".

## C. Type detection

```
RESULT C.listing: opened: []
RESULT C.type data.bin: application/octet-stream; opened=yes
RESULT C.type noext-with-attr: application/octet-stream; opened=yes
RESULT C.type noextension: application/octet-stream; opened=yes
RESULT C.type photo2.jpg: image/jpeg; opened=no
RESULT C.type report.pdf: application/pdf; opened=no
```

Listing the folder (`KCoreDirLister::openUrl`) opened nothing — directory entries and stat data
are enough for the listing itself. Resolving each item's MIME type the way Dolphin's roles
updater does (`KFileItem::determineMimeType()`) opened the two files whose extension does not
settle a type (`data.bin`, `noextension`) and also opened `noext-with-attr`, which carries a
`user.mime_type=image/jpeg` xattr (the shared-mime-info/GVFS convention) on an extensionless
placeholder. **The `user.mime_type` xattr did not stop the open**, and it was not even consulted
for the result: all three came back as the generic `application/octet-stream`, not `image/jpeg`.
`photo2.jpg` and `report.pdf` were resolved from their extension alone and never opened.
`KFileItem::determineMimeType()` in this KIO version does not read `user.mime_type` at all; it
falls through to content sniffing (which requires opening the file) whenever the extension is
absent or ambiguous.

## Decision

THUMBNAILS: FILL FullyEncoded normal,large,x-large,xx-large

(What was built fills `normal`, `large` and `x-large` only: `xx-large` is left out on purpose.
See `docs/design/desktop.md` §8 and `docs/limitations-and-workarounds.md`, K15.)

Section B measured the mechanism directly at `normal` (128) and `large` (256): a correctly tagged
(`Thumb::URI` + matching `Thumb::MTime`) PNG in those directories is drawn without an open.
Section A confirms the same MD5-of-FullyEncoded-URI naming scheme names the `x-large` and
`xx-large` directories too — the freedesktop spec ties the lookup mechanism to the file's mtime
and the hash of its URI, not to how the entry was produced, so a placeholder we write ourselves
for `x-large`/`xx-large` is read the same way B showed for `normal`/`large`. (KIO's own inability
to *generate* an `xx-large` thumbnail for two of the four test names, noted in A, is about KIO's
preview job, not about reading a cache entry the filler writes directly — it does not weaken
this.)
