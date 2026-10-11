//! The steps towards the folder against a program's writes, renames and removals, a
//! listing's pages and deaths, in orders made from a seed: bounded, and the same at every
//! run. Everything is a job of the one write-operation thread, so what races is what a
//! step was planned from against what the rows say when it runs, and what a death leaves
//! between a step's disk and its commit.

use std::collections::BTreeMap;
use std::sync::Arc;

use konedrive_tree::{ContentPresence, FolderStep, Item, ItemKind, ItemPlace, ItemVersions, LocalItemId, LocalVersionChange};

use super::super::{FolderSteps, StepOutcome};
use super::{count_records, make_steps, read_disk, run, run_in_halves};
use crate::mount::item_fixture::ItemFixture;

/// Numbers made from a seed: the same at every run.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, count: usize) -> usize {
        (self.next() % count as u64) as usize
    }

    fn pick<'a, T>(&mut self, of: &'a [T]) -> Option<&'a T> {
        (!of.is_empty()).then(|| &of[self.below(of.len())])
    }
}

const DIRECTORIES: [&str; 3] = ["", "d1", "d2"];

/// One folder under the race, with what the test knows of it beside the rows.
struct Race {
    fixture: ItemFixture,
    steps: Arc<FolderSteps>,
    numbers: Numbers,
    /// The paths OneDrive has files at that no listing moved or removed yet.
    onedrive_files: Vec<String>,
    /// The bytes a program put into a file here that OneDrive does not have as such: they
    /// must be on disk under the item's name whatever a step does.
    own_contents: BTreeMap<LocalItemId, Vec<u8>>,
    /// The steps a plan made at some moment, run later or never.
    planned: Vec<FolderStep>,
    name_count: usize,
    written_count: usize,
    refused_count: usize,
}

impl Race {
    fn new(seed: u64) -> Self {
        let fixture = ItemFixture::new();
        fixture.show_onedrive_directory("d1");
        fixture.show_onedrive_directory("d2");
        let steps = make_steps(&fixture);
        // Files of OneDrive whose copies are on disk from the start.
        let mut onedrive_files = Vec::new();
        for path in ["c1", "d1/c2", "d2/c3"] {
            let shown = fixture.show_onedrive_file(path, path.as_bytes());
            fixture.land_copy(shown.local_item_id, path.as_bytes());
            onedrive_files.push(path.to_owned());
        }
        Race { fixture, steps, numbers: Numbers(seed | 1), onedrive_files, own_contents: BTreeMap::new(), planned: Vec::new(), name_count: 0, written_count: 0, refused_count: 0 }
    }

    fn make_path(&mut self, prefix: &str) -> String {
        self.name_count += 1;
        let directory = DIRECTORIES[self.numbers.below(DIRECTORIES.len())];
        match directory {
            "" => format!("{prefix}{}", self.name_count),
            directory => format!("{directory}/{prefix}{}", self.name_count),
        }
    }

    fn make_content(&mut self) -> Vec<u8> {
        format!("content {}", self.numbers.next()).into_bytes()
    }

    /// The files the folder shows, by local item id.
    fn list_shown_files(&self) -> Vec<Item> {
        self.fixture.list_items().into_iter().filter(|row| row.kind == ItemKind::File && row.local.is_some()).collect()
    }

    fn find_path(&self, item: LocalItemId) -> String {
        self.fixture.read_rows(move |reader| reader.find_path(item)).expect("a shown item has a path").to_string_lossy().into_owned()
    }

    // What OneDrive does, told by a listing.

    fn list_new_file(&mut self) {
        let (path, content) = (self.make_path("f"), self.make_content());
        self.fixture.list_onedrive_file(&path, &content);
        self.onedrive_files.push(path);
    }

    fn list_changed_content(&mut self) {
        let Some(path) = self.numbers.pick(&self.onedrive_files).cloned() else { return };
        let content = self.make_content();
        self.fixture.list_onedrive_file(&path, &content);
    }

    fn list_move(&mut self) {
        if self.onedrive_files.is_empty() {
            return;
        }
        let from = self.onedrive_files.swap_remove(self.numbers.below(self.onedrive_files.len()));
        let to = self.make_path("m");
        self.fixture.list_onedrive_move(&from, &to);
    }

    fn list_removal(&mut self) {
        if self.onedrive_files.is_empty() {
            return;
        }
        let path = self.onedrive_files.swap_remove(self.numbers.below(self.onedrive_files.len()));
        self.fixture.list_onedrive_removal(&path);
    }

    // What a program does, as the mount's write operations do it: the disk through a
    // place change on the write-operation thread, then the row.

