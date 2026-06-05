mod common;
use common::*;
use tofupilot::types::*;

#[tokio::test]
async fn list_runs_returns_data() {
    let uid_val = uid();
    create_test_run(&uid_val).await;

    let result = client().runs().list()
        .part_numbers(vec![format!("PART-{uid_val}")])
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_outcome() {
    let uid_val = uid();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;
    let part = format!("PART-OUT-{uid_val}");

    let pass_run = client().runs().create()
        .serial_number(format!("SN-P-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    client().runs().create()
        .serial_number(format!("SN-F-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Fail)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .outcomes(vec![Outcome::Pass])
        .part_numbers(vec![part])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
    assert!(result.data.iter().all(|r| r.outcome == Outcome::Pass));
    assert!(result.data.iter().any(|r| r.id == pass_run.id));
}

#[tokio::test]
async fn list_runs_filter_by_serial_number() {
    let uid_val = uid();
    let serial = format!("SN-FILT-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    client().runs().create()
        .serial_number(&serial)
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .serial_numbers(vec![serial.clone()])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_pagination() {
    let uid_val = uid();
    let part = format!("PART-PAGE-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    for i in 0..3 {
        client().runs().create()
            .serial_number(format!("SN-PG-{i}-{uid_val}"))
            .procedure_id(proc_id)
            .part_number(&part)
            .started_at(now - chrono::Duration::minutes(5))
            .ended_at(now)
            .outcome(Outcome::Pass)
            .send()
            .await
            .unwrap();
    }

    let page1 = client().runs().list()
        .part_numbers(vec![part.clone()])
        .limit(1)
        .send()
        .await
        .unwrap();

    assert_eq!(1, page1.data.len());
    assert!(page1.meta.has_more);

    let cursor = *page1.meta.next_cursor.as_ref().unwrap();
    let page2 = client().runs().list()
        .part_numbers(vec![part])
        .limit(1)
        .cursor(cursor)
        .send()
        .await
        .unwrap();

    assert_eq!(1, page2.data.len());
    assert_ne!(page1.data[0].id, page2.data[0].id);
}

#[tokio::test]
async fn list_runs_filter_by_procedure_id() {
    let uid_val = uid();
    let proc = client().procedures().create()
        .name(format!("Proc FPI {uid_val}"))
        .send()
        .await
        .unwrap();

    let now = chrono::Utc::now();
    client().runs().create()
        .serial_number(format!("SN-FPI-{uid_val}"))
        .procedure_id(&proc.id)
        .part_number(format!("PART-FPI-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .procedure_ids(vec![proc.id.clone()])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
    assert!(result.data.iter().all(|r| r.procedure.id == proc.id));
}

#[tokio::test]
async fn list_runs_filter_by_part_number() {
    let uid_val = uid();
    create_test_run(&uid_val).await;

    let result = client().runs().list()
        .part_numbers(vec![format!("PART-{uid_val}")])
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_date_range() {
    let uid_val = uid();
    let before = chrono::Utc::now();
    create_test_run(&uid_val).await;
    let after = chrono::Utc::now();

    let result = client().runs().list()
        .part_numbers(vec![format!("PART-{uid_val}")])
        .created_after(before)
        .created_before(after + chrono::Duration::seconds(1))
        .send()
        .await
        .unwrap();
    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_sort_order() {
    let uid_val = uid();
    let part = format!("PART-SORT-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    for i in 0..2 {
        client().runs().create()
            .serial_number(format!("SN-SORT-{i}-{uid_val}"))
            .procedure_id(proc_id)
            .part_number(&part)
            .started_at(now - chrono::Duration::minutes(10 - i))
            .ended_at(now - chrono::Duration::minutes(5 - i))
            .outcome(Outcome::Pass)
            .send()
            .await
            .unwrap();
    }

    let desc = client().runs().list()
        .part_numbers(vec![part.clone()])
        .sort_by(RunListSortBy::StartedAt)
        .sort_order(ListSortOrder::Desc)
        .send()
        .await
        .unwrap();

    let asc = client().runs().list()
        .part_numbers(vec![part])
        .sort_by(RunListSortBy::StartedAt)
        .sort_order(ListSortOrder::Asc)
        .send()
        .await
        .unwrap();

    if desc.data.len() >= 2 {
        assert!(desc.data[0].started_at >= desc.data[1].started_at);
    }
    if asc.data.len() >= 2 {
        assert!(asc.data[0].started_at <= asc.data[1].started_at);
    }
}

#[tokio::test]
async fn list_runs_filter_by_ids() {
    let run1 = create_test_run(&uid()).await;
    let run2 = create_test_run(&uid()).await;

    let result = client().runs().list()
        .ids(vec![run1.id.clone(), run2.id.clone()])
        .send()
        .await
        .unwrap();

    assert_eq!(2, result.data.len());
    assert!(result.data.iter().any(|r| r.id == run1.id));
    assert!(result.data.iter().any(|r| r.id == run2.id));
}

#[tokio::test]
async fn list_runs_empty_result() {
    let result = client().runs().list()
        .serial_numbers(vec![format!("NONEXISTENT-{}", uid())])
        .send()
        .await
        .unwrap();
    assert!(result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_duration_range() {
    let uid_val = uid();
    let part = format!("PART-DUR-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    // Create a run with ~2 minute duration
    client().runs().create()
        .serial_number(format!("SN-DUR-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now - chrono::Duration::minutes(3))
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .part_numbers(vec![part])
        .duration_min("PT1M")
        .duration_max("PT5M")
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_ended_at() {
    let uid_val = uid();
    let part = format!("PART-END-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    client().runs().create()
        .serial_number(format!("SN-END-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now - chrono::Duration::minutes(3))
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .part_numbers(vec![part])
        .ended_after(now - chrono::Duration::minutes(10))
        .ended_before(now + chrono::Duration::minutes(5))
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_created_at() {
    let uid_val = uid();
    let part = format!("PART-CRT-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    client().runs().create()
        .serial_number(format!("SN-CRT-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .part_numbers(vec![part])
        .created_after(now - chrono::Duration::minutes(10))
        .created_before(now + chrono::Duration::minutes(5))
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_revision_numbers() {
    let uid_val = uid();
    let part = format!("PART-REV-{uid_val}");
    let rev = format!("REV-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    client().parts().create()
        .number(&part)
        .name(format!("Part {uid_val}"))
        .send()
        .await
        .unwrap();

    client().revisions().create()
        .part_number(&part)
        .number(&rev)
        .send()
        .await
        .unwrap();

    client().runs().create()
        .serial_number(format!("SN-REV-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .revision_number(&rev)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .part_numbers(vec![part])
        .revision_numbers(vec![rev])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_procedure_versions() {
    let uid_val = uid();
    let part = format!("PART-PV-{uid_val}");
    let version = format!("1.0.{}", &uid_val[..4]);
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    client().runs().create()
        .serial_number(format!("SN-PV-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .procedure_version(&version)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .part_numbers(vec![part])
        .procedure_versions(vec![version])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}

#[tokio::test]
async fn list_runs_filter_by_batch_numbers() {
    let uid_val = uid();
    let part = format!("PART-BN-{uid_val}");
    let batch = format!("BATCH-{uid_val}");
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    client().runs().create()
        .serial_number(format!("SN-BN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(&part)
        .batch_number(&batch)
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .unwrap();

    let result = client().runs().list()
        .part_numbers(vec![part])
        .batch_numbers(vec![batch])
        .send()
        .await
        .unwrap();

    assert!(!result.data.is_empty());
}
