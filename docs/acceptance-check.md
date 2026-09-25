# Acceptance check

A manual check of a build on a real machine, against a real OneDrive account, covering what the
automated suites cannot: the helper under systemd, the real Graph service, Dolphin, and Plasma's
tray and notifications. Anyone testing a build can follow it; it takes about half an hour. Keep a
short run log: the build's commit, the kernel and filesystem, and, for each step, whether it
matched "Expected" and anything that did not.

What you need: a Fedora (or similar) machine with KDE Plasma 6, the build dependencies from the
README, `sudo`, and a Microsoft account you can sign in with (a test account is fine). konedrive
signs in with its own application registration, so there is nothing to register beforehand.

Everything here is read-only against OneDrive: nothing is uploaded, renamed or deleted in the
cloud by konedrive. The one step that touches a local file's permissions (step 8, the conflict)
still writes nothing back to Microsoft — it only shows that the daemon rescues a local change
rather than losing it. Steps 6 and 8 ask you to change a file on onedrive.com yourself; use a file
you do not mind renaming or editing.

A OneDrive folder is kept in step only while the helper is connected, so the helper comes first
(step 2). The no-interception mode is a developer's mode and is not part of this check (README,
"A folder without OneDrive or the helper").

## 1. Install this build (as yourself), and sign in
    scripts/dev-install.sh
    konedrivectl login                     # with no account yet, adds one called Personal first
Or open KOneDrive and choose **Sign in…** (at the top of the sidebar, or on the Status page
while there is no account): it opens the account picker in the browser straight away — the
account is created, named after its email, only once the sign-in succeeds.
Expected: the window opens from the launcher (or `konedrive` on the command line); after the
browser sign-in, `konedrivectl status` shows you signed in, with your name and quota, and
`konedrivectl account list` shows the one account, `signed-in`, `read-only`, with no folder yet.
The account switcher at the top of the window's sidebar shows its name and email. The tray icon
appears; hovering it repeats the window's status line.

If this machine ran a version from before multiple accounts, with a folder registered: after the
upgrade the window shows one account, "Personal", with the same folder, still signed in; its
Places entry is now called "OneDrive — Personal"; and `~/.config/konedrive/config.toml.v1` holds
the old configuration. Skip `login` then.

`dev-install.sh` does not install the Dolphin plugins. For the emblems in steps 3 and 5, install
them for your user as the README's "Dolphin integration" says, then log out and back in.

## 2. The helper, then a scratch folder (needs sudo)
    cargo build --release -p konedrive-helper
    sudo scripts/install-helper.sh
    systemctl status konedrive-helper      # active (running)
    konedrivectl sync status               # repeat until: Helper: connected
Expected: the helper is `active (running)`. No automated test runs its hardened unit under
systemd (limitations log W16), so this step is where the unit is checked: if it is `failed`, keep
the output of `journalctl -u konedrive-helper` in the run log. The daemon connects to the helper within half a minute; until then `Helper:` is not
`connected`.

    mkdir ~/OneDrive-test
    konedrivectl sync register ~/OneDrive-test
    konedrivectl sync status        # repeat: State: listing, then ready
Or, in the window, **Choose Folder…** on the **Account** page.
Expected: `sync register` succeeds. If it is refused `NoHelper`, the daemon has not connected to
the helper yet: wait for `Helper: connected` and run it again.

The folder fills with your OneDrive's folders and files, all read-only, as the listing runs — items
appear a page at a time rather than all at once at the end. `Items:` reads "N in OneDrive, M in the
folder". Once listing finishes, `find ~/OneDrive-test | wc -l` is M + 1 — the in-folder number,
plus the folder itself; N can be larger than M (limitations log F32). The window's **Status** page
(left sidebar) shows the same folder, phase and count, with "Refresh Now" and "Open in File
Manager", and no card about the helper.

    konedrivectl sync skipped
Expected: the Personal Vault, any shared folder added with "Add to my OneDrive", any OneNote
notebook, and anything with a name too long — each with why. The window's **Not in the Folder**
page lists the same.

## 3. Download, verify, free up
    konedrivectl sync hydrate ~/OneDrive-test/<a photo>
    konedrivectl sync state ~/OneDrive-test/<a photo>     # hydrated