    fn make_own_file(&mut self) {
        let (path, content) = (self.make_path("o"), self.make_content());
        let made = self.fixture.make_own_file(&path, &content);
        self.own_contents.insert(made.local_item_id, content);
    }

    fn write_file(&mut self) {
        let on_disk: Vec<Item> = self.list_shown_files().into_iter().filter(|row| row.content_presence == ContentPresence::OnDisk).collect();
        let Some(row) = self.numbers.pick(&on_disk).cloned() else { return };
        self.write(&row);
    }

    fn write(&mut self, row: &Item) {
        let content = self.make_content();
        self.fixture.change_file_here(row.local_item_id, &content);
        self.own_contents.insert(row.local_item_id, content);
    }

    fn rename_file(&mut self) {
        let shown = self.list_shown_files();
        let Some(row) = self.numbers.pick(&shown).cloned() else { return };
        self.rename(&row);
    }

    fn rename(&mut self, row: &Item) {
        let (item, from, to) = (row.local_item_id, self.find_path(row.local_item_id), self.make_path("p"));
        let (directory, name) = to.rsplit_once('/').unwrap_or(("", &to));
        let parent = self.fixture.find_item_at(directory).expect("the directory is shown").local_item_id;
        let place = ItemPlace { parent_local_item_id: parent, name: name.into() };
        let (place_changes, store, is_on_disk) = (Arc::clone(&self.fixture.place_changes), self.fixture.store.clone(), row.content_presence == ContentPresence::OnDisk);
        self.fixture.run_on_write_operation_thread(move || {
            let marks = store.call_blocking(move |store| store.item_store().reader().find_path_with_marks(parent)).expect("the rows").expect("the directory is shown").1;
            let change = place_changes.begin();
            if is_on_disk {
                change.move_entry(from.as_ref(), to.as_ref(), &marks).expect("the program's rename on disk");
            }
            store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Moved { item, place, is_name_ignored: false })).expect("the program's rename in the rows");
            change.end();
        });
    }

    fn remove_file(&mut self) {
        let shown = self.list_shown_files();
        let Some(row) = self.numbers.pick(&shown).cloned() else { return };
        self.remove(&row);
    }

    fn remove(&mut self, row: &Item) {
        let (item, path) = (row.local_item_id, self.find_path(row.local_item_id));
        let (place_changes, store, is_on_disk) = (Arc::clone(&self.fixture.place_changes), self.fixture.store.clone(), row.content_presence == ContentPresence::OnDisk);
        self.fixture.run_on_write_operation_thread(move || {
            let change = place_changes.begin();
            let undo = match is_on_disk {
                true => Some(change.set_aside(path.as_ref(), item).expect("the program's removal on disk")),
                false => None,
            };
            store.call_blocking(move |store| store.item_store().apply(&LocalVersionChange::Removed { item })).expect("the program's removal in the rows");
            change.end();
            undo.into_iter().for_each(|undo| place_changes.discard(undo));
        });
        self.own_contents.remove(&item);
    }

    // The plan, and its steps.

    /// A step for every row that has one to take, as a plan made now would carry it: by
    /// the versions the row has at this moment.
    fn plan(&mut self) {
        for row in self.fixture.list_items() {
            let item = ItemVersions::of(&row);
            let agrees_on_place = row.local.as_ref().is_some_and(|local| local.place == row.synced_place);
            let agrees_on_content = row.local.as_ref().is_some_and(|local| local.content == row.synced_content);
            let step = match (&row.local, &row.onedrive) {
                (None, Some(onedrive)) if onedrive.place.is_some() && row.synced_place.is_none() => FolderStep::ShowItem { item },
                (Some(_), None) if row.onedrive_item.is_some() && agrees_on_place && agrees_on_content && row.kind == ItemKind::File => FolderStep::RemoveItem { item },
                (Some(local), Some(onedrive)) if agrees_on_place && onedrive.place != local.place && onedrive.place.is_some() => FolderStep::MoveItem { item },
                (Some(local), Some(onedrive)) if row.kind == ItemKind::File && agrees_on_content && onedrive.content != local.content => FolderStep::TakeOneDriveContent { item },
                (Some(_), Some(_)) if row.kind == ItemKind::File && row.content_presence == ContentPresence::NotOnDisk && agrees_on_content => FolderStep::DownloadContent { item },
                _ => continue,
            };
            self.planned.push(step);
        }
    }

    fn take_planned_step(&mut self) -> Option<FolderStep> {
        (!self.planned.is_empty()).then(|| self.planned.swap_remove(self.numbers.below(self.planned.len())))
    }

    /// A program changes the very file a step was planned for, between the plan and the
    /// step: it writes it, renames it or removes it.
    fn disturb(&mut self, step: &FolderStep) {
        let Some(row) = self.fixture.find_item(step.item().local_item_id).filter(|row| row.kind == ItemKind::File && row.local.is_some()) else { return };
        match (self.numbers.below(3), row.content_presence) {
            (0, ContentPresence::OnDisk) => self.write(&row),
            (1, _) => self.remove(&row),
            _ => self.rename(&row),
        }
    }

    fn run_step(&mut self) {
        let Some(step) = self.take_planned_step() else { return };
        if self.numbers.below(3) == 0 {
            self.disturb(&step);
            self.fixture.assert_rows_and_disk_agree();
        }
        match run(&self.fixture, &self.steps, step.clone()) {
            StepOutcome::Written { .. } => self.written_count += 1,
            StepOutcome::RowsChanged(_) | StepOutcome::NotRun(_) | StepOutcome::Refused(_) => self.refused_count += 1,
            StepOutcome::Failed(failure) => panic!("the step {step:?} failed: {failure:?}"),
        }
    }

    /// A step that changes a name runs as far as its disk, and the daemon dies; the next
    /// start checks the backing directory, and removes what is still set aside.
    fn die_in_step(&mut self) {
        let Some(step) = self.take_planned_step() else { return };
        if matches!(step, FolderStep::ShowItem { .. } | FolderStep::DownloadContent { .. }) {
            return;
        }
        let _ = run_in_halves(&self.fixture, &self.steps, &step, || None);
        self.fixture.run_start_check();
        self.fixture.place_changes.remove_aside_entries().expect("what is still set aside goes");
        // What was planned before the death is planned again by the start's plan.
        self.planned.clear();
    }

    /// What must hold after everything: the rows and the disk say the same; nothing a
    /// program wrote is lost; no record stands and nothing is left set aside.
    fn assert_nothing_is_lost(&self, after: &str) {
        self.fixture.assert_rows_and_disk_agree();
        assert_eq!(count_records(&self.fixture), 0, "after {after}: a record stands");
        let disk = read_disk(&self.fixture);
        assert!(disk.keys().all(|path| !path.starts_with(".konedrive-aside")), "after {after}: an entry is left set aside: {:?}", disk.keys());
        for (item, content) in &self.own_contents {
            let row = self.fixture.find_item(*item).unwrap_or_else(|| panic!("after {after}: the row of item {item}, which a program wrote, is gone"));
            assert!(row.local.is_some(), "after {after}: the folder does not show item {item}, which a program wrote");
            let path = self.find_path(*item);
            assert_eq!(disk.get(&path).and_then(|(bytes, _)| bytes.as_ref()), Some(content), "after {after}: what a program wrote into {path} is not there");
        }
    }
}

