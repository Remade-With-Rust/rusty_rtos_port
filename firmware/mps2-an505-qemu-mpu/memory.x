/* QEMU mps2-an505 (Cortex-M33, IoTKit), Secure aliases. The core resets
   Secure with its vector table at 0x10000000 (SSRAM1 seen through the IDAU's
   Secure alias); SSRAM2/3 are data, Secure alias 0x38000000. */
MEMORY
{
  FLASH : ORIGIN = 0x10000000, LENGTH = 4M
  RAM   : ORIGIN = 0x38000000, LENGTH = 2M
}