Open it in an image viewer: it is the real photo, and opening it a second time is instant (no
further download — `konedrivectl sync transfers` shows nothing running for it). In Dolphin, with
the plugins installed (step 1), the cloud and check emblems follow.

    konedrivectl sync dehydrate ~/OneDrive-test/<a photo>
Expected: `online-only`, and `du -h ~/OneDrive-test/<a photo>` shows it takes no space again.

## 4. Opening a file downloads it; a second open is instant
Pick a file you have not touched yet.
    konedrivectl sync state ~/OneDrive-test/<a document>   # online-only
Open it in whatever normally opens it (LibreOffice, an image viewer, a text editor — anything).
Expected: the open itself pauses while the file downloads (the helper, step 2), then the real
content; `sync state` now reads `hydrated`. Close it and open it again: no pause, no new entry in
`sync transfers` — the content is already on disk and verified.

## 5. Thumbnails without a download
Open the scratch folder in Dolphin, switch to icon view, and scroll past a folder of photos you
have not opened.
Expected: real thumbnails appear for the images. `konedrivectl sync state` on one of those photos
still reads `online-only` and `du -h` still shows no space used — the thumbnail came from
OneDrive's own cache, not from downloading the file.

## 6. A change on the web arrives
Rename a file on onedrive.com (or edit one you have already downloaded). Within about a minute —
or at once with `konedrivectl sync refresh` — it has the new name here, with its content, if it
was downloaded, untouched until you open it again.

## 7. Activity, and the window's other pages
    konedrivectl sync activity --limit 20
    konedrivectl sync transfers
Expected: the rename/edit from step 6 as one entry (downloaded, updated, renamed, or moved,
depending what you did), and step 3's download and free-up as earlier entries. The window's
**Activity** page shows the same two lists ("Downloading now" and "Recent"); clicking a "Recent"
entry shows the file in Dolphin. The **Account** page shows the account's name with "Rename…",
the line "Read-only" with no switch, you signed in with your quota, the folder with "Forget
Folder", and "Remove Account…"; the **Settings** page shows "Start at login", "Show download
progress" and "Show in Places". Dolphin's Places panel has an entry for the folder, named
`OneDrive — <account name>`.

With "Show download progress" on (the default), open a large file that is not downloaded yet:
after about 2 s, Plasma's notifications show "Downloading from OneDrive" with the file's name and
its progress, and the entry ends when the download does. Switched off, nothing shows.

## 8. A conflict (lifts the read-only lock on one file — nothing is written to OneDrive)
The folder is read-only, so making a genuine local edit needs lifting the permission bits first.
You own the files (the lock is `chmod`, not ownership), so this needs no `sudo`:
    konedrivectl sync hydrate ~/OneDrive-test/<a small text file>
    chmod u+w ~/OneDrive-test/<that file>
    echo "local edit, never uploaded" >> ~/OneDrive-test/<that file>
    chmod u-w ~/OneDrive-test/<that file>
Now change that same file in OneDrive's web UI (or rename it) and either wait about a minute or run
`konedrivectl sync refresh`.
Expected: your local edit is moved to `$XDG_DATA_HOME/konedrive/rescued/<account id>/<timestamp>/…`
(the id is the one `konedrivectl account list` shows) rather than lost, and:
    konedrivectl sync conflicts
