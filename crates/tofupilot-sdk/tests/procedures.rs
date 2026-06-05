mod common;
use common::*;
use tofupilot::types::*;

#[tokio::test]
async fn create_procedure_returns_id() {
    let result = client().procedures().create()
        .name(format!("Proc {}", uid()))
        .send()
        .await
        .unwrap();
    assert!(!result.id.is_empty());
}

#[tokio::test]
async fn get_procedure_returns_matching_data() {
    let name = format!("Proc Get {}", uid());
    let created = client().procedures().create()
        .name(&name)
        .send()
        .await
        .unwrap();

    let fetched = client().procedures().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();

    assert_eq!(created.id, fetched.id);
    assert_eq!(name, fetched.name);
}

#[tokio::test]
async fn get_procedure_nonexistent_returns_not_found() {
    let result = client().procedures().get()
        .id(uuid::Uuid::new_v4().to_string())
        .send()
        .await;

    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn list_procedures_returns_list() {
    let result = client().procedures().list()
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_procedures_with_search_query() {
    let name = format!("Proc Srq {}", uid());
    client().procedures().create()
        .name(&name)
        .send()
        .await
        .unwrap();

    let result = client().procedures().list()
        .search_query(&name)
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_procedures_pagination() {
    for _ in 0..3 {
        client().procedures().create()
            .name(format!("Proc Pg {}", uid()))
            .send()
            .await
            .unwrap();
    }

    let page1 = client().procedures().list()
        .limit(1)
        .send()
        .await
        .unwrap();
    assert_eq!(page1.data.len(), 1);

    if page1.meta.has_more {
        let cursor = *page1.meta.next_cursor.as_ref().unwrap() as f64;
        let page2 = client().procedures().list()
            .limit(1)
            .cursor(cursor)
            .send()
            .await
            .unwrap();
        assert_eq!(page2.data.len(), 1);
        assert_ne!(page1.data[0].id, page2.data[0].id);
    }
}

#[tokio::test]
async fn delete_procedure_returns_id() {
    let created = client().procedures().create()
        .name(format!("Proc Del {}", uid()))
        .send()
        .await
        .unwrap();

    let deleted = client().procedures().delete()
        .id(&created.id)
        .send()
        .await
        .unwrap();
    assert_eq!(created.id, deleted.id);
}

#[tokio::test]
async fn delete_procedure_nonexistent_returns_not_found() {
    let result = client().procedures().delete()
        .id(uuid::Uuid::new_v4().to_string())
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_procedure_name() {
    let uid_val = uid();
    let created = client().procedures().create()
        .name(format!("Proc Old {uid_val}"))
        .send()
        .await
        .unwrap();

    let new_name = format!("Proc New {uid_val}");
    client().procedures().update()
        .id(&created.id)
        .name(&new_name)
        .send()
        .await
        .unwrap();

    let fetched = client().procedures().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();
    assert_eq!(new_name, fetched.name);
}

#[tokio::test]
async fn get_procedure_includes_recent_runs() {
    let uid_val = uid();
    let proc = client().procedures().create()
        .name(format!("Proc RR {uid_val}"))
        .send()
        .await
        .unwrap();

    let now = chrono::Utc::now();
    client().runs().create()
        .serial_number(format!("SN-RR-{uid_val}"))
        .procedure_id(&proc.id)
        .part_number(format!("PART-RR-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let fetched = client().procedures().get()
        .id(&proc.id)
        .send()
        .await
        .unwrap();

    assert!(!fetched.recent_runs.is_empty());
}

#[tokio::test]
async fn list_procedures_with_date_range() {
    let before = chrono::Utc::now();
    client().procedures().create()
        .name(format!("Proc DR {}", uid()))
        .send()
        .await
        .unwrap();
    let after = chrono::Utc::now();

    let result = client().procedures().list()
        .created_after(before)
        .created_before(after + chrono::Duration::seconds(1))
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn update_procedure_empty_name_fails() {
    let created = client().procedures().create()
        .name(format!("Proc EN {}", uid()))
        .send()
        .await
        .unwrap();

    let result = client().procedures().update()
        .id(&created.id)
        .name("")
        .send()
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn update_procedure_nonexistent_returns_not_found() {
    let result = client().procedures().update()
        .id(uuid::Uuid::new_v4().to_string())
        .name("irrelevant")
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_procedure_multiple_updates() {
    let uid_val = uid();
    let created = client().procedures().create()
        .name(format!("Proc MU {uid_val}"))
        .send()
        .await
        .unwrap();

    for i in 1..=3 {
        client().procedures().update()
            .id(&created.id)
            .name(format!("Proc MU {uid_val} v{i}"))
            .send()
            .await
            .unwrap();
    }

    let fetched = client().procedures().get()
        .id(&created.id)
        .send()
        .await
        .unwrap();
    assert!(fetched.name.ends_with("v3"));
}
