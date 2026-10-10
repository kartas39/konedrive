//! The flag `is_name_ignored` of the rows gone over once, when the list of ignored names
//! was changed.
//!
//! **State owned:** none of its own; it rewrites `is_name_ignored` and, through the
//! statements of `item_row` that keep it, `is_below_ignored_directory`, of rows that stay
//! where they are. Every other writer of the flag writes it with a place (design §3).
//!
//! **Answers for:** design §3, "when the list of ignored names is changed, the rows are
//! gone over once".
//!
//! **Tested without the rest:**
// test: an_unbound_item_whose_name_the_new_list_ignores_is_left_here_with_what_is_below_it
// test: an_unbound_item_whose_name_the_new_list_no_longer_ignores_is_unsent_again
// test: an_item_onedrive_has_keeps_being_sent_whatever_the_list_says_of_its_name
// test: the_flag_below_an_ignored_directory_equals_the_walk_up_after_the_go_over
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;

use super::ItemStore;
use crate::TreeError;

impl ItemStore<'_> {
    /// Asks `is_name_ignored` of the local name of every item the folder shows and writes
    /// the flag where the answer is another than the row's; how many rows changed. One
    /// transaction, however many rows: a plan made meanwhile sees the old list or the new.
    ///
    /// Called by the change of the account's ignore list (`sync/settings.rs`), under the
    /// tree lock, so that no write operation gives a name its flag by the old list after
    /// it; the plan's task is woken after it. `is_name_ignored` is the upload rules' pure
    /// question of a name (`fuse::UploadRules::is_name_ignored`). MISSING-4.f.
    pub fn rewrite_ignored_names(&mut self, is_name_ignored: &dyn Fn(&OsStr) -> bool) -> Result<u64, TreeError> {
        todo!()
    }
}
