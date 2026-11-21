use super::*;

#[test]
fn flash_layout_and_profile_keys_are_consistent() {
    const {
        assert!(FS_RANGE.start < FS_RANGE.end);
        assert!(CYW43_FW_ADDR + CYW43_FW_LEN <= CYW43_CLM_ADDR);
    }
    assert_eq!(FS_RANGE.end as usize, FLASH_SIZE);
    assert_eq!((FS_RANGE.end - FS_RANGE.start) as usize, 64 * 1024);
    assert!(CYW43_FW_ADDR < 0x1000_0000 + FS_RANGE.start as usize);
    assert_eq!(profile_flash_key(0), Some(FLASH_KEY_PROFILE_BASE));
    assert_eq!(profile_flash_key(9), Some(FLASH_KEY_PROFILE_BASE + 9));
    assert_eq!(profile_flash_key(10), None);
}
