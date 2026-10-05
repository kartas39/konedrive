//! The accounts of one daemon (design §2, §4): the manager at `/org/konedrive/Accounts` —
//! `org.konedrive.Accounts`, `org.konedrive.Files` and the `ObjectManager` — and, for each
//! account, its `Account`, its folder's interfaces (`Folder`, `Transfers`, `UploadQueue`,
//! `Conflicts`, `LocalScan`, `ActivityLog`) and `TokenExport` at `/org/konedrive/Accounts/<id>`.
//!
//! [`startup::start`] is the daemon's startup, in the order of design §2.2: `config.toml` loaded and
//! migrated, the files of a migrated account moved, every account brought up to where no
//! helper is needed, every object exported, and only then the bus name claimed.

pub mod manager;
pub mod startup;
pub mod stop;

