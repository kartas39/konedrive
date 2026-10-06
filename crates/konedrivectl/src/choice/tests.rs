use super::{choose, command_prefix, AccountInfo, NoChoice, Source};
use crate::text::refusals::{refusal_text_in, Context, SyncAction};

fn account(id: &str, label: &str, email: &str) -> AccountInfo {
    AccountInfo {
        path: konedrive_dbus::account_path(id).unwrap(),
        id: id.into(),
        label: label.into(),
        email: email.into(),
    }
}

/// `docs/design/desktop.md` §3: an exact id, else a label, else an email, the last two
/// whatever the case; with no name, the only account; several and no
/// name is a mistake on the command line (exit status 2).
#[test]
fn an_account_is_chosen_by_id_label_or_email() {
    let accounts =
        [account("3f9a1c0e5b7d", "Personal", "ann@outlook.com"), account("8c21d07a44e1", "Family", "")];
    let chosen = |name: &str| choose(&accounts, Some((name, Source::Option))).map(|a| a.label.as_str());
    assert_eq!(chosen("8c21d07a44e1"), Ok("Family"));
    assert_eq!(chosen("family"), Ok("Family"));
    assert_eq!(chosen("ANN@outlook.com"), Ok("Personal"));
    let unknown = chosen("nobody").unwrap_err();
    assert_eq!(unknown.exit_status(), 2);
    assert!(unknown.to_string().contains("Personal, Family"), "{unknown}");

    let several = choose(&accounts, None).unwrap_err();
    assert_eq!(several, NoChoice::Several { labels: vec!["Personal".into(), "Family".into()] });
    assert_eq!(several.to_string(), "Several accounts: choose one with --account (Personal, Family)");
    assert_eq!(several.exit_status(), 2);
    assert_eq!(choose(&accounts[..1], None).unwrap().label, "Personal");
    assert_eq!(choose(&[], None).unwrap_err().exit_status(), 1);
    assert!(choose(&[], None).unwrap_err().to_string().contains("`konedrivectl account add` signs in"));
}

/// A name that fits two accounts — one's id and the other's label, or one label twice in a
/// hand-edited `config.toml` — is refused with exit status 2, listing both, never taken
/// as the first: `account remove` asks nothing.
#[test]
fn a_name_that_fits_two_accounts_is_refused() {
    let accounts = [account("3f9a1c0e5b7d", "Personal", ""), account("8c21d07a44e1", "3f9a1c0e5b7d", "")];
    let refused = choose(&accounts, Some(("3f9a1c0e5b7d", Source::Argument))).unwrap_err();
    assert_eq!(refused.exit_status(), 2);
    let said = refused.to_string();
    assert!(said.contains("Personal (3f9a1c0e5b7d), 3f9a1c0e5b7d (8c21d07a44e1)"), "{said}");
    assert_eq!(choose(&accounts, Some(("8c21d07a44e1", Source::Option))).unwrap().label, "3f9a1c0e5b7d");

    let twice = [account("3f9a1c0e5b7d", "Personal", ""), account("8c21d07a44e1", "personal", "")];
    assert!(matches!(choose(&twice, Some(("PERSONAL", Source::Option))), Err(NoChoice::Ambiguous { .. })));
}

/// Named from the environment with no account at all: exit status 1, naming the variable.
#[test]
fn a_name_with_no_account_at_all_says_where_it_came_from() {
    let refused = choose(&[], Some(("Test", Source::Environment))).unwrap_err();
    assert_eq!(refused.exit_status(), 1);
    assert!(refused.to_string().starts_with("KONEDRIVE_ACCOUNT names the account \"Test\""), "{refused}");
}

/// A suggested command names the account whenever the bare one could act on another.
#[test]
fn a_suggested_command_names_the_account_when_it_has_to() {
    assert_eq!(command_prefix(Some("Family"), false, false), "konedrivectl");
    assert_eq!(command_prefix(Some("Family"), true, false), "konedrivectl --account Family");
    assert_eq!(command_prefix(Some("My Home"), false, true), "konedrivectl --account 'My Home'");
    assert_eq!(command_prefix(None, true, false), "konedrivectl --account <account>");
    let context = Context { prefix: "konedrivectl --account Family", ..Context::default() };
    let text = refusal_text_in(
        SyncAction::Register("/home/u/Other"),
        Some("org.konedrive.Error.AlreadyRegistered"),
        "",
        Context { root: "/home/u/Family", ..context },
    );
    assert!(text.contains("run `konedrivectl --account Family sync forget` first"), "{text}");
}
