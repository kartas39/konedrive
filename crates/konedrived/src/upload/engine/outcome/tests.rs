use super::without_urls;

/// The journal line of a failure carries no address.
#[test]
fn the_journal_gets_no_url() {
    assert_eq!(
        without_urls("error sending request for url (https://graph.microsoft.com/v1.0/me/drive?x=1) and http://a.b/c done"),
        "error sending request for url (<url> and <url> done"
    );
    assert_eq!(without_urls("no address here"), "no address here");
}
