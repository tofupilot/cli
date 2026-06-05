mod common;
use common::*;

async fn create_part_and_unit(prefix: &str) -> (String, String) {
    let uid_val = uid();
    let part_number = format!("PART-{prefix}-{uid_val}");
    let serial = format!("SN-{prefix}-{uid_val}");
    let rev_number = format!("REV-{prefix}-{uid_val}");

    client().parts().create()
        .number(&part_number)
        .name(format!("Part {uid_val}"))
        .send()
        .await
        .unwrap();

    client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    client().units().create()
        .serial_number(&serial)
        .part_number(&part_number)
        .revision_number(&rev_number)
        .send()
        .await
        .unwrap();

    (part_number, serial)
}

#[tokio::test]
async fn create_unit_returns_id() {
    let uid_val = uid();
    let part_number = format!("PART-CRE-{uid_val}");
    let rev_number = format!("REV-CRE-{uid_val}");

    client().parts().create()
        .number(&part_number)
        .name(format!("Part {uid_val}"))
        .send()
        .await
        .unwrap();

    client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    let result = client().units().create()
        .serial_number(format!("SN-CRE-{uid_val}"))
        .part_number(&part_number)
        .revision_number(&rev_number)
        .send()
        .await
        .unwrap();

    assert!(!result.id.is_empty());
}

#[tokio::test]
async fn get_unit_returns_matching_data() {
    let (part_number, serial) = create_part_and_unit("GET").await;
    let fetched = client().units().get()
        .serial_number(&serial)
        .send()
        .await
        .unwrap();

    assert_eq!(serial, fetched.serial_number);
    assert_eq!(part_number, fetched.part.number);
}

#[tokio::test]
async fn get_unit_nonexistent_returns_not_found() {
    let result = client().units().get()
        .serial_number(format!("NONEXISTENT-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn list_units_returns_list() {
    create_part_and_unit("LST").await;
    let result = client().units().list()
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_units_filter_by_serial_number() {
    let (_, serial) = create_part_and_unit("FSN").await;
    let result = client().units().list()
        .serial_numbers(vec![serial.clone()])
        .send()
        .await
        .unwrap();

    assert_eq!(1, result.data.len());
    assert_eq!(serial, result.data[0].serial_number);
}

#[tokio::test]
async fn list_units_filter_by_part_number() {
    let (part_number, _) = create_part_and_unit("FPN").await;
    let result = client().units().list()
        .part_numbers(vec![part_number.clone()])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
    assert!(result.data.iter().all(|u| u.part.number == part_number));
}

#[tokio::test]
async fn list_units_pagination() {
    for i in 0..3 {
        create_part_and_unit(&format!("PAG{i}")).await;
    }

    let page1 = client().units().list()
        .limit(1)
        .send()
        .await
        .unwrap();

    assert_eq!(1, page1.data.len());
    assert!(page1.meta.has_more);

    let cursor = *page1.meta.next_cursor.as_ref().unwrap();
    let page2 = client().units().list()
        .limit(1)
        .cursor(cursor)
        .send()
        .await
        .unwrap();

    assert_eq!(1, page2.data.len());
    assert_ne!(page1.data[0].id, page2.data[0].id);
}

#[tokio::test]
async fn delete_unit_returns_ids() {
    let (_, serial) = create_part_and_unit("DEL").await;
    let deleted = client().units().delete()
        .serial_numbers(vec![serial])
        .send()
        .await
        .unwrap();
    assert!(!deleted.id.is_empty());
}

#[tokio::test]
async fn update_unit_serial_number() {
    let (_, serial) = create_part_and_unit("UPD").await;
    let new_serial = format!("SN-UPNEW-{}", uid());

    client().units().update()
        .serial_number(&serial)
        .new_serial_number(&new_serial)
        .send()
        .await
        .unwrap();

    let fetched = client().units().get()
        .serial_number(&new_serial)
        .send()
        .await
        .unwrap();
    assert_eq!(new_serial, fetched.serial_number);
}

#[tokio::test]
async fn list_units_with_search_query() {
    let (_, serial) = create_part_and_unit("SRQ").await;

    let result = client().units().list()
        .search_query(&serial)
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
    assert!(result.data.iter().any(|u| u.serial_number == serial));
}

#[tokio::test]
async fn list_units_sort_order() {
    use tofupilot::types::ListSortOrder;

    for i in 0..2 {
        create_part_and_unit(&format!("SRT{i}")).await;
    }

    let desc = client().units().list()
        .sort_order(ListSortOrder::Desc)
        .limit(2)
        .send()
        .await
        .unwrap();

    let asc = client().units().list()
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
async fn list_units_filter_by_ids() {
    let (_, serial) = create_part_and_unit("FID").await;

    // First get the unit to know its ID
    let unit = client().units().get()
        .serial_number(&serial)
        .send()
        .await
        .unwrap();

    let result = client().units().list()
        .ids(vec![unit.id.clone()])
        .send()
        .await
        .unwrap();

    assert_eq!(1, result.data.len());
    assert_eq!(unit.id, result.data[0].id);
}

#[tokio::test]
async fn delete_unit_nonexistent_returns_not_found() {
    let result = client().units().delete()
        .serial_numbers(vec![format!("NONEXISTENT-{}", uid())])
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn update_unit_part_revision() {
    let uid_val = uid();
    let (_, serial) = create_part_and_unit("UPR").await;

    // Create new part + revision
    let new_part = format!("PART-UPRNEW-{uid_val}");
    let new_rev = format!("REV-UPRNEW-{uid_val}");
    client().parts().create()
        .number(&new_part)
        .name(format!("New Part {uid_val}"))
        .send()
        .await
        .unwrap();
    client().revisions().create()
        .part_number(&new_part)
        .number(&new_rev)
        .send()
        .await
        .unwrap();

    client().units().update()
        .serial_number(&serial)
        .part_number(&new_part)
        .revision_number(&new_rev)
        .send()
        .await
        .unwrap();

    let fetched = client().units().get()
        .serial_number(&serial)
        .send()
        .await
        .unwrap();
    assert_eq!(new_part, fetched.part.number);
}

#[tokio::test]
async fn update_unit_duplicate_serial_returns_conflict() {
    let (_, serial1) = create_part_and_unit("DUS1").await;
    let (_, serial2) = create_part_and_unit("DUS2").await;

    let result = client().units().update()
        .serial_number(&serial2)
        .new_serial_number(&serial1)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Conflict(_))));
}

#[tokio::test]
async fn create_unit_duplicate_serial_returns_conflict() {
    let (part_number, serial) = create_part_and_unit("DUCS").await;

    let rev = format!("REV-DUCS2-{}", uid());
    client().revisions().create()
        .part_number(&part_number)
        .number(&rev)
        .send()
        .await
        .unwrap();

    let result = client().units().create()
        .serial_number(&serial)
        .part_number(&part_number)
        .revision_number(&rev)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::Conflict(_))));
}