#[test]
fn steps_towards_the_folder_run_against_a_programs_writes_and_renames_and_deaths() {
    let (mut written_count, mut refused_count) = (0, 0);
    for seed in 1..=16 {
        let mut race = Race::new(0x9E37_79B9_7F4A_7C15_u64.wrapping_mul(seed));
        for round in 0..60 {
            let chosen = race.numbers.below(16);
            let what = match chosen {
                0 | 1 => "a listing of a new file",
                2 => "a listing of another content",
                3 => "a listing of a move",
                4 => "a listing of a removal",
                5 => "a program's new file",
                6 => "a program's write",
                7 | 8 => "a program's rename",
                9 => "a program's removal",
                10 | 11 => "a plan",
                12..=14 => "a step",
                _ => "a death in a step",
            };
            match chosen {
                0 | 1 => race.list_new_file(),
                2 => race.list_changed_content(),
                3 => race.list_move(),
                4 => race.list_removal(),
                5 => race.make_own_file(),
                6 => race.write_file(),
                7 | 8 => race.rename_file(),
                9 => race.remove_file(),
                10 | 11 => race.plan(),
                12..=14 => race.run_step(),
                _ => race.die_in_step(),
            }
            race.assert_nothing_is_lost(&format!("{what} (seed {seed}, round {round})"));
        }
        // What is still planned runs, whatever it was planned from.
        while !race.planned.is_empty() {
            race.run_step();
            race.assert_nothing_is_lost(&format!("the steps left (seed {seed})"));
        }
        written_count += race.written_count;
        refused_count += race.refused_count;
    }
    assert!(written_count > 60 && refused_count > 20, "the race runs steps that write ({written_count}) and steps planned from other rows ({refused_count})");
}
