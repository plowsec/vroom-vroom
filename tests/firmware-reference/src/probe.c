#include "efi_min.h"
EFI_STATUS efi_main(EFI_HANDLE image,EFI_SYSTEM_TABLE*st){
 (void)image;
 print_ascii(st,"VROOM_SIGNED_PROBE_EXECUTED\r\n");
 return EFI_SUCCESS;
}
