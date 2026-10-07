//! The two ids `config.toml` keeps for an account, as types of their own: the account's id
//! here ([`AccountId`]) and the id of its drive in OneDrive ([`DriveId`]). Both are text, and
//! one given where the other is asked for does not compile.

use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize};

/// An account's id: 12 random lowercase hex characters, none that an account present has
/// (a removed account's may come again); in object paths and file paths. One read from a
/// hand-edited file, or taken from a caller, may be anything:
/// [`is_valid`](Self::is_valid) says whether it is an id, and an account whose id is not
/// one is held (`Config::holds`) and has no files (`Paths::account`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    /// `text` as an id, unchecked.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// A fresh id: 12 random lowercase hex characters (48 bits), none of `taken`.
    pub fn fresh<'a>(taken: impl IntoIterator<Item = &'a AccountId> + Clone) -> Self {
        loop {
            let mut bytes = [0u8; 6];
            getrandom::getrandom(&mut bytes).expect("the OS random number generator failed");
            let id = Self(bytes.iter().map(|b| format!("{b:02x}")).collect());
            if !taken.clone().into_iter().any(|t| *t == id) {
                return id;
            }
        }
    }

    /// Whether it is an account id: 12 lowercase hex characters.
    pub fn is_valid(&self) -> bool {
        is_account_id(&self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `text` has the form of an account id: 12 lowercase hex characters.
pub fn is_account_id(text: &str) -> bool {
    text.len() == 12 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The id of a drive in OneDrive, as Graph gives it: an account's identity. Never empty: an
/// account that has no drive yet has none (`Option<DriveId>`).
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct DriveId(String);

impl DriveId {
    /// `text` as a drive id; `None` when it is empty.
    pub fn new(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        (!text.is_empty()).then_some(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// `drive_id` of an account as the file has it: a missing key and an empty text are no drive.
pub(super) fn drive_or_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<DriveId>, D::Error> {
    Ok(DriveId::new(String::deserialize(deserializer)?))
}

/// `write_test_drive_ids` as the file has it; an empty entry names no drive and is left out.
pub(super) fn drives<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<DriveId>, D::Error> {
    Ok(Vec::<String>::deserialize(deserializer)?.into_iter().filter_map(DriveId::new).collect())
}

macro_rules! text_id {
    ($id:ty) => {
        /// As the text it is, quoted: what `{:?}` printed while the id was a `String`, which
        /// `Accounts.LastError` and the log say.
        impl fmt::Debug for $id {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.0, f)
            }
        }

        impl fmt::Display for $id {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Deref for $id {
            type Target = str;

            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl PartialEq<str> for $id {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $id {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $id {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }
    };
}

text_id!(AccountId);
text_id!(DriveId);
