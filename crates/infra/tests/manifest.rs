use archive_domain::*;
use archive_infra::DiskPlan;
use archive_sync::*;
use std::{path::Path, time::SystemTime};
fn remote(key: &str) -> Location {
    Location::Remote(ObjectId::parse_s3(&format!("s3://bucket/{key}")).unwrap())
}
fn entry(source: Location, destination: Location) -> ResolvedCopy {
    let mut meta = ObjectMetadata::file(source.clone(), 7);
    meta.queried_at = Some(SystemTime::now());
    ResolvedCopy {
        route: CopyRoute::new(source, destination).unwrap(),
        source_metadata: meta,
        destination_observation: DestinationObservation::Missing,
    }
}
#[tokio::test]
async fn manifest_is_sealed_before_reading_and_pages_through_disk_records() {
    let cancel = CancelContext::new();
    let mut plan = DiskPlan::new().await.unwrap();
    assert!(plan.next(&cancel).await.is_err());
    for n in 0..600 {
        plan.append(
            entry(
                remote(&format!("source/{n}")),
                remote(&format!("destination/{n}")),
            ),
            &cancel,
        )
        .await
        .unwrap();
    }
    plan.seal(&cancel).await.unwrap();
    assert!(
        plan.append(entry(remote("late"), remote("late")), &cancel)
            .await
            .is_err()
    );
    for n in 0..600 {
        let row = plan.next(&cancel).await.unwrap().unwrap();
        assert_eq!(row.route.source(), remote(&format!("source/{n}")));
    }
    assert!(plan.next(&cancel).await.unwrap().is_none());
    plan.discard().await.unwrap();
    plan.discard().await.unwrap();
    assert!(plan.next(&cancel).await.is_err());
}
#[tokio::test]
async fn manifest_preserves_non_utf8_local_paths_and_rejects_duplicate_remote_destinations() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tempfile::tempdir().unwrap();
    let source = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"source-\xff".to_vec()));
    let destination = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"destination-\xfe".to_vec()));
    let cancel = CancelContext::new();
    let mut plan = DiskPlan::new().await.unwrap();
    plan.append(
        entry(Location::Local(source.clone()), remote("uploaded")),
        &cancel,
    )
    .await
    .unwrap();
    let record = entry(remote("source"), Location::Local(destination.clone()));
    let roundtrip: ResolvedCopy =
        serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
    assert_eq!(
        roundtrip.route.destination(),
        Location::Local(destination.clone())
    );
    let supported = match std::fs::File::create(&destination) {
        Ok(_) => {
            std::fs::remove_file(&destination).unwrap();
            true
        }
        Err(_) => false,
    };
    let result = plan.append(record, &cancel).await;
    assert_eq!(result.is_ok(), supported);
    plan.seal(&cancel).await.unwrap();
    assert_eq!(
        plan.next(&cancel).await.unwrap().unwrap().route.source(),
        Location::Local(source)
    );
    if supported {
        assert_eq!(
            plan.next(&cancel)
                .await
                .unwrap()
                .unwrap()
                .route
                .destination(),
            Location::Local(destination)
        );
    }
    plan.discard().await.unwrap();
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    let mut plan = DiskPlan::new().await.unwrap();
    plan.append(entry(remote("a"), remote("same")), &cancel)
        .await
        .unwrap();
    assert_eq!(
        plan.append(entry(remote("b"), remote("same")), &cancel)
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::Input
    );
    plan.discard().await.unwrap();
}
#[tokio::test]
async fn manifest_detects_overlapping_paths_and_real_volume_unicode_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    let mut plan = DiskPlan::new().await.unwrap();
    plan.append(
        entry(remote("a"), Location::Local(temp.path().join("new/file"))),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(
        plan.append(
            entry(
                remote("b"),
                Location::Local(temp.path().join("new/file/child"))
            ),
            &cancel
        )
        .await
        .err()
        .unwrap()
        .kind,
        ErrorKind::Input
    );
    plan.discard().await.unwrap();
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    let composed = temp.path().join("é");
    let decomposed = temp.path().join("e\u{301}");
    std::fs::write(&composed, b"").unwrap();
    let aliases = decomposed.exists();
    std::fs::remove_file(&composed).unwrap();
    let mut plan = DiskPlan::new().await.unwrap();
    plan.append(entry(remote("a"), Location::Local(composed)), &cancel)
        .await
        .unwrap();
    let second = plan
        .append(entry(remote("b"), Location::Local(decomposed)), &cancel)
        .await;
    assert_eq!(second.is_err(), aliases);
    plan.discard().await.unwrap();
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
#[tokio::test]
async fn manifest_cancel_before_append_makes_no_destination_changes() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancelContext::new();
    cancel.cancel();
    let mut plan = DiskPlan::new().await.unwrap();
    assert_eq!(
        plan.append(
            entry(
                remote("source"),
                Location::Local(temp.path().join("output"))
            ),
            &cancel
        )
        .await
        .err()
        .unwrap()
        .kind,
        ErrorKind::Cancelled
    );
    plan.discard().await.unwrap();
    assert!(!Path::new(&temp.path().join("output")).exists());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
