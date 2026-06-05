mod common;
use common::*;

async fn create_procedure(uid_val: &str) -> String {
    client().procedures().create()
        .name(format!("Proc Ver {uid_val}"))
        .send()
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn create_version_returns_id() {
    let uid_val = uid();
    let proc_id = create_procedure(&uid_val).await;

    let version = client().versions().create()
        .procedure_id(&proc_id)
        .tag(format!("v{uid_val}"))
        .send()
        .await
        .unwrap();
    assert!(!version.id.is_empty());
}

#[tokio::test]
async fn get_version_returns_matching_data() {
    let uid_val = uid();
    let proc_id = create_procedure(&uid_val).await;
    let tag = format!("v-g-{uid_val}");

    let created = client().versions().create()
        .procedure_id(&proc_id)
        .tag(&tag)
        .send()
        .await
        .unwrap();

    let fetched = client().versions().get()
        .procedure_id(&proc_id)
        .tag(&tag)
        .send()
        .await
        .unwrap();

    assert_eq!(created.id, fetched.id);
    assert_eq!(tag, fetched.tag);
}

#[tokio::test]
async fn get_version_nonexistent_returns_not_found() {
    let uid_val = uid();
    let proc_id = create_procedure(&uid_val).await;

    let result = client().versions().get()
        .procedure_id(&proc_id)
        .tag(format!("v-none-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn delete_version_returns_id() {
    let uid_val = uid();
    let proc_id = create_procedure(&uid_val).await;
    let tag = format!("v-d-{uid_val}");

    let created = client().versions().create()
        .procedure_id(&proc_id)
        .tag(&tag)
        .send()
        .await
        .unwrap();

    let deleted = client().versions().delete()
        .procedure_id(&proc_id)
        .tag(&tag)
        .send()
        .await
        .unwrap();
    assert_eq!(created.id, deleted.id);
}

#[tokio::test]
async fn delete_version_nonexistent_returns_not_found() {
    let uid_val = uid();
    let proc_id = create_procedure(&uid_val).await;

    let result = client().versions().delete()
        .procedure_id(&proc_id)
        .tag(format!("v-none-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}
