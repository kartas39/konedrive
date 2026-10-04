use zbus::DBusError;

use super::*;

fn name_and_message(fault: &Fault) -> (String, Option<String>) {
    (fault.name().to_string(), fault.description().map(str::to_owned))
}

/// Every refusal of the folder goes out under the name written here, with the error's own
/// words as the message. The match does not compile until a new `SyncError` has its line.
#[test]
fn every_refusal_of_the_folder_goes_out_under_its_name() {
    let all = [
        SyncError::NotEmpty,
        SyncError::Unsupported("no user attributes".into()),
        SyncError::InUse,
        SyncError::NoRoot,
        SyncError::NoHelper,
        SyncError::NotManaged,
        SyncError::NotHydrated,
        SyncError::ModifiedLocally,
        SyncError::OutsideRoot,
        SyncError::ForeignFolder,
        SyncError::Overlaps("Work".into()),
        SyncError::AlreadyRegistered,
        SyncError::NotSignedIn,
        SyncError::NoSource,
        SyncError::NoConflict("/f/a".into()),
        SyncError::NotAllowed("/f/a is pinned by /f: unpin it first".into()),
        SyncError::NotUploaded("/f/a".into()),
        SyncError::NotInOneDrive("/f/a".into()),
        SyncError::Unreachable("no network".into()),
        SyncError::InvalidArgs("\"a/b\": a pattern names one name".into()),
        SyncError::PendingUploads("2 change(s) made here have not been uploaded yet".into()),
        SyncError::Io("the disk is full".into()),
    ];
    for error in all {
        let said = error.to_string();
        let (name, message) = match &error {
            SyncError::NotEmpty | SyncError::ForeignFolder => ("org.konedrive.Error.NotEmpty", said.clone()),
            SyncError::Unsupported(_) => ("org.konedrive.Error.Unsupported", said.clone()),
            SyncError::InUse => ("org.konedrive.Error.InUse", said.clone()),
            SyncError::NoRoot => ("org.konedrive.Error.NoRoot", said.clone()),
            SyncError::NoHelper => ("org.konedrive.Error.NoHelper", said.clone()),
            SyncError::NotManaged => ("org.konedrive.Error.NotManaged", said.clone()),
            SyncError::NotHydrated => ("org.konedrive.Error.NotHydrated", said.clone()),
            SyncError::ModifiedLocally => ("org.konedrive.Error.ModifiedLocally", said.clone()),
            SyncError::OutsideRoot => ("org.konedrive.Error.OutsideRoot", said.clone()),
            SyncError::Overlaps(_) => ("org.konedrive.Error.Overlaps", said.clone()),
            SyncError::AlreadyRegistered => ("org.konedrive.Error.AlreadyRegistered", said.clone()),
            SyncError::NotSignedIn => ("org.konedrive.Error.NotSignedIn", said.clone()),
            SyncError::NoSource => ("org.konedrive.Error.NoSource", said.clone()),
            SyncError::NoConflict(_) => ("org.konedrive.Error.NoConflict", said.clone()),
            SyncError::NotAllowed(_) => ("org.konedrive.Error.NotAllowed", said.clone()),
            SyncError::NotUploaded(_) | SyncError::NotInOneDrive(_) => ("org.konedrive.Error.NotUploaded", said.clone()),
            SyncError::Unreachable(_) => ("org.konedrive.Error.Unreachable", said.clone()),
            SyncError::PendingUploads(_) => ("org.konedrive.Error.PendingUploads", said.clone()),
            SyncError::Io(_) => ("org.konedrive.Error.Failed", said.clone()),
            // As it goes out today: an error of zbus's own, the bus's name inside the message.
            SyncError::InvalidArgs(_) => ("org.freedesktop.zbus.Error", format!("org.freedesktop.DBus.Error.InvalidArgs: {said}")),
        };
        let fault = to_fault(error);
        assert_eq!(fault.name().as_str(), name, "{said}");
        // What the reply carries: the message, or of zbus's own error what it says of itself.
        let carried = match &fault {
            Fault::ZBus(inner) => inner.to_string(),
            Fault::Refused(_, message) => message.clone(),
        };
        assert_eq!(carried, message, "{name}");
    }
}

/// The mode's, the accounts' and the account's refusals: the daemon's names where a client
/// can act on them, the bus's own for an argument that is not one and for the rest.
#[test]
fn the_mode_and_the_accounts_refuse_under_their_names() {
    let why = || "why".to_owned();
    let said = |name: &str| (name.to_owned(), Some(why()));
    let mode = |error: ModeError| name_and_message(&Fault::from(error));
    assert_eq!(mode(ModeError::WritesNotAllowed(why())), said("org.konedrive.Error.WritesNotAllowed"));
    assert_eq!(mode(ModeError::ModeNotGranted(why())), said("org.konedrive.Error.ModeNotGranted"));
    assert_eq!(mode(ModeError::PendingUploads(why())), said("org.konedrive.Error.PendingUploads"));
    assert_eq!(mode(ModeError::NotSignedIn(why())), said("org.konedrive.Error.NotSignedIn"));
    assert_eq!(mode(ModeError::Failed(why())), said("org.konedrive.Error.Failed"));
    assert_eq!(mode(ModeError::InvalidMode(why())), said("org.freedesktop.DBus.Error.InvalidArgs"));

    let manager = |error: ManagerError| name_and_message(&Fault::from(error));
    assert_eq!(manager(ManagerError::InvalidArgs(why())), said("org.freedesktop.DBus.Error.InvalidArgs"));
    assert_eq!(manager(ManagerError::Failed(why())), said("org.freedesktop.DBus.Error.Failed"));
    assert_eq!(
        manager(ManagerError::NoAccount("/org/konedrive/Accounts/x".into())),
        ("org.konedrive.Error.NoAccount".to_owned(), Some("there is no account /org/konedrive/Accounts/x".to_owned()))
    );
    assert_eq!(manager(ManagerError::Sync(SyncError::NoHelper)).0, "org.konedrive.Error.NoHelper");

    let account = |error: AccountError| name_and_message(&Fault::from(error)).0;
    assert_eq!(account(AccountError::InvalidClientId), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(account(AccountError::InvalidLabel(why())), "org.freedesktop.DBus.Error.InvalidArgs");
    assert_eq!(account(AccountError::Busy), "org.freedesktop.DBus.Error.Failed");
    assert_eq!(account(AccountError::NoClientId), "org.freedesktop.DBus.Error.Failed");
    assert_eq!(account(AccountError::Failed(why())), "org.freedesktop.DBus.Error.Failed");
}

/// A refusal this build does not know still goes out: under its name when it is one, and
/// as having no name of its own when it is none.
#[test]
fn a_refusal_with_no_known_name_still_goes_out() {
    let later = Fault::refused(Refusal::Other("org.konedrive.Error.Later".into()), "x");
    assert_eq!(later.name().as_str(), "org.konedrive.Error.Later");
    let no_name = Fault::refused(Refusal::Other("not a name".into()), "x");
    assert_eq!(no_name.name().as_str(), "org.konedrive.Error.Failed");
    assert_eq!(no_name.to_string(), "org.konedrive.Error.Failed: x");
}
