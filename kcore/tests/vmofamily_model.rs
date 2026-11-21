//! MM8: families of VMOs (SNAPSHOT and PRIVATE children) against a reference. Random create, write, snapshot, private child, release and
//! refused operations. The reference keeps the same tree of nodes, but its lookup is a few lines over plain vectors of bytes, so what is
//! checked is that the kernel's page handling (moving pages into the hidden node, copying on write, charging, freeing) gives each
//! member exactly the bytes the tree says (COW1), and that every frame is charged to exactly one job, to the job of the VMO that owns
//! it (COW2): after every step the jobs' page usage adds up to the frames in use, and a write that has to copy charges the writer's job
//! and nobody else. At the end everything is released and no frame, VMO or charge is left.
mod common;
use common::{FakeMem, Rng};
use kcore::aspace::{ChildMode, VmoError, Vmos, MAX_FAMILY_DEPTH};
use kcore::frame::FrameDb;
use kcore::job::{JobId, Jobs, Limits, Resource};
use kcore::Mm;

const PAGE: u64 = 4096;

struct Node {
    own: Vec<Option<u8>>,
    /// (node, offset)
    parent: Option<(usize, u64)>,
    job: usize,
    /// the real VMO, for the members a program can reach; hidden nodes have none
    id: Option<u64>,
    alive: bool,
}

struct Model {
    nodes: Vec<Node>,
}

impl Model {
    fn read(&self, mut n: usize, mut p: u64) -> u8 {
        loop {
            let node = &self.nodes[n];
            if let Some(Some(v)) = node.own.get(p as usize) {
                return *v;
            }
            match node.parent {
                Some((up, off)) => {
                    n = up;
                    p += off;
                }
                None => return 0,
            }
        }
    }

    fn depth(&self, mut n: usize) -> usize {
        let mut d = 0;
        while let Some((up, _)) = self.nodes[n].parent {
            n = up;
            d += 1;
        }
        d
    }
}

fn read_page(mm: &mut Mm<FakeMem>, vmos: &mut Vmos, id: u64, page: u64) -> u8 {
    let mut b = [0u8; 1];
    vmos.read(mm, id, page * PAGE, &mut b).unwrap();
    b[0]
}

