/* Load config for the ARM64X forwarder: the EC view points the loader at
 * the CHPE metadata, and the linker fills in the dynamic value relocation
 * table that switches an x64 process to the EC exports. */
#include <windows.h>

#ifdef _M_ARM64EC
extern const char __chpe_metadata;
#endif

const IMAGE_LOAD_CONFIG_DIRECTORY64 _load_config_used = {
   .Size = sizeof(IMAGE_LOAD_CONFIG_DIRECTORY64),
#ifdef _M_ARM64EC
   .CHPEMetadataPointer = (ULONGLONG)&__chpe_metadata,
#endif
};
