use super::*;

fn make_dhcp(msg_type: u8) -> [u8; 244] {
    let mut pkt = [0u8; 244];
    pkt[4..8].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]); // xid
    pkt[28..34].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]); // chaddr
    pkt[236..240].copy_from_slice(&MAGIC_COOKIE);
    pkt[240..243].copy_from_slice(&[53, 1, msg_type]);
    pkt[243] = 255;
    pkt
}

#[test]
fn builds_offer_for_discover_and_ack_for_request() {
    let mut reply = [0u8; 300];
    assert!(build_reply(&make_dhcp(1), &mut reply));
    assert_eq!(reply[0], 2);
    assert_eq!(&reply[4..8], &[0x12, 0x34, 0x56, 0x78]);
    assert_eq!(&reply[16..20], &[192, 168, 4, 2]);
    assert_eq!(&reply[240..243], &[53, 1, 2]); // DHCPOFFER

    assert!(build_reply(&make_dhcp(3), &mut reply));
    assert_eq!(&reply[240..243], &[53, 1, 5]); // DHCPACK

    // Pad option (code == 0) before option 53 and trailing truncated option byte
    let mut padded = [0u8; 246];
    padded[4..8].copy_from_slice(&[1, 2, 3, 4]);
    padded[236..240].copy_from_slice(&MAGIC_COOKIE);
    padded[240] = 0; // PAD
    padded[241] = 0; // PAD
    padded[242..245].copy_from_slice(&[53, 1, 1]);
    padded[245] = 12; // truncated option without length byte at end of packet
    assert!(build_reply(&padded, &mut reply));
    assert_eq!(&reply[240..243], &[53, 1, 2]);
}

#[test]
fn ignores_short_wrong_cookie_and_other_types() {
    let mut reply = [0u8; 300];
    assert!(!build_reply(&[0u8; 100], &mut reply));

    let mut bad_cookie = make_dhcp(1);
    bad_cookie[236] = 0;
    assert!(!build_reply(&bad_cookie, &mut reply));

    assert!(!build_reply(&make_dhcp(4), &mut reply)); // DECLINE ignored
}
