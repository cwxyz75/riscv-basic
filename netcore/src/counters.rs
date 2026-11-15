//! The counters (NET-SPEC 6.7): 64-bit, named, readable through `GetStats`.

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        /// Every counter of the core.
        #[derive(Default, Clone, Debug, PartialEq, Eq)]
        pub struct Counters { $(pub $name: u64,)* }

        impl Counters {
            /// `(name id, name, value)` in a fixed order: the id is the position, stable across builds of one version.
            pub fn pairs(&self) -> alloc::vec::Vec<(u16, &'static str, u64)> {
                let mut v = alloc::vec::Vec::new();
                let mut i = 0u16;
                $( v.push((i, stringify!($name), self.$name)); i += 1; )*
                let _ = i;
                v
            }
        }
    };
}

counters!(
    rx_frames,
    rx_runt,
    rx_not_for_us,
    rx_unknown_ethertype,
    rx_arp_malformed,
    rx_arp_unconfigured,
    rx_ip_malformed,
    rx_ip_checksum,
    rx_fragment,
    rx_ttl,
    rx_ip_not_for_us,
    rx_unknown_protocol,
    rx_icmp_malformed,
    rx_udp_malformed,
    icmp_echo_replies_sent,
    icmp_echo_rate_limited,
    icmp_echo_ignored,
    icmp_unmatched,
    icmp_unreachable_received,
    icmp_other,
    icmp_unreachable_sent,
    icmp_unreachable_rate_limited,
    udp_delivered,
    udp_no_port,
    arp_requests_sent,
    arp_replies_sent,
    arp_queue_dropped,
    arp_failed,
    arp_evictions,
    tx_frames,
    tx_link_down,
    tx_queue_full,
    tx_too_big,
    // S8 follow-up: a received destination-unreachable is routed to the transport it quotes (S9-SPEC 18.4)
    icmp_error_udp,
    icmp_error_tcp,
    icmp_error_other_protocol,
    icmp_error_malformed,
    // S9 step 4a: TCP in the core
    tcp_checksum,
    tcp_bad_header,
    tcp_no_connection,
    tcp_rst_sent,
    tcp_rst_rate_limited,
    tcp_listen_overflow,
    tcp_orphan_reset,
    tcp_loopback_dropped,
    tcp_tx_no_route,
);
