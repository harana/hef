use super::*;
use crate::writer::build::{BuildRow, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};
use hashbrown::HashMap;

/// The in-memory stand-in for the application's durable external-id table.
#[derive(Default)]
struct MemoryStore {
    records: HashMap<(TenantId, Vec<u8>), ExternalIdLocation>,
}

impl ExternalIdStore for MemoryStore {
    fn get(&self, tenant_id: TenantId, external_id: &[u8]) -> Result<Option<ExternalIdLocation>, StorageError> {
        Ok(self.records.get(&(tenant_id, external_id.to_vec())).copied())
    }

    fn put(
        &mut self,
        tenant_id: TenantId,
        external_id: &[u8],
        location: ExternalIdLocation,
    ) -> Result<(), StorageError> {
        self.records.insert((tenant_id, external_id.to_vec()), location);
        Ok(())
    }
}

/// A sealed file of generation `generation` whose rows carry `ids` (row `i` carries `ids[i]`).
fn file_with_ids(generation: u64, ids: &[&[u8]]) -> HefFile {
    let rows: Vec<BuildRow> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut row = plain_row(i as u64);
            row.external_id = Some(id.to_vec());
            row
        })
        .collect();
    let mut config = source_form_config(2);
    config.generation_id = generation;
    let built = build_hef_file(rows, &config).unwrap();
    HefFile::open(built.bytes, Some(&built.file_seal)).unwrap()
}

#[test]
fn a_recorded_id_is_found_and_a_missing_one_is_not() {
    let long_id = [b'm'; 64];
    let file = file_with_ids(1, &[&b"$a"[..], &long_id[..], &b"$c"[..]]);
    let mut store = MemoryStore::default();
    assert_eq!(record_file(&mut store, &file).unwrap(), 3);
    let tenant = file.header().tenant_id;
    assert_eq!(
        store.get(tenant, &long_id).unwrap(),
        Some(ExternalIdLocation {
            file_id: file.header().file_id,
            generation: 1,
            row_ordinal: 1,
        })
    );
    assert_eq!(store.get(tenant, b"$missing").unwrap(), None);
}

#[test]
fn the_newest_generation_wins_whatever_order_files_are_recorded_in() {
    let older = file_with_ids(3, &[&b"$shared"[..], &b"$old-only"[..]]);
    let newer = file_with_ids(4, &[&b"$new-only"[..], &b"$x"[..], &b"$shared"[..]]);
    for order in [[&older, &newer], [&newer, &older]] {
        let mut store = MemoryStore::default();
        for file in order {
            record_file(&mut store, file).unwrap();
        }
        let tenant = newer.header().tenant_id;
        let shared = store.get(tenant, b"$shared").unwrap().expect("recorded");
        assert_eq!(shared.generation, 4);
        assert_eq!(shared.file_id, newer.header().file_id);
        assert_eq!(shared.row_ordinal, 2);
        assert_eq!(
            store
                .get(tenant, b"$old-only")
                .unwrap()
                .map(|location| location.generation),
            Some(3)
        );
    }
}
