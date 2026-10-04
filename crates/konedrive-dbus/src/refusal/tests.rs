use super::*;
use crate::ERROR_PREFIX;

/// Every name is written out here as it goes over the bus, so a name reworded in
/// [`Refusal::known_name`] fails this test: the window's and Dolphin's tables are keyed by
/// these. The match does not compile until a new variant has its line.
#[test]
fn every_refusal_goes_out_under_its_name_and_is_read_back_from_it() {
    let spelled = |refusal: &Refusal| match refusal {
        Refusal::NotEmpty => "org.konedrive.Error.NotEmpty",
        Refusal::Unsupported => "org.konedrive.Error.Unsupported",
        Refusal::InUse => "org.konedrive.Error.InUse",
        Refusal::NoRoot => "org.konedrive.Error.NoRoot",
        Refusal::NoHelper => "org.konedrive.Error.NoHelper",
        Refusal::NotManaged => "org.konedrive.Error.NotManaged",
        Refusal::NotHydrated => "org.konedrive.Error.NotHydrated",
        Refusal::ModifiedLocally => "org.konedrive.Error.ModifiedLocally",
        Refusal::OutsideRoot => "org.konedrive.Error.OutsideRoot",
        Refusal::AlreadyRegistered => "org.konedrive.Error.AlreadyRegistered",
        Refusal::NotSignedIn => "org.konedrive.Error.NotSignedIn",
        Refusal::NoSource => "org.konedrive.Error.NoSource",
        Refusal::NoConflict => "org.konedrive.Error.NoConflict",
        Refusal::NotAllowed => "org.konedrive.Error.NotAllowed",
        Refusal::Overlaps => "org.konedrive.Error.Overlaps",
        Refusal::NoAccount => "org.konedrive.Error.NoAccount",
        Refusal::NotUploaded => "org.konedrive.Error.NotUploaded",
        Refusal::PendingUploads => "org.konedrive.Error.PendingUploads",
        Refusal::Unreachable => "org.konedrive.Error.Unreachable",
        Refusal::NotUp => "org.konedrive.Error.NotUp",
        Refusal::Failed => "org.konedrive.Error.Failed",
        Refusal::WritesNotAllowed => "org.konedrive.Error.WritesNotAllowed",
        Refusal::ModeNotGranted => "org.konedrive.Error.ModeNotGranted",
        Refusal::InvalidArgs => "org.freedesktop.DBus.Error.InvalidArgs",
        Refusal::BusFailed => "org.freedesktop.DBus.Error.Failed",
        Refusal::UnknownObject => "org.freedesktop.DBus.Error.UnknownObject",
        Refusal::UnknownMethod => "org.freedesktop.DBus.Error.UnknownMethod",
        Refusal::UnknownInterface => "org.freedesktop.DBus.Error.UnknownInterface",
        Refusal::Internal => "org.freedesktop.zbus.Error",
        Refusal::Other(name) => panic!("{name} is in the list of the known"),
    };
    assert_eq!(Refusal::ALL.len(), 29);
    for (at, refusal) in Refusal::ALL.iter().enumerate() {
        let name = spelled(refusal);
        assert_eq!(refusal.name(), name);
        assert_eq!(refusal.known_name(), Some(name));
        assert_eq!(refusal.to_string(), name);
        assert_eq!(&Refusal::parse(name), refusal, "{name}");
        assert!(zbus::names::ErrorName::try_from(name).is_ok(), "{name} is a D-Bus error name");
        assert_eq!(Refusal::ALL.iter().position(|other| other == refusal), Some(at), "{name} is listed once");
    }
    // The daemon's own names are the first twenty-three, and only they are under its prefix.
    for (at, refusal) in Refusal::ALL.iter().enumerate() {
        let ours = refusal.name().strip_prefix(ERROR_PREFIX).is_some_and(|rest| rest.starts_with('.'));
        assert_eq!(ours, at < 23, "{refusal}");
    }
}

/// A name no variant spells is kept as it came, and is none of the known: not one that
/// ends as a known name does, nor one that only begins as one.
#[test]
fn a_name_this_build_does_not_know_is_kept() {
    for name in [
        "org.konedrive.Error.NobodyWroteThisYet",
        "org.freedesktop.DBus.Error.InUse",
        "org.freedesktop.DBus.Error.NoReply",
        "org.konedrive.Error",
        "org.konedrive.Error.NoRoot.Again",
        "org.konedrive.ErrorNoRoot",
        "NoRoot",
        "",
    ] {
        let refusal = Refusal::parse(name);
        assert_eq!(refusal, Refusal::Other(name.to_owned()), "{name}");
        assert_eq!(refusal.name(), name);
        assert_eq!(refusal.known_name(), None);
        assert!(!refusal.is_gone());
    }
}

/// Only a reply that carries a name is a refusal; the three names for what is not there
/// say an account is gone.
#[test]
fn a_failed_call_is_read_by_its_name() {
    let named = |name: &'static str| {
        let name = zbus::names::OwnedErrorName::try_from(name).unwrap();
        let call = zbus::message::Message::method_call("/org/konedrive/Accounts", "Ping").unwrap().build(&()).unwrap();
        zbus::Error::MethodError(name, Some("the message".to_owned()), call)
    };
    assert_eq!(Refusal::from_error(&named("org.konedrive.Error.NoHelper")), Some(Refusal::NoHelper));
    assert_eq!(Refusal::from_error(&named("org.freedesktop.DBus.Error.Failed")), Some(Refusal::BusFailed));
    assert_eq!(
        Refusal::from_error(&named("org.konedrive.Error.Later")),
        Some(Refusal::Other("org.konedrive.Error.Later".to_owned()))
    );
    assert_eq!(Refusal::from_error(&zbus::Error::InvalidReply), None);
    let gone: Vec<&Refusal> = Refusal::ALL.iter().filter(|refusal| refusal.is_gone()).collect();
    assert_eq!(gone, [&Refusal::UnknownObject, &Refusal::UnknownMethod, &Refusal::UnknownInterface]);
}
