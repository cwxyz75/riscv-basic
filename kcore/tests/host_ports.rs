//! T195: the host fixtures' ports a boot is told on its kernel line (`hostports=<tcp>,<dns>,<http>`), as `debug_stat` 15 packs them.
use kcore::testlog::{host_port, host_ports};

#[test]
fn three_ports_are_packed_and_read_back() {
    let p = host_ports("47802,47803,47804");
    assert_eq!(p, 47802 | (47803 << 16) | (47804 << 32));
    assert_eq!(host_port(p, 0), Some(47802));
    assert_eq!(host_port(p, 1), Some(47803));
    assert_eq!(host_port(p, 2), Some(47804));
    let q = host_ports("1,65535,50000");
    assert_eq!(
        (host_port(q, 0), host_port(q, 1), host_port(q, 2)),
        (Some(1), Some(65535), Some(50000))
    );
}

#[test]
fn anything_but_three_ports_is_no_fixtures() {
    for bad in [
        "",
        "47802",
        "47802,47803",
        "47802,47803,47804,1",
        "0,47803,47804",
        "47802,0,47804",
        "47802,47803,0",
        "65536,1,1",
        "a,b,c",
        "47802,,47804",
        " 47802,47803,47804",
        "-1,2,3",
    ] {
        assert_eq!(host_ports(bad), 0, "{bad:?}");
    }
    for which in 0..3 {
        assert_eq!(host_port(0, which), None);
    }
}
