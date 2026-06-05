mod common;
use common::*;

#[tokio::test]
async fn list_users_returns_list() {
    let result = client().user().list()
        .send()
        .await
        .unwrap();
    assert!(!result.is_empty());
}

#[tokio::test]
async fn list_users_current() {
    let result = client().user().list()
        .current(true)
        .send()
        .await
        .unwrap();
    assert_eq!(1, result.len());
    assert!(!result[0].id.is_empty());
    assert!(!result[0].email.is_empty());
}
