mod common;
use common::*;

#[tokio::test]
async fn delete_run_returns_ids() {
    let created = create_test_run(&uid()).await;
    let deleted = client().runs().delete()
        .ids(vec![created.id.clone()])
        .send()
        .await
        .unwrap();

    assert!(!deleted.id.is_empty());
    assert!(deleted.id.contains(&created.id));
}

#[tokio::test]
async fn delete_multiple_runs_returns_ids() {
    let run1 = create_test_run(&uid()).await;
    let run2 = create_test_run(&uid()).await;

    let deleted = client().runs().delete()
        .ids(vec![run1.id.clone(), run2.id.clone()])
        .send()
        .await
        .unwrap();

    assert_eq!(2, deleted.id.len());
    assert!(deleted.id.contains(&run1.id));
    assert!(deleted.id.contains(&run2.id));
}

#[tokio::test]
async fn delete_run_nonexistent_returns_not_found() {
    let result = client().runs().delete()
        .ids(vec![uuid::Uuid::new_v4().to_string()])
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}
