mod common;
use common::*;
use tofupilot::types::*;

async fn create_batch_via_run(uid_val: &str) -> String {
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;
    let batch_number = format!("BATCH-{uid_val}");

    client().runs().create()
        .serial_number(format!("SN-B-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-B-{uid_val}"))
        .batch_number(&batch_number)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    batch_number
}

#[tokio::test]
async fn create_batch_returns_id() {
    let uid_val = uid();
    let result = client().batches().create()
        .number(format!("BATCH-CRE-{uid_val}"))
        .send()
        .await
        .unwrap();
    assert!(!result.id.is_empty());
}

#[tokio::test]
async fn get_batch_returns_matching_data() {
    let uid_val = uid();
    let batch_number = create_batch_via_run(&uid_val).await;

    let fetched = client().batches().get()
        .number(&batch_number)
        .send()
        .await
        .unwrap();

    assert_eq!(batch_number, fetched.number);
}

#[tokio::test]
async fn get_batch_nonexistent_returns_not_found() {
    let result = client().batches().get()
        .number(format!("NONEXISTENT-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn list_batches_returns_list() {
    create_batch_via_run(&uid()).await;
    let result = client().batches().list()
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_batches_with_search_query() {
    let uid_val = uid();
    let batch_number = create_batch_via_run(&uid_val).await;

    let result = client().batches().list()
        .search_query(&batch_number)
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_batches_with_number_filter() {
    let uid_val = uid();
    let batch_number = create_batch_via_run(&uid_val).await;

    let result = client().batches().list()
        .numbers(vec![batch_number.clone()])
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
    assert_eq!(batch_number, result.data[0].number);
}

#[tokio::test]
async fn list_batches_pagination() {
    for _ in 0..3 {
        create_batch_via_run(&uid()).await;
    }

    let page1 = client().batches().list()
        .limit(1)
        .send()
        .await
        .unwrap();

    assert_eq!(1, page1.data.len());
    if page1.meta.has_more {
        let cursor = *page1.meta.next_cursor.as_ref().unwrap();
        let page2 = client().batches().list()
            .limit(1)
            .cursor(cursor)
            .send()
            .await
            .unwrap();
        assert_eq!(1, page2.data.len());
        assert_ne!(page1.data[0].id, page2.data[0].id);
    }
}

#[tokio::test]
async fn list_batches_sort_order() {
    for _ in 0..2 {
        create_batch_via_run(&uid()).await;
    }

    let desc = client().batches().list()
        .sort_order(ListSortOrder::Desc)
        .limit(2)
        .send()
        .await
        .unwrap();

    let asc = client().batches().list()
        .sort_order(ListSortOrder::Asc)
        .limit(2)
        .send()
        .await
        .unwrap();

    if desc.data.len() >= 2 && asc.data.len() >= 2 {
        assert!(desc.data[0].created_at >= desc.data[1].created_at);
        assert!(asc.data[0].created_at <= asc.data[1].created_at);
    }
}

#[tokio::test]
async fn delete_batch_returns_id() {
    let uid_val = uid();
    let number = format!("BATCH-DEL-{uid_val}");
    let created = client().batches().create()
        .number(&number)
        .send()
        .await
        .unwrap();

    let deleted = client().batches().delete()
        .number(&number)
        .send()
        .await
        .unwrap();
    assert!(deleted.id.contains(&created.id));
}

#[tokio::test]
async fn delete_batch_nonexistent_returns_not_found() {
    let result = client().batches().delete()
        .number(format!("NONEXISTENT-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_batch_number() {
    let uid_val = uid();
    let old_number = format!("BATCH-UPOLD-{uid_val}");
    let new_number = format!("BATCH-UPNEW-{uid_val}");

    client().batches().create()
        .number(&old_number)
        .send()
        .await
        .unwrap();

    client().batches().update()
        .number(&old_number)
        .new_number(&new_number)
        .send()
        .await
        .unwrap();

    // New number should be fetchable
    let fetched = client().batches().get()
        .number(&new_number)
        .send()
        .await
        .unwrap();
    assert_eq!(new_number, fetched.number);

    // Old number should 404
    let result = client().batches().get()
        .number(&old_number)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_batch_duplicate_number_returns_conflict() {
    let uid_val = uid();
    let number1 = format!("BATCH-DUP1-{uid_val}");
    let number2 = format!("BATCH-DUP2-{uid_val}");

    client().batches().create().number(&number1).send().await.unwrap();
    client().batches().create().number(&number2).send().await.unwrap();

    let result = client().batches().update()
        .number(&number2)
        .new_number(&number1)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Conflict(_))));
}

#[tokio::test]
async fn create_batch_empty_number_fails() {
    let result = client().batches().create()
        .number("")
        .send()
        .await;
    assert!(result.is_err());
}