#[test]
fn families_agree_with_the_reference_and_every_frame_is_charged_once() {
    for seed in 1..=60u64 {
        let mut rng = Rng::new(seed);
        let mut mm = Mm {
            db: FrameDb::new(3000, 500),
            mem: FakeMem::default(),
        };
        let total = mm.db.free_count();
        let mut vmos = Vmos::new();
        let mut jobs = Jobs::new();
        let job_ids: Vec<JobId> = (0..2)
            .map(|_| jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap())
            .collect();
        let mut m = Model { nodes: vec![] };
        let usage = |jobs: &Jobs, j: usize| jobs.usage(job_ids[j]).unwrap().get(Resource::Pages);
        // a few roots to start from
        for (j, &jid) in job_ids.iter().enumerate() {
            let pages = 2 + rng.below(6);
            let id = vmos.create_in(&mut jobs, pages, jid);
            m.nodes.push(Node {
                own: vec![None; pages as usize],
                parent: None,
                job: j,
                id: Some(id),
                alive: true,
            });
        }
        for step in 0..400 {
            let ctx = format!("seed {seed} step {step}");
            let alive: Vec<usize> = (0..m.nodes.len())
                .filter(|&i| m.nodes[i].alive && m.nodes[i].id.is_some())
                .collect();
            if alive.is_empty() {
                break;
            }
            // bias towards the newest member so that chains get deep
            let pick = |rng: &mut Rng| {
                if rng.below(3) == 0 {
                    *alive.last().unwrap()
                } else {
                    alive[rng.below(alive.len() as u64) as usize]
                }
            };
            let op = rng.below(10);
            if std::env::var("FAMTRACE").is_ok() {
                eprintln!("{ctx}: op {op} alive {alive:?}");
            }
            match op {
                0..=3 => {
                    // write a byte
                    let i = pick(&mut rng);
                    let id = m.nodes[i].id.unwrap();
                    let pages = m.nodes[i].own.len() as u64;
                    let p = rng.below(pages);
                    let v = 1 + rng.below(250) as u8;
                    let copies = m.nodes[i].own[p as usize].is_none();
                    let job = m.nodes[i].job;
                    let before = [usage(&jobs, 0), usage(&jobs, 1)];
                    vmos.write(&mut mm, &mut jobs, id, p * PAGE, &[v]).unwrap();
                    m.nodes[i].own[p as usize] = Some(v);
                    let mut want = before;
                    if copies {
                        want[job] += 1;
                    }
                    assert_eq!(
                        [usage(&jobs, 0), usage(&jobs, 1)],
                        want,
                        "{ctx}: a write that needs a page charges the writer's job, and only it"
                    );
                }
                4 | 5 => {
                    // a SNAPSHOT child, same job
                    let i = pick(&mut rng);
                    let id = m.nodes[i].id.unwrap();
                    let pages = m.nodes[i].own.len() as u64;
                    let off = rng.below(pages);
                    let len = 1 + rng.below(pages - off);
                    let job = m.nodes[i].job;
                    vmos.reserve_n(2).unwrap();
                    let r = vmos.create_child(
                        &mut jobs,
                        id,
                        off,
                        len,
                        job_ids[job],
                        ChildMode::Snapshot,
                    );
                    if m.depth(i) + 1 >= MAX_FAMILY_DEPTH {
                        assert_eq!(r, Err(VmoError::TooDeep), "{ctx}");
                    } else {
                        let child = r.unwrap();
                        // the hidden node takes the source's pages and its place under its parent
                        let h = m.nodes.len();
                        let own =
                            std::mem::replace(&mut m.nodes[i].own, vec![None; pages as usize]);
                        let old_parent = m.nodes[i].parent;
                        m.nodes.push(Node {
                            own,
                            parent: old_parent,
                            job,
                            id: None,
                            alive: true,
                        });
                        m.nodes[i].parent = Some((h, 0));
                        m.nodes.push(Node {
                            own: vec![None; len as usize],
                            parent: Some((h, off)),
                            job,
                            id: Some(child),
                            alive: true,
                        });
                    }
                }
                6 | 7 => {
                    // a PRIVATE child, any job
                    let i = pick(&mut rng);
                    let id = m.nodes[i].id.unwrap();
                    let pages = m.nodes[i].own.len() as u64;
                    let off = rng.below(pages);
                    let len = 1 + rng.below(pages - off);
                    let job = rng.below(2) as usize;
                    vmos.reserve_n(2).unwrap();
                    let r = vmos.create_child(
                        &mut jobs,
                        id,
                        off,
                        len,
                        job_ids[job],
                        ChildMode::Private,
                    );
                    if m.depth(i) + 1 >= MAX_FAMILY_DEPTH {
                        assert_eq!(r, Err(VmoError::TooDeep), "{ctx}");
                    } else {
                        let child = r.unwrap();
                        m.nodes.push(Node {
                            own: vec![None; len as usize],
                            parent: Some((i, off)),
                            job,
                            id: Some(child),
                            alive: true,
                        });
                    }
                }
                8 => {
                    // refused operations change nothing
                    let i = pick(&mut rng);
                    let id = m.nodes[i].id.unwrap();
                    let job = m.nodes[i].job;
                    let other = 1 - job;
                    vmos.reserve_n(2).unwrap();
                    assert_eq!(
                        vmos.create_child(&mut jobs, id, 0, 1, job_ids[other], ChildMode::Snapshot),
                        Err(VmoError::JobMismatch),
                        "{ctx}: a SNAPSHOT child is charged to the source's job"
                    );
                    let pages = m.nodes[i].own.len() as u64;
                    assert_eq!(
                        vmos.create_child(
                            &mut jobs,
                            id,
                            pages,
                            1,
                            job_ids[job],
                            ChildMode::Private
                        ),
                        Err(VmoError::OutOfRange)
                    );
                    if vmos.in_family(id) {
                        assert_eq!(
                            vmos.decommit(&mut mm, &mut jobs, id, 0, 1),
                            Err(VmoError::InFamily),
                            "{ctx}"
                        );
                    }
                }
                _ => {
                    // a member goes away (its handle closes)
                    if alive.len() > 2 {
                        let i = pick(&mut rng);
                        let id = m.nodes[i].id.unwrap();
                        vmos.release(&mut mm, &mut jobs, id).unwrap();
                        m.nodes[i].alive = false;
                    }
                }
            }
            // every member reads what the tree says; every frame in use is charged once
            for &i in m
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.alive && n.id.is_some())
                .map(|(i, _)| i)
                .collect::<Vec<_>>()
                .iter()
            {
                let id = m.nodes[i].id.unwrap();
                for p in 0..m.nodes[i].own.len() as u64 {
                    assert_eq!(
                        read_page(&mut mm, &mut vmos, id, p),
                        m.read(i, p),
                        "{ctx}: member {i} page {p}"
                    );
                }
            }
            assert_eq!(
                usage(&jobs, 0) + usage(&jobs, 1),
                (total - mm.db.free_count()) as u64,
                "{ctx}: every frame is charged to exactly one job"
            );
        }
        for n in m.nodes.iter().filter(|n| n.alive) {
            if let Some(id) = n.id {
                vmos.release(&mut mm, &mut jobs, id).unwrap();
            }
        }
        assert_eq!(
            vmos.count(),
            0,
            "seed {seed}: no VMO left, hidden nodes included"
        );
        assert_eq!(mm.db.free_count(), total, "seed {seed}: no frame left");
        assert_eq!(usage(&jobs, 0) + usage(&jobs, 1), 0, "seed {seed}");
        mm.db.audit().unwrap();
    }
}

#[test]
fn a_chain_of_children_is_at_most_64_deep_and_a_refusal_changes_nothing() {
    let mut mm = Mm {
        db: FrameDb::new(400, 100),
        mem: FakeMem::default(),
    };
    let mut vmos = Vmos::new();
    let mut jobs = Jobs::new();
    let job = jobs.create(JobId::ROOT, Limits::UNLIMITED).unwrap();
    let mut cur = vmos.create_in(&mut jobs, 2, job);
    vmos.write(&mut mm, &mut jobs, cur, 0, &[7]).unwrap();
    let mut made = vec![cur];
    let mut refused = false;
    for _ in 0..(MAX_FAMILY_DEPTH + 4) {
        vmos.reserve_n(2).unwrap();
        let before = vmos.count();
        match vmos.create_child(&mut jobs, cur, 0, 2, job, ChildMode::Private) {
            Ok(c) => {
                cur = c;
                made.push(c);
            }
            Err(e) => {
                assert_eq!(e, VmoError::TooDeep);
                assert_eq!(vmos.count(), before, "a refusal makes no VMO");
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "the 65th link is refused");
    assert_eq!(read_page(&mut mm, &mut vmos, cur, 0), 7);
    for id in made.into_iter().rev() {
        vmos.release(&mut mm, &mut jobs, id).unwrap();
    }
    assert_eq!(vmos.count(), 0);
}
