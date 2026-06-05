mod common;
use common::*;

#[tokio::test]
async fn run_attachments_create() {
    let run = create_test_run(&uid()).await;

    let dir = std::env::temp_dir().join(format!("tofupilot-{}", uid()));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("test.txt");
    tokio::fs::write(&path, "attach helper test").await.unwrap();

    let id = client().runs().attachments().upload(&run.id, &path).await.unwrap();
    assert!(!id.is_empty());

    let fetched = client().runs().get().id(&run.id).send().await.unwrap();
    let attachments = fetched.attachments.unwrap_or_default();
    assert!(attachments.iter().any(|a| a.id == id));

    tokio::fs::remove_dir_all(&dir).await.ok();
}

#[tokio::test]
async fn run_attachments_create_file_not_found() {
    let result = client().runs().attachments().upload("fake-id", "/nonexistent/file.txt").await;
    assert!(matches!(result, Err(tofupilot::Error::Io(_))));
}

#[tokio::test]
async fn unit_attachments_create_and_delete() {
    let uid_val = uid();
    let part = format!("PART-DAT-{uid_val}");
    let serial = format!("SN-DAT-{uid_val}");
    let rev = format!("REV-DAT-{uid_val}");

    client().parts().create().number(&part).name(format!("Part {uid_val}")).send().await.unwrap();
    client().revisions().create().part_number(&part).number(&rev).send().await.unwrap();
    client().units().create().serial_number(&serial).part_number(&part).revision_number(&rev).send().await.unwrap();

    let dir = std::env::temp_dir().join(format!("tofupilot-{uid_val}"));
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let path = dir.join("delete-test.txt");
    tokio::fs::write(&path, "to be deleted").await.unwrap();

    let id = client().units().attachments().upload(&serial, &path).await.unwrap();
    assert!(!id.is_empty());

    let result = client().units().attachments().delete(&serial, vec![id.clone()]).await.unwrap();
    assert!(result.ids.iter().any(|i| i == &id));

    let fetched = client().units().get().serial_number(&serial).send().await.unwrap();
    let attachments = fetched.attachments.unwrap_or_default();
    assert!(!attachments.iter().any(|a| a.id == id));

    tokio::fs::remove_dir_all(&dir).await.ok();
}
