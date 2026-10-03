use super::*;

async fn get(port: u16, target: &str) -> (u16, String) {
    let response = reqwest::get(format!("http://127.0.0.1:{port}{target}")).await.unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

#[tokio::test]
async fn returns_code_for_matching_state() {
    let listener = LoopbackListener::bind().await.unwrap();
    let port = listener.port();
    assert_eq!(listener.redirect_uri(), format!("http://localhost:{port}"));
    let waiter = tokio::spawn(async move { listener.wait("S1", Duration::from_secs(5)).await });
    let (status, body) = get(port, "/?code=C1&state=S1").await;
    assert_eq!(status, 200);
    assert!(body.contains("close this tab"));
    assert_eq!(waiter.await.unwrap().unwrap(), Callback::Code("C1".into()));
}

#[tokio::test]
async fn ignores_other_paths_and_wrong_state() {
    let listener = LoopbackListener::bind().await.unwrap();
    let port = listener.port();
    let waiter = tokio::spawn(async move { listener.wait("S2", Duration::from_secs(5)).await });
    assert_eq!(get(port, "/favicon.ico").await.0, 404);
    assert_eq!(get(port, "/?code=EVIL&state=WRONG").await.0, 404);
    assert_eq!(get(port, "/?code=C2&state=S2").await.0, 200);
    assert_eq!(waiter.await.unwrap().unwrap(), Callback::Code("C2".into()));
}

#[tokio::test]
async fn reports_authorization_errors() {
    let listener = LoopbackListener::bind().await.unwrap();
    let port = listener.port();
    let waiter = tokio::spawn(async move { listener.wait("S3", Duration::from_secs(5)).await });
    let target = "/?error=access_denied&error_description=user%20cancelled&state=S3";
    assert_eq!(get(port, target).await.0, 200);
    assert_eq!(
        waiter.await.unwrap().unwrap(),
        Callback::Error { error: "access_denied".into(), description: "user cancelled".into() }
    );
}

#[tokio::test]
async fn times_out() {
    let listener = LoopbackListener::bind().await.unwrap();
    let result = listener.wait("S", Duration::from_millis(100)).await;
    assert!(matches!(result, Err(LoopbackError::TimedOut)));
}
