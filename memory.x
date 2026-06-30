MEMORY
{
/* RP2350B, W25Q128JVxQ: Total Flash is 16384K. We reserve 256K for WiFi FW and 64K for NVM at the end.
   Application Flash: 16384K - 256K - 64K = 16064K. No BOOT2 section — RP2350 bootrom handles flash init. */
FLASH : ORIGIN = 0x10000000, LENGTH = 16064K
RAM   : ORIGIN = 0x20000000, LENGTH = 520K
}

_stack_start = ORIGIN(RAM) + LENGTH(RAM);
_stack_end = _stack_start - 32K;
