use super::*;
use std::sync::Mutex;

fn volumes(count: usize) -> Vec<LocalVolume> {
    (0..count)
        .map(|index| LocalVolume {
            id: format!("vol-{index}"),
            path: format!("/mnt/vol-{index}"),
            weight: 1,
        })
        .collect()
}

/// An in-memory volume that can be told to fail every write.
#[derive(Debug, Default)]
struct SimVolume {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    failing: bool,
    id: String,
}

impl CacheVolume for SimVolume {
    fn id(&self) -> &str {
        &self.id
    }

    fn read_blob(&self, relative_path: &str) -> Result<Option<Vec<u8>>, PlacementError> {
        Ok(self.blobs.lock().unwrap().get(relative_path).cloned())
    }

    fn write_blob(&self, relative_path: &str, bytes: &[u8]) -> Result<(), PlacementError> {
        if self.failing {
            return Err(PlacementError::VolumeIo {
                detail: "injected volume failure".to_owned(),
                volume_id: self.id.clone(),
            });
        }
        self.blobs
            .lock()
            .unwrap()
            .insert(relative_path.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn delete_blob(&self, relative_path: &str) {
        self.blobs.lock().unwrap().remove(relative_path);
    }

    fn clear_directory(&self, _relative_dir: &str) {
        self.blobs.lock().unwrap().clear();
    }
}

fn sim_volumes(count: usize, failing: Option<usize>) -> BTreeMap<String, Box<dyn CacheVolume>> {
    (0..count)
        .map(|index| {
            let volume = SimVolume {
                failing: failing == Some(index),
                id: format!("vol-{index}"),
                ..SimVolume::default()
            };
            (volume.id.clone(), Box::new(volume) as Box<dyn CacheVolume>)
        })
        .collect()
}

fn total_blobs(volumes: &BTreeMap<String, Box<dyn CacheVolume>>, plan: &PlacementPlan) -> usize {
    plan.writes
        .iter()
        .filter(|write| {
            volumes[&write.volume_id]
                .read_blob(&write.relative_path)
                .unwrap()
                .is_some()
        })
        .count()
}

#[test]
fn single_layout_spreads_blocks_over_every_configured_volume() {
    let policy = PlacementPolicy::new(LocalLayout::Single, volumes(3)).unwrap();
    let mut used: HashSet<String> = HashSet::new();
    for index in 0..64 {
        let plan = policy.plan(&format!("block-{index}"));
        assert_eq!(plan.writes.len(), 1);
        used.insert(plan.writes[0].volume_id.clone());
    }
    assert_eq!(used.len(), 3, "equally weighted volumes must all take work");
    assert_eq!(
        policy.plan("block-7"),
        policy.plan("block-7"),
        "planning is deterministic"
    );
}

#[test]
fn single_layout_places_in_proportion_to_the_configured_weights() {
    let weighted = vec![
        LocalVolume {
            id: "vol-0".to_owned(),
            path: "/mnt/vol-0".to_owned(),
            weight: 0,
        },
        LocalVolume {
            id: "vol-1".to_owned(),
            path: "/mnt/vol-1".to_owned(),
            weight: 3,
        },
    ];
    let policy = PlacementPolicy::new(LocalLayout::Single, weighted).unwrap();
    for index in 0..64 {
        assert_eq!(policy.plan(&format!("block-{index}")).writes[0].volume_id, "vol-1");
    }

    let unweighted: Vec<LocalVolume> = volumes(2)
        .into_iter()
        .map(|volume| LocalVolume { weight: 0, ..volume })
        .collect();
    let policy = PlacementPolicy::new(LocalLayout::Single, unweighted).unwrap();
    assert_eq!(policy.plan("block-0").writes[0].volume_id, "vol-0");
}

#[test]
fn mirror_and_stripe_require_two_distinct_volumes_at_startup() {
    assert!(matches!(
        PlacementPolicy::new(LocalLayout::Mirror, volumes(1)),
        Err(PlacementError::InsufficientVolumes { .. })
    ));
    assert!(matches!(
        PlacementPolicy::new(LocalLayout::Stripe, volumes(1)),
        Err(PlacementError::InsufficientVolumes { .. })
    ));
    let mut duplicated = volumes(2);
    duplicated[1].id = "vol-0".to_owned();
    assert!(matches!(
        PlacementPolicy::new(LocalLayout::Mirror, duplicated),
        Err(PlacementError::DuplicateVolume { .. })
    ));
    assert_eq!(
        PlacementPolicy::new(LocalLayout::Single, Vec::new()).unwrap_err(),
        PlacementError::NoVolumes
    );
    assert!(PlacementPolicy::new(LocalLayout::Single, volumes(1)).is_ok());
    assert!(PlacementPolicy::new(LocalLayout::Mirror, volumes(2)).is_ok());
}

#[test]
fn mirror_rejects_two_volume_paths_that_resolve_to_the_same_physical_directory() {
    let dir = tempfile::TempDir::new().unwrap();
    let real = dir.path().to_str().unwrap().to_owned();
    let aliased = vec![
        LocalVolume {
            id: "vol-0".to_owned(),
            path: real.clone(),
            weight: 1,
        },
        LocalVolume {
            id: "vol-1".to_owned(),
            path: format!("{real}/."),
            weight: 1,
        },
    ];
    assert!(matches!(
        PlacementPolicy::new(LocalLayout::Mirror, aliased),
        Err(PlacementError::DuplicateVolume { .. })
    ));
}

#[test]
fn a_mirror_copy_shortfall_fails_and_rolls_back_the_copy_that_landed() {
    let policy = PlacementPolicy::new(LocalLayout::Mirror, volumes(2)).unwrap();
    let plan = policy.plan("block-1");
    let sims = sim_volumes(2, Some(1));
    assert_eq!(
        stage_local_copies(&policy, &plan, b"bytes", &sims),
        Err(PlacementError::MirrorCopiesNotMet {
            achieved: 1,
            required: 2
        })
    );
    assert_eq!(
        total_blobs(&sims, &plan),
        0,
        "a failed placement leaves no partial copies behind"
    );
}

#[test]
fn stripe_chunks_cover_the_bytes_and_a_lost_chunk_fails_and_rolls_back() {
    let policy = PlacementPolicy::new(LocalLayout::Stripe, volumes(3)).unwrap();
    let plan = policy.plan("block-0");
    let sims = sim_volumes(3, None);
    let bytes: Vec<u8> = (0..10u8).collect();
    stage_local_copies(&policy, &plan, &bytes, &sims).unwrap();
    let mut reassembled = Vec::new();
    for write in &plan.writes {
        reassembled.extend(sims[&write.volume_id].read_blob(&write.relative_path).unwrap().unwrap());
    }
    assert_eq!(reassembled, bytes);

    let failing = sim_volumes(3, Some(2));
    assert_eq!(
        stage_local_copies(&policy, &plan, &bytes, &failing),
        Err(PlacementError::StripeWriteFailed { stripe_index: 2 })
    );
    assert_eq!(total_blobs(&failing, &plan), 0);
}