#[tokio::test]
async fn list_units_filter_by_revision_numbers() {
    let uid_val = uid();
    let part_number = format!("PART-RV-{uid_val}");
    let rev_number = format!("REV-RV-{uid_val}");

    client().parts().create()
        .number(&part_number)
        .name(format!("Part {uid_val}"))
        .send()
        .await
        .unwrap();

    client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    client().units().create()
        .serial_number(format!("SN-RV-{uid_val}"))
        .part_number(&part_number)
        .revision_number(&rev_number)
        .send()
        .await
        .unwrap();

    let result = client().units().list()
        .part_numbers(vec![part_number.clone()])
        .revision_numbers(vec![rev_number])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
    assert!(result.data.iter().all(|u| u.part.number == part_number));
}

#[tokio::test]
async fn list_units_filter_by_batch_numbers() {
    let uid_val = uid();
    let part_number = format!("PART-BN-{uid_val}");
    let rev_number = format!("REV-BN-{uid_val}");
    let batch_number = format!("BATCH-{uid_val}");
    let proc_id = procedure_id().await;

    client().parts().create()
        .number(&part_number)
        .name(format!("Part {uid_val}"))
        .send()
        .await
        .unwrap();

    client().revisions().create()
        .part_number(&part_number)
        .number(&rev_number)
        .send()
        .await
        .unwrap();

    // Create a run with batch_number to auto-create the batch and link the unit
    let now = chrono::Utc::now();
    client().runs().create()
        .serial_number(format!("SN-BN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part_number)
        .revision_number(&rev_number)
        .batch_number(&batch_number)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(tofupilot::types::Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().units().list()
        .part_numbers(vec![part_number])
        .batch_numbers(vec![batch_number])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_units_filter_by_created_at() {
    let now = chrono::Utc::now();
    let (part_number, _) = create_part_and_unit("CA").await;

    let result = client().units().list()
        .part_numbers(vec![part_number])
        .created_after(now - chrono::Duration::minutes(5))
        .created_before(now + chrono::Duration::minutes(5))
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_units_exclude_units_with_parent() {
    let (_, parent_serial) = create_part_and_unit("EP").await;
    let (_, child_serial) = create_part_and_unit("EC").await;

    client().units().add_child()
        .serial_number(&parent_serial)
        .child_serial_number(&child_serial)
        .send()
        .await
        .unwrap();

    // With exclude - child should not appear
    let excluded = client().units().list()
        .serial_numbers(vec![parent_serial.clone(), child_serial.clone()])
        .exclude_units_with_parent(true)
        .send()
        .await
        .unwrap();

    assert!(excluded.data.iter().all(|u| u.serial_number == parent_serial));

    // Without exclude - both should appear
    let included = client().units().list()
        .serial_numbers(vec![parent_serial, child_serial])
        .exclude_units_with_parent(false)
        .send()
        .await
        .unwrap();

    assert_eq!(2, included.data.len());
}
