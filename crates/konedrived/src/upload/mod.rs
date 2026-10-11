#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod onedrive_space;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod onedrive_steps;
#[cfg_attr(not(test), allow(dead_code, unused_imports))]
pub mod sending_holds;

/// How files are cut (provisional numbers; the tests make them small). How many
/// run at once is the account's transfer pool's (`konedrive_graph::pool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The fragment a file goes up in, a multiple of 320 KiB: a file up to
    /// this size is one fragment.
    pub chunk: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { chunk: konedrive_graph::drive::CHUNK_SIZE }
    }
}
