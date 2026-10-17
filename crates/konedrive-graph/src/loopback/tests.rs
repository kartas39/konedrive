use super::*;

async fn get(port: u16, target: &str) -> (u16, String) {
    let response = reqwest::get(format!("http://127.0.0.1:{port}{target}")).await.unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

/// A listener waiting for `state`, the browser's request for `target` under way, and what
/// the listener made of it: the request is answered only through the [`Answer`].
async fn redirected(state: &'static str, target: &'static str) -> (Callback, Answer, tokio::task::JoinHandle<(u16, String)>) {
    let listener = LoopbackListener::bind().await.unwrap();
    let port = listener.port();
    let browser = tokio::spawn(get(port, target));
    let (callback, answer) = listener.wait(state, Duration::from_secs(5)).await.unwrap();
    (callback, answer, browser)
}

#[tokio::test]
async fn returns_code_for_matching_state() {
    let listener = LoopbackListener::bind().await.unwrap();
    assert_eq!(listener.redirect_uri(), format!("http://localhost:{}", listener.port()));
    let (callback, answer, browser) = redirected("S1", "/?code=C1&state=S1").await;
    assert_eq!(callback, Callback::Code("C1".into()));
    answer.say("Signed in as a@example.com.");
    let (status, body) = browser.await.unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("<p>Signed in as a@example.com.</p>"), "{body}");
    assert!(body.contains("close this tab"));
}

/// The browser is not answered before it is said what came of the sign-in.
#[tokio::test]
async fn the_answer_waits_to_be_said() {
    let (_, answer, mut browser) = redirected("S5", "/?code=C5&state=S5").await;
    assert!(tokio::time::timeout(Duration::from_millis(200), &mut browser).await.is_err(), "answered untold");
    answer.say("done");
    assert!(browser.await.unwrap().1.contains("<p>done</p>"));
}

#[tokio::test]
async fn the_sentence_is_text_not_markup() {
    let (_, answer, browser) = redirected("S6", "/?code=C6&state=S6").await;
    answer.say("This account is 'A' <a href=\"x\">&</a>");
    let (status, body) = browser.await.unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("<p>This account is &#39;A&#39; &lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;</p>"), "{body}");
}

#[tokio::test]
async fn an_answer_dropped_untold_points_to_konedrive() {
    let (_, answer, browser) = redirected("S7", "/?code=C7&state=S7").await;
    drop(answer);
    let (status, body) = browser.await.unwrap();
    assert_eq!(status, 200);
    assert!(body.contains(UNTOLD.replace('\'', "&#39;").as_str()), "{body}");
}

/// An answer nobody says in time is given all the same.
#[tokio::test]
async fn an_answer_is_not_waited_for_for_ever() {
    let server = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = server.local_addr().unwrap().port();
    let browser = tokio::spawn(get(port, "/"));
    let (mut stream, _) = server.accept().await.unwrap();
    read_target(&mut stream).await.unwrap();
    let _held = Answer::hold(stream, Duration::from_millis(100));
    assert!(browser.await.unwrap().1.contains("Look at KOneDrive"));
}

#[tokio::test]
async fn ignores_other_paths_and_wrong_state() {
    let listener = LoopbackListener::bind().await.unwrap();
    let port = listener.port();
    let waiter = tokio::spawn(async move { listener.wait("S2", Duration::from_secs(5)).await });
    assert_eq!(get(port, "/favicon.ico").await.0, 404);
    assert_eq!(get(port, "/?code=EVIL&state=WRONG").await.0, 404);
    let browser = tokio::spawn(get(port, "/?code=C2&state=S2"));
    let (callback, answer) = waiter.await.unwrap().unwrap();
    assert_eq!(callback, Callback::Code("C2".into()));
    drop(answer);
    assert_eq!(browser.await.unwrap().0, 200);
}

#[tokio::test]
async fn reports_authorization_errors() {
    let (callback, _answer, _browser) =
        redirected("S3", "/?error=access_denied&error_description=user%20cancelled&state=S3").await;
    assert_eq!(callback, Callback::Error { error: "access_denied".into(), description: "user cancelled".into() });
}

#[tokio::test]
async fn times_out() {
    let listener = LoopbackListener::bind().await.unwrap();
    let result = listener.wait("S", Duration::from_millis(100)).await;
    assert!(matches!(result, Err(LoopbackError::TimedOut)));
}
