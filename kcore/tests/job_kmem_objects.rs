//! MM5: an object's body is charged to its creator's kernel bytes, with the object, and credited when the object is destroyed.
use abi::status::Status;
use kcore::handle::HandleTable;
use kcore::ipc::{self, Kobjs, Quota, OBJECT_BYTES};
use kcore::job::{JobId, Jobs, Limits, Resource};

fn used(jobs: &Jobs, job: JobId) -> u64 {
    jobs.usage(job).unwrap().get(Resource::KernelBytes)
}

#[test]
fn object_bodies_are_charged_refused_at_the_derived_limit_and_credited_back() {
    let mut reg = Kobjs::new();
    let mut table = HandleTable::new(4000);
    let mut jobs = Jobs::new();
    // pages 4: a kernel-byte limit of 4 * 512 bytes
    let mut l = Limits::UNLIMITED;
    l.pages = 4;
    let j = jobs.create(JobId::ROOT, l).unwrap();
    let cap = 4 * 512 / OBJECT_BYTES; // objects the limit admits
    assert!(
        cap >= 2,
        "the limit admits a few objects ({cap} of {OBJECT_BYTES} bytes)"
    );
    let mut made = vec![];
    loop {
        let mut q = Quota {
            jobs: &mut jobs,
            job: Some(j),
        };
        match ipc::channel_create_in(&mut reg, &mut table, &mut q) {
            Ok(pair) => made.push(pair),
            Err(e) => {
                assert_eq!(e, Status::NoMemory);
                break;
            }
        }
        assert!(made.len() < 1000);
    }
    // a channel is two objects
    assert_eq!(
        made.len() as u64 * 2,
        cap / 2 * 2,
        "refused exactly at the limit"
    );
    assert_eq!(used(&jobs, j), made.len() as u64 * 2 * OBJECT_BYTES);
    assert!(used(&jobs, j) <= 4 * 512);
    assert_eq!(
        used(&jobs, JobId::ROOT),
        used(&jobs, j),
        "the parent sees it"
    );
    for (a, b) in made {
        table.close(&mut reg, a).unwrap();
        table.close(&mut reg, b).unwrap();
    }
    ipc::settle(&mut reg, &mut jobs);
    assert_eq!(used(&jobs, j), 0, "everything comes back");
    assert_eq!(used(&jobs, JobId::ROOT), 0);
}
