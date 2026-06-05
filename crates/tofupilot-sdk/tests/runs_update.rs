mod common;
use common::*;

#[tokio::test]
async fn update_run_returns_id() {
    let created = create_test_run(&uid()).await;
    let updated = client().runs().update()
        .id(&created.id)
        .send()
        .await
        .unwrap();
    assert!(!updated.id.is_empty());
    assert_eq!(created.id, updated.id);
}

#[tokio::test]
async fn update_run_nonexistent_returns_not_found() {
    let result = client().runs().update()
        .id(uuid::Uuid::new_v4().to_string())
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}