lists it (original path, rescued path, when). The window's **Conflicts** page shows the same, with
"Show in Folder" and "Dismiss". A KDE notification appears ("your changed version was moved to
…"), and the tray icon switches to its "needs attention" state (a warning triangle) until you
dismiss it:
    konedrivectl sync dismiss <rescued path>
If you would rather skip this step, nothing else in the checklist depends on it — note in the run
log that it was skipped and why.

## 9. Free Up Space
In the window's **Status** page, click "Free Up Space…" and confirm. Or from the command line:
    konedrivectl sync free-up-space
Expected: every downloaded file that is not open right now goes back to `online-only`; the report
(files freed, bytes freed, files skipped as busy) matches what "On this computer" showed before and
after. A file you have open in another program is skipped and counted busy, not freed.

## 10. Pinning: Always keep on this device
In Dolphin, right-click a folder inside the scratch folder that has at least one file not yet
downloaded, and choose **Always keep on this device**.
Expected: it downloads everything under it (`konedrivectl sync status` shows "Always on this
device: 1"); the folder and every file under it get the filled check emblem; the window's Status
page shows "Always on this device: 1 item".

Right-click the same folder again and uncheck **Always keep on this device**.
Expected: the files stay downloaded — the emblem changes to the outline check, nothing is freed,
and "Always on this device" is gone from the Status page. Now choose **Free up space** on the
folder: the files go back to `online-only`.

## 11. The Baloo exclusion
    balooctl6 config list excludeFolders
    grep 'exclude folders' ~/.config/baloofilerc
Expected: the scratch folder's path is listed (by the second command at least: `balooctl6 config
list` has been seen printing an empty list while the settings file held exclusions) — Baloo was told not to index it when you registered
it (opening every file to index it would download the whole drive). If you had excluded some other
folder yourself before this run, it is still listed too; registering or forgetting the OneDrive
folder never touches an exclusion you did not add.

## 12. Without the helper: what you are told (needs sudo)
Close any program that has a file in the folder open first: stopping the helper hands a file that
is still downloading to its program empty (limitations log Z1). Do not open files in the folder
while the helper is stopped: nothing intercepts the open, and a file that is not downloaded reads
as zeros.
    sudo systemctl stop konedrive-helper
    konedrivectl sync status
Expected: `Helper: stopped`, with the instruction `sudo systemctl start konedrive-helper`, and
`State: error`: the folder is not kept in step (a change on the web does not arrive). The window
shows the same instruction on a card.

    sudo systemctl start konedrive-helper
    konedrivectl sync status        # repeat
Expected: within half a minute, `Helper: connected` and `State: ready`; the card is gone.

## 13. Tray states and a notification
With the helper installed (step 2), watch the tray icon while a cycle runs:
- synced ("state-ok", a checkmark) once a cycle finishes with nothing to do;
- syncing ("state-sync") while `sync status` reads `listing` or a download is under way;
- needs attention ("state-warning") during step 8's conflict, or if you sign out below;
- signed out ("state-offline") after `konedrivectl logout` (sign back in afterwards with
  `konedrivectl login` — this only affects your own session, nothing in the cloud).
A KDE notification fires when you sign out ("needs signing in again"); System Settings → KOneDrive
lists every notification event `konedrive.notifyrc` defines, and you can check the box there
instead of forcing one, if you prefer.

## 14. A second account (optional: needs a second Microsoft account)
Everything above used one account. With a second Microsoft account (a test account is fine):
    mkdir ~/OneDrive-test-2
    konedrivectl account add Second
    konedrivectl --account Second login
    konedrivectl --account Second sync register ~/OneDrive-test/<a folder>   # refused
    konedrivectl --account Second sync register ~/OneDrive-test-2
Or, in the window, **Sign in…** from the switcher's menu: once you are signed in, the folder
picker opens for the new account at once.
Expected: the first `sync register` is refused, naming the first account — a folder inside another
account's folder cannot be registered; the second succeeds. Then:
- `konedrivectl account list` shows both accounts, each with its own folder; `konedrivectl sync
  status` shows both folders, each under its account's name, with one `Helper:` line above them.
  Without `--account`, a command such as `konedrivectl sync refresh` now stops and lists the two
  names.
- The window's switcher lists both; choosing one switches every page but Settings to it, and each
  page's title names the account. The tray's tooltip has a line per account; its icon shows the
  worse of the two states. Places has "OneDrive — Second" beside the first account's entry.
- In Dolphin, a file in either folder downloads when opened, and "Always keep on this device" and
  "Free up space" work in both folders (the right account's `sync activity` records each).
  `konedrivectl sync state <a file in either folder>` works without `--account`.
- A Microsoft account can be connected once: add a third account (`konedrivectl account add
  Third`), and sign it in (`konedrivectl --account Third login`) with the first account's
  Microsoft account. It stays signed out, and `konedrivectl --account Third status` (or its
  Account page) says that Microsoft account is already connected, naming the first account.
  Remove it again with `konedrivectl account remove Third`.

Then remove the second account: **Remove Account…** on its Account page, or
    konedrivectl account remove Second
Expected: it is gone from `account list`, from the switcher and from Places; its folder is still
there, unlocked, with the files it had; the first account is untouched.

## Undo
    konedrivectl sync forget                     # the lock comes off; the files stay
    rm -r ~/OneDrive-test ~/OneDrive-test-2
    sudo scripts/install-helper.sh --uninstall    # refuses while a folder is registered: forget first
    balooctl6 config rm excludeFolders ~/OneDrive-test   # only if Forget did not already remove it
