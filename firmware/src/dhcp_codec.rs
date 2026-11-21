//! Pure DHCP server packet parsing and OFFER/ACK reply builder (AP mode).

pub const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

/// Parses an incoming DHCP packet in `req` and, if it is DISCOVER (1) or
/// REQUEST (3), fills `reply` with an OFFER (2) or ACK (5) for `192.168.4.2`.
pub fn build_reply(req: &[u8], reply: &mut [u8; 300]) -> bool {
    if req.len() < 240 || req[236..240] != MAGIC_COOKIE {
        return false;
    }

    let xid = &req[4..8];
    let chaddr = &req[28..44];
    let mut msg_type = 0u8;

    let mut opt_ptr = 240;
    while opt_ptr < req.len() {
        let code = req[opt_ptr];
        if code == 255 {
            break;
        }
        if code == 0 {
            opt_ptr += 1;
            continue;
        }
        if opt_ptr + 1 >= req.len() {
            break;
        }
        let len = req[opt_ptr + 1] as usize;
        if code == 53 && len == 1 && opt_ptr + 2 < req.len() {
            msg_type = req[opt_ptr + 2];
        }
        opt_ptr = opt_ptr.saturating_add(2 + len);
    }

    if msg_type != 1 && msg_type != 3 {
        return false;
    }

    reply.fill(0);
    reply[0] = 2;
    reply[1] = 1;
    reply[2] = 6;
    reply[4..8].copy_from_slice(xid);
    reply[16..20].copy_from_slice(&[192, 168, 4, 2]);
    reply[20..24].copy_from_slice(&[192, 168, 4, 1]);
    reply[28..44].copy_from_slice(chaddr);
    reply[236..240].copy_from_slice(&MAGIC_COOKIE);

    let next_type = if msg_type == 1 { 2 } else { 5 };
    reply[240..243].copy_from_slice(&[53, 1, next_type]);
    reply[243..249].copy_from_slice(&[54, 4, 192, 168, 4, 1]);
    reply[249..255].copy_from_slice(&[51, 4, 0, 0, 14, 16]);
    reply[255..261].copy_from_slice(&[1, 4, 255, 255, 255, 0]);
    reply[261..267].copy_from_slice(&[3, 4, 192, 168, 4, 1]);
    reply[267] = 255;

    true
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/dhcp_codec.rs"]
mod tests;
