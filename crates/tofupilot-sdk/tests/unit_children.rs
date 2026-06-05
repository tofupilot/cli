mod common;
use common::*;

async fn create_unit(prefix: &str) -> String {
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

    serial
}

#[tokio::test]
async fn add_child_success() {
    let parent = create_unit("ACHP").await;
    let child = create_unit("ACHC").await;

    client().units().add_child()
        .serial_number(&parent)
        .child_serial_number(&child)
        .send()
        .await
        .unwrap();

    let parent_unit = client().units().get()
        .serial_number(&parent)
        .send()
        .await
        .unwrap();

    let children = parent_unit.children.expect("children should be present");
    assert!(children.iter().any(|c| c.serial_number == child));

    let child_unit = client().units().get()
        .serial_number(&child)
        .send()
        .await
        .unwrap();

    let parent_ref = child_unit.parent.expect("parent should be present");
    assert_eq!(parent, parent_ref.serial_number);
}

#[tokio::test]
async fn add_multiple_children() {
    let parent = create_unit("AMCP").await;
    let mut children = Vec::new();
    for i in 0..3 {
        children.push(create_unit(&format!("AMC{i}")).await);
    }

    for child in &children {
        client().units().add_child()
            .serial_number(&parent)
            .child_serial_number(child)
            .send()
            .await
            .unwrap();
    }

    let parent_unit = client().units().get()
        .serial_number(&parent)
        .send()
        .await
        .unwrap();

    let fetched_children = parent_unit.children.expect("children should be present");
    assert_eq!(3, fetched_children.len());
}

#[tokio::test]
async fn remove_child_success() {
    let parent = create_unit("RMCP").await;
    let child = create_unit("RMCC").await;

    client().units().add_child()
        .serial_number(&parent)
        .child_serial_number(&child)
        .send()
        .await
        .unwrap();

    client().units().remove_child()
        .serial_number(&parent)
        .child_serial_number(&child)
        .send()
        .await
        .unwrap();

    let parent_unit = client().units().get()
        .serial_number(&parent)
        .send()
        .await
        .unwrap();

    let has_child = parent_unit.children
        .as_ref()
        .map(|c| c.iter().any(|ch| ch.serial_number == child))
        .unwrap_or(false);
    assert!(!has_child);
}

#[tokio::test]
async fn remove_child_from_multiple() {
    let parent = create_unit("RFMP").await;
    let mut children = Vec::new();
    for i in 0..3 {
        let child = create_unit(&format!("RFM{i}")).await;
        client().units().add_child()
            .serial_number(&parent)
            .child_serial_number(&child)
            .send()
            .await
            .unwrap();
        children.push(child);
    }

    client().units().remove_child()
        .serial_number(&parent)
        .child_serial_number(&children[1])
        .send()
        .await
        .unwrap();

    let parent_unit = client().units().get()
        .serial_number(&parent)
        .send()
        .await
        .unwrap();

    let fetched_children = parent_unit.children.expect("children should be present");
    assert_eq!(2, fetched_children.len());
    assert!(!fetched_children.iter().any(|c| c.serial_number == children[1]));
}

#[tokio::test]
async fn add_child_self_reference_fails() {
    let unit = create_unit("SELF").await;
    let result = client().units().add_child()
        .serial_number(&unit)
        .child_serial_number(&unit)
        .send()
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn add_child_cycle_detection() {
    let a = create_unit("CYCA").await;
    let b = create_unit("CYCB").await;

    client().units().add_child()
        .serial_number(&a)
        .child_serial_number(&b)
        .send()
        .await
        .unwrap();

    let result = client().units().add_child()
        .serial_number(&b)
        .child_serial_number(&a)
        .send()
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn add_child_parent_not_found() {
    let child = create_unit("ACPNF").await;
    let result = client().units().add_child()
        .serial_number(format!("NONEXISTENT-{}", uid()))
        .child_serial_number(&child)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn add_child_child_not_found() {
    let parent = create_unit("ACCNF").await;
    let result = client().units().add_child()
        .serial_number(&parent)
        .child_serial_number(format!("NONEXISTENT-{}", uid()))
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}

#[tokio::test]
async fn exclude_units_with_parent() {
    let parent = create_unit("EXWP").await;
    let child = create_unit("EXWC").await;

    client().units().add_child()
        .serial_number(&parent)
        .child_serial_number(&child)
        .send()
        .await
        .unwrap();

    let result = client().units().list()
        .serial_numbers(vec![parent.clone(), child.clone()])
        .exclude_units_with_parent(true)
        .send()
        .await
        .unwrap();

    assert!(result.data.iter().all(|u| u.serial_number != child));
    assert!(result.data.iter().any(|u| u.serial_number == parent));
}

#[tokio::test]
async fn remove_child_not_actually_child_fails() {
    let parent = create_unit("RNAC_P").await;
    let other = create_unit("RNAC_O").await;

    // other is NOT a child of parent
    let result = client().units().remove_child()
        .serial_number(&parent)
        .child_serial_number(&other)
        .send()
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn remove_child_parent_not_found() {
    let child = create_unit("RCPNF").await;
    let result = client().units().remove_child()
        .serial_number(format!("NONEXISTENT-{}", uid()))
        .child_serial_number(&child)
        .send()
        .await;
    assert!(matches!(result, Err(tofupilot::Error::NotFound(_))));
}
