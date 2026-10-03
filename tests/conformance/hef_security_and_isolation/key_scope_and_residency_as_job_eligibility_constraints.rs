//! Checks that background jobs (compaction, projection builds, index rebuilds) are only scheduled on nodes that hold a
//! valid lease for the key scope the job requires, in the residency region the job must run in. A node lacking the
//! required tenant-DEK lease, or holding it for a different region, is ineligible so no job output derived from
//! encrypted blocks is ever produced without a valid key or outside the tenant's residency.

use hef::events::TenantId;
use hef::security::{KeyLease, KeyResidencyRegion, KeyScope, is_eligible_for_job};
use hef::typed_id::TypedIdTestExt;

/// conformance:
/// hef-security-and-isolation/key-scope-and-residency-as-job-eligibility-constraints/
/// job-without-key-lease-is-ineligible
#[test]
fn job_without_key_lease_is_ineligible() {
    let tenant = TenantId::new_test_id(42);
    let other_tenant = TenantId::new_test_id(99);
    let region = KeyResidencyRegion("eu-west".to_owned());

    // A node with no leases at all is ineligible for any job that requires a key scope.
    assert!(
        !is_eligible_for_job(tenant, KeyScope::TenantDek, &region, &[]),
        "a node with no leases must be ineligible for a TenantDek-scoped job"
    );
    assert!(
        !is_eligible_for_job(tenant, KeyScope::SubjectKeys, &region, &[]),
        "a node with no leases must be ineligible for a SubjectKeys-scoped job"
    );

    // A node holding a lease for the wrong tenant is still ineligible.
    let wrong_tenant_lease = KeyLease {
        residency: region.clone(),
        scope: KeyScope::TenantDek,
        tenant_id: other_tenant,
    };
    assert!(
        !is_eligible_for_job(tenant, KeyScope::TenantDek, &region, &[wrong_tenant_lease]),
        "a lease for a different tenant must not satisfy the job's key requirement"
    );

    // A node holding the correct tenant DEK lease for the job's region is eligible for a TenantDek job.
    let correct_lease = KeyLease {
        residency: region.clone(),
        scope: KeyScope::TenantDek,
        tenant_id: tenant,
    };
    assert!(
        is_eligible_for_job(tenant, KeyScope::TenantDek, &region, &[correct_lease]),
        "a node with the correct tenant-DEK lease must be eligible for the job"
    );

    // A TenantDek lease does not satisfy a SubjectKeys job requirement — the scopes are distinct and the scheduler must
    // not treat them as equivalent.
    let dek_lease = KeyLease {
        residency: region.clone(),
        scope: KeyScope::TenantDek,
        tenant_id: tenant,
    };
    assert!(
        !is_eligible_for_job(tenant, KeyScope::SubjectKeys, &region, &[dek_lease]),
        "a TenantDek lease must not satisfy a SubjectKeys job requirement"
    );

    // A job that requires no key scope (KeyScope::None) reads no encrypted blocks, so it carries no residency
    // constraint and is eligible on any node regardless of what leases it holds.
    assert!(
        is_eligible_for_job(tenant, KeyScope::None, &region, &[]),
        "a KeyScope::None job must be eligible on a node with no leases"
    );
}

/// conformance:
/// hef-security-and-isolation/key-scope-and-residency-as-job-eligibility-constraints/
/// job-in-the-wrong-region-is-ineligible
#[test]
fn job_in_the_wrong_region_is_ineligible() {
    // A key leased for eu-west may not be used to process a tenant's data in us-east: the lease carries its region, and
    // eligibility requires the required-scope lease to be for the job's region, so a tenant's keys never leave their
    // jurisdiction even when the node holds a valid lease for the right scope and tenant.
    let tenant = TenantId::new_test_id(42);
    let eu_west = KeyResidencyRegion("eu-west".to_owned());
    let us_east = KeyResidencyRegion("us-east".to_owned());

    let dek_lease = KeyLease {
        residency: eu_west.clone(),
        scope: KeyScope::TenantDek,
        tenant_id: tenant,
    };
    let subject_lease = KeyLease {
        residency: eu_west.clone(),
        scope: KeyScope::SubjectKeys,
        tenant_id: tenant,
    };
    let node_leases = [dek_lease, subject_lease];

    // In the region the leases are for, the node is eligible for both scopes.
    assert!(
        is_eligible_for_job(tenant, KeyScope::TenantDek, &eu_west, &node_leases),
        "the node holds the eu-west tenant-DEK lease and is eligible for a eu-west job"
    );
    assert!(
        is_eligible_for_job(tenant, KeyScope::SubjectKeys, &eu_west, &node_leases),
        "the node holds both eu-west leases and is eligible for a eu-west SubjectKeys job"
    );

    // For a job that must run in us-east, the eu-west leases do not satisfy the region constraint.
    assert!(
        !is_eligible_for_job(tenant, KeyScope::TenantDek, &us_east, &node_leases),
        "a eu-west tenant-DEK lease must not satisfy a us-east job"
    );
    assert!(
        !is_eligible_for_job(tenant, KeyScope::SubjectKeys, &us_east, &node_leases),
        "eu-west leases must not satisfy a us-east SubjectKeys job"
    );
}
