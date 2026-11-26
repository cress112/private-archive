use archive_domain::*;
use archive_infra::DiskObjects;
use archive_sync::*;
fn object(key: &str, size: Option<u64>) -> PlannedObject {
    let id = ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap();
    let mut metadata = ObjectMetadata::file(Location::Remote(id), 1);
    metadata.size = size;
    PlannedObject {
        metadata,
        full_observation: false,
    }
}
#[tokio::test]
async fn disk_plan_orders_all_objects_and_deduplicates_first_uri_without_using_index() {
    let cancel = CancelContext::new();
    let mut plan = DiskObjects::new().await.unwrap();
    assert!(plan.next(&cancel).await.is_err());
    for n in (0..600).rev() {
        assert!(
            plan.append(0, object(&format!("p/{n:04}"), Some(1)), &cancel)
                .await
                .unwrap()
        );
    }
    assert!(
        !plan
            .append(1, object("p/0000", None), &cancel)
            .await
            .unwrap()
    );
    assert!(
        plan.append(1, object("a/later-uri", None), &cancel)
            .await
            .unwrap()
    );
    let summary = plan.seal(&cancel).await.unwrap();
    assert_eq!(summary.objects, 601);
    assert_eq!(summary.total_bytes, None);
    for n in 0..600 {
        assert_eq!(
            plan.next(&cancel)
                .await
                .unwrap()
                .unwrap()
                .metadata
                .location
                .display(),
            format!("s3://bucket/p/{n:04}")
        );
    }
    assert_eq!(
        plan.next(&cancel)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .location
            .display(),
        "s3://bucket/a/later-uri"
    );
    assert!(plan.next(&cancel).await.unwrap().is_none());
    let changed = object("p/0000", Some(9));
    let Location::Remote(id) = changed.metadata.location.clone() else {
        panic!()
    };
    plan.replace(&id, changed, &cancel).await.unwrap();
    plan.rewind(&cancel).await.unwrap();
    assert_eq!(
        plan.next(&cancel).await.unwrap().unwrap().metadata.size,
        Some(9)
    );
    assert!(
        plan.append(2, object("rejected", Some(1)), &cancel)
            .await
            .is_err()
    );
    plan.discard().await.unwrap();
    assert!(plan.next(&cancel).await.is_err());
}
#[tokio::test]
async fn oversized_records_and_cancelled_seal_fail_before_any_mutation() {
    let mut plan = DiskObjects::new().await.unwrap();
    let cancel = CancelContext::new();
    assert!(
        plan.append(0, object(&"x".repeat(128 * 1024), Some(1)), &cancel)
            .await
            .is_err()
    );
    plan.append(0, object("valid", Some(1)), &cancel)
        .await
        .unwrap();
    cancel.cancel();
    assert_eq!(
        plan.seal(&cancel).await.err().unwrap().kind,
        ErrorKind::Cancelled
    );
    plan.discard().await.unwrap();
}
