mod common;
use common::*;

async fn create_station(uid_val: &str) -> (String, String) {
    let name = format!("Station-{uid_val}");
    let proc_id = procedure_id().await;
    let created = client().stations().create()
        .name(&name)
        .procedure_id(proc_id)
        .send()
        .await
        .unwrap();
    (created.id, name)
}

#[tokio::test]
async fn create_station_returns_id() {
    let uid_val = uid();
    let (id, _) = create_station(&uid_val).await;
    assert!(!id.is_empty());
}

#[tokio::test]
async fn get_station_returns_matching_data() {
    let uid_val = uid();
    let (id, name) = create_station(&uid_val).await;

    let fetched = client().stations().get()
        .id(&id)
        .send()
        .await
        .unwrap();

    assert_eq!(id, fetched.id);
    assert_eq!(name, fetched.name);
}

#[tokio::test]
async fn get_station_nonexistent_returns_not_found() {
    let result = client().stations().get()
        .id(uuid::Uuid::new_v4().to_string())
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn list_stations_returns_list() {
    create_station(&uid()).await;
    let result = client().stations().list()
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_stations_with_search_query() {
    let uid_val = uid();
    let (_, name) = create_station(&uid_val).await;

    let result = client().stations().list()
        .search_query(&name)
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
    assert!(result.data.iter().any(|s| s.name == name));
}

#[tokio::test]
async fn list_stations_pagination() {
    for _ in 0..3 {
        create_station(&uid()).await;
    }

    let page1 = client().stations().list()
        .limit(1)
        .send()
        .await
        .unwrap();

    assert_eq!(1, page1.data.len());
    assert!(page1.meta.has_more);

    let cursor = *page1.meta.next_cursor.as_ref().unwrap();
    let page2 = client().stations().list()
        .limit(1)
        .cursor(cursor)
        .send()
        .await
        .unwrap();

    assert_eq!(1, page2.data.len());
    assert_ne!(page1.data[0].id, page2.data[0].id);
}

#[tokio::test]
async fn remove_station_returns_id() {
    let (id, _) = create_station(&uid()).await;
    let removed = client().stations().remove()
        .id(&id)
        .send()
        .await
        .unwrap();
    assert_eq!(id, removed.id);
}

#[tokio::test]
async fn remove_station_nonexistent_returns_not_found() {
    let result = client().stations().remove()
        .id(uuid::Uuid::new_v4().to_string())
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn remove_station_twice_fails() {
    let (id, _) = create_station(&uid()).await;
    client().stations().remove()
        .id(&id)
        .send()
        .await
        .unwrap();

    let result = client().stations().remove()
        .id(&id)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_station_name() {
    let uid_val = uid();
    let (id, _) = create_station(&uid_val).await;

    let new_name = format!("Station-Updated-{uid_val}");
    client().stations().update()
        .id(&id)
        .name(&new_name)
        .send()
        .await
        .unwrap();

    let fetched = client().stations().get()
        .id(&id)
        .send()
        .await
        .unwrap();
    assert_eq!(new_name, fetched.name);
}

#[tokio::test]
async fn update_station_nonexistent_returns_not_found() {
    let result = client().stations().update()
        .id(uuid::Uuid::new_v4().to_string())
        .name("irrelevant")
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn create_station_duplicate_name_returns_conflict() {
    let uid_val = uid();
    let (_, name) = create_station(&uid_val).await;
    let proc_id = procedure_id().await;

    let result = client().stations().create()
        .name(&name)
        .procedure_id(proc_id)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Conflict(_))));
}

#[tokio::test]
async fn get_current_with_user_key_returns_forbidden() {
    let result = client().stations().get_current()
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Forbidden(_))));
}
