mod common;
use common::*;

#[tokio::test]
async fn create_part_returns_id() {
    let uid_val = uid();
    let result = client().parts().create()
        .number(format!("PART-{uid_val}"))
        .name(format!("Part {uid_val}"))
        .send()
        .await
        .unwrap();
    assert!(!result.id.is_empty());
}

#[tokio::test]
async fn get_part_returns_matching_data() {
    let uid_val = uid();
    let number = format!("PART-GET-{uid_val}");
    let name = format!("Part Get {uid_val}");

    client().parts().create()
        .number(&number)
        .name(&name)
        .send()
        .await
        .unwrap();

    let fetched = client().parts().get()
        .number(&number)
        .send()
        .await
        .unwrap();

    assert_eq!(number, fetched.number);
    assert_eq!(name, fetched.name);
}

#[tokio::test]
async fn get_part_nonexistent_returns_not_found() {
    let result = client().parts().get()
        .number(format!("NONEXISTENT-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn list_parts_returns_list() {
    client().parts().create()
        .number(format!("PART-LST-{}", uid()))
        .name(format!("Part List {}", uid()))
        .send()
        .await
        .unwrap();

    let result = client().parts().list()
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_parts_with_search_query() {
    let uid_val = uid();
    let number = format!("PART-SRQ-{uid_val}");
    client().parts().create()
        .number(&number)
        .name(format!("Part Srq {uid_val}"))
        .send()
        .await
        .unwrap();

    let result = client().parts().list()
        .search_query(&number)
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_parts_pagination() {
    for _ in 0..3 {
        let u = uid();
        client().parts().create()
            .number(format!("PART-PG-{u}"))
            .name(format!("Part Pg {u}"))
            .send()
            .await
            .unwrap();
    }

    let page1 = client().parts().list()
        .limit(1)
        .send()
        .await
        .unwrap();
    assert_eq!(1, page1.data.len());

    if page1.meta.has_more {
        let cursor = *page1.meta.next_cursor.as_ref().unwrap();
        let page2 = client().parts().list()
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
async fn delete_part_returns_id() {
    let uid_val = uid();
    let number = format!("PART-DEL-{uid_val}");

    let created = client().parts().create()
        .number(&number)
        .name(format!("Part Del {uid_val}"))
        .send()
        .await
        .unwrap();

    let deleted = client().parts().delete()
        .number(&number)
        .send()
        .await
        .unwrap();
    assert_eq!(created.id, deleted.id);
}

#[tokio::test]
async fn delete_part_nonexistent_returns_not_found() {
    let result = client().parts().delete()
        .number(format!("NONEXISTENT-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_part_name() {
    let uid_val = uid();
    let number = format!("PART-UPD-{uid_val}");

    client().parts().create()
        .number(&number)
        .name(format!("Part Old {uid_val}"))
        .send()
        .await
        .unwrap();

    let new_name = format!("Part New {uid_val}");
    client().parts().update()
        .number(&number)
        .name(&new_name)
        .send()
        .await
        .unwrap();

    let fetched = client().parts().get()
        .number(&number)
        .send()
        .await
        .unwrap();
    assert_eq!(new_name, fetched.name);
}

#[tokio::test]
async fn update_part_number() {
    let uid_val = uid();
    let old_number = format!("PART-UPOLD-{uid_val}");
    let new_number = format!("PART-UPNEW-{uid_val}");

    client().parts().create()
        .number(&old_number)
        .name(format!("Part Upd {uid_val}"))
        .send()
        .await
        .unwrap();

    client().parts().update()
        .number(&old_number)
        .new_number(&new_number)
        .send()
        .await
        .unwrap();

    // New number fetchable
    let fetched = client().parts().get()
        .number(&new_number)
        .send()
        .await
        .unwrap();
    assert_eq!(new_number, fetched.number);

    // Old number 404
    let result = client().parts().get()
        .number(&old_number)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_part_duplicate_number_returns_conflict() {
    let uid_val = uid();
    let number1 = format!("PART-DUP1-{uid_val}");
    let number2 = format!("PART-DUP2-{uid_val}");

    client().parts().create().number(&number1).name("P1").send().await.unwrap();
    client().parts().create().number(&number2).name("P2").send().await.unwrap();

    let result = client().parts().update()
        .number(&number2)
        .new_number(&number1)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Conflict(_))));
}

#[tokio::test]
async fn create_part_duplicate_number_returns_conflict() {
    let uid_val = uid();
    let number = format!("PART-CDUP-{uid_val}");

    client().parts().create().number(&number).name("P1").send().await.unwrap();

    let result = client().parts().create()
        .number(&number)
        .name("P2")
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Conflict(_))));
}

#[tokio::test]
async fn create_part_empty_number_fails() {
    let result = client().parts().create()
        .number("")
        .name("Empty")
        .send()
        .await;
    assert!(result.is_err());
}
