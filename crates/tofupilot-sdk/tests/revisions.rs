mod common;
use common::*;

async fn create_part(uid_val: &str) -> String {
    let part_number = format!("PART-RV-{uid_val}");
    client().parts().create()
        .number(&part_number)
        .name(format!("Rev Part {uid_val}"))
        .send()
        .await
        .unwrap();
    part_number
}

#[tokio::test]
async fn create_revision_returns_id() {
    let uid_val = uid();
    let part_number = create_part(&uid_val).await;

    let revision = client().revisions().create()
        .part_number(&part_number)
        .number(format!("REV-{uid_val}"))
        .send()
        .await
        .unwrap();
    assert!(!revision.id.is_empty());
}

#[tokio::test]
async fn get_revision_returns_matching_data() {
    let uid_val = uid();
    let part_number = create_part(&uid_val).await;
    let rev_number = format!("REV-G-{uid_val}");

    let created = client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    let fetched = client().revisions().get()
        .part_number(&part_number)
        .revision_number(&rev_number)
        .send()
        .await
        .unwrap();

    assert_eq!(created.id, fetched.id);
    assert_eq!(rev_number, fetched.number);
}

#[tokio::test]
async fn get_revision_nonexistent_returns_not_found() {
    let uid_val = uid();
    let part_number = create_part(&uid_val).await;

    let result = client().revisions().get()
        .part_number(&part_number)
        .revision_number(format!("REV-NONE-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn delete_revision_returns_id() {
    let uid_val = uid();
    let part_number = create_part(&uid_val).await;
    let rev_number = format!("REV-D-{uid_val}");

    let created = client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    let deleted = client().revisions().delete()
        .part_number(&part_number)
        .revision_number(&rev_number)
        .send()
        .await
        .unwrap();
    assert_eq!(created.id, deleted.id);
}

#[tokio::test]
async fn delete_revision_nonexistent_returns_not_found() {
    let uid_val = uid();
    let part_number = create_part(&uid_val).await;

    let result = client().revisions().delete()
        .part_number(&part_number)
        .revision_number(format!("REV-NONE-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn create_revision_duplicate_on_same_part_returns_conflict() {
    let uid_val = uid();
    let part_number = create_part(&uid_val).await;
    let rev_number = format!("REV-DUP-{uid_val}");

    client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    let result = client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Conflict(_))));
}

#[tokio::test]
async fn create_revision_same_number_different_parts_succeeds() {
    let uid_val = uid();
    let rev_number = format!("REV-SHARED-{uid_val}");

    let part1 = create_part(&format!("{uid_val}a")).await;
    let part2 = create_part(&format!("{uid_val}b")).await;

    let rev1 = client().revisions().create()
        .part_number(&part1)
        .number(&rev_number)
        .send()
        .await
        .unwrap();
    let rev2 = client().revisions().create()
        .part_number(&part2)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    assert!(!rev1.id.is_empty());
    assert!(!rev2.id.is_empty());
    assert_ne!(rev1.id, rev2.id);
}

#[tokio::test]
async fn create_revision_invalid_part_number_returns_not_found() {
    let result = client().revisions().create()
        .part_number(format!("PART-INVALID-{}", uid()))
        .number(format!("REV-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}
