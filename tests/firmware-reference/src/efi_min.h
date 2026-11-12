/* Minimal UEFI 2.10 x64 ABI declarations, derived from the published interface.
 * This file has no host firmware operations. It is linked only into guest EFI apps. */
#ifndef EFI_MIN_H
#define EFI_MIN_H
typedef unsigned char U8;
typedef unsigned short U16;
typedef unsigned int U32;
typedef unsigned long long U64;
typedef U64 UINTN;
typedef U64 EFI_STATUS;
typedef void *EFI_HANDLE;
typedef U16 CHAR16;
typedef struct { U32 a; U16 b, c; U8 d[8]; } EFI_GUID;
typedef struct { U64 Signature; U32 Revision, HeaderSize, CRC32, Reserved; } EFI_TABLE_HEADER;
#define EFIERR(n) (0x8000000000000000ULL | (n))
#define EFI_SUCCESS 0
#define EFI_SECURITY_VIOLATION EFIERR(26)
#define EFI_ACCESS_DENIED EFIERR(15)
#define EFI_NOT_FOUND EFIERR(14)
#define EFI_VARIABLE_ATTRS 0x27
#define EFI_ERROR(s) (((s) >> 63) != 0)
typedef struct EFI_FILE EFI_FILE;
struct EFI_FILE {
    U64 Revision;
    EFI_STATUS (*Open)(EFI_FILE *, EFI_FILE **, CHAR16 *, U64, U64);
    EFI_STATUS (*Close)(EFI_FILE *);
    void *Delete;
    EFI_STATUS (*Read)(EFI_FILE *, UINTN *, void *);
    void *Write, *GetPosition, *SetPosition, *GetInfo, *SetInfo, *Flush;
};
typedef struct { U64 Revision; EFI_STATUS (*OpenVolume)(void *, EFI_FILE **); } EFI_SIMPLE_FILE_SYSTEM;
typedef struct { void *Reset; EFI_STATUS (*OutputString)(void *, CHAR16 *); } EFI_TEXT_OUTPUT;
typedef struct {
    EFI_TABLE_HEADER Hdr;
    void *RaiseTPL, *RestoreTPL, *AllocatePages, *FreePages, *GetMemoryMap;
    EFI_STATUS (*AllocatePool)(U32, UINTN, void **);
    EFI_STATUS (*FreePool)(void *);
    void *CreateEvent, *SetTimer, *WaitForEvent, *SignalEvent, *CloseEvent, *CheckEvent;
    void *InstallProtocolInterface, *ReinstallProtocolInterface, *UninstallProtocolInterface;
    EFI_STATUS (*HandleProtocol)(EFI_HANDLE, EFI_GUID *, void **);
    void *Reserved, *RegisterProtocolNotify, *LocateHandle, *LocateDevicePath, *InstallConfigurationTable;
    EFI_STATUS (*LoadImage)(U8, EFI_HANDLE, void *, void *, UINTN, EFI_HANDLE *);
    EFI_STATUS (*StartImage)(EFI_HANDLE, UINTN *, CHAR16 **);
    void *Exit;
    EFI_STATUS (*UnloadImage)(EFI_HANDLE);
    void *ExitBootServices, *GetNextMonotonicCount, *Stall, *SetWatchdogTimer;
} EFI_BOOT_SERVICES;
typedef struct {
    EFI_TABLE_HEADER Hdr;
    void *GetTime, *SetTime, *GetWakeupTime, *SetWakeupTime, *SetVirtualAddressMap, *ConvertPointer;
    EFI_STATUS (*GetVariable)(CHAR16 *, EFI_GUID *, U32 *, UINTN *, void *);
    void *GetNextVariableName;
    EFI_STATUS (*SetVariable)(CHAR16 *, EFI_GUID *, U32, UINTN, void *);
    void *GetNextHighMonotonicCount;
    void (*ResetSystem)(U32, EFI_STATUS, UINTN, void *);
} EFI_RUNTIME_SERVICES;
typedef struct {
    EFI_TABLE_HEADER Hdr;
    CHAR16 *FirmwareVendor; U32 FirmwareRevision;
    EFI_HANDLE ConsoleInHandle; void *ConIn;
    EFI_HANDLE ConsoleOutHandle; EFI_TEXT_OUTPUT *ConOut;
    EFI_HANDLE StandardErrorHandle; EFI_TEXT_OUTPUT *StdErr;
    EFI_RUNTIME_SERVICES *RuntimeServices; EFI_BOOT_SERVICES *BootServices;
    UINTN NumberOfTableEntries; void *ConfigurationTable;
} EFI_SYSTEM_TABLE;
typedef struct {
    U32 Revision; EFI_HANDLE ParentHandle; EFI_SYSTEM_TABLE *SystemTable;
    EFI_HANDLE DeviceHandle; void *FilePath, *Reserved;
    U32 LoadOptionsSize; void *LoadOptions, *ImageBase; U64 ImageSize;
    U32 ImageCodeType, ImageDataType; void *Unload;
} EFI_LOADED_IMAGE;
_Static_assert(sizeof(EFI_GUID)==16, "GUID");
_Static_assert(__builtin_offsetof(EFI_SYSTEM_TABLE,RuntimeServices)==88, "ST");
_Static_assert(__builtin_offsetof(EFI_BOOT_SERVICES,LoadImage)==200, "BS");
_Static_assert(__builtin_offsetof(EFI_RUNTIME_SERVICES,GetVariable)==72, "RT");
_Static_assert(__builtin_offsetof(EFI_LOADED_IMAGE,LoadOptions)==56, "LI");
static EFI_GUID GLOBAL_GUID={0x8be4df61,0x93ca,0x11d2,{0xaa,0x0d,0x00,0xe0,0x98,0x03,0x2b,0x8c}};
static EFI_GUID DB_GUID={0xd719b2cb,0x3d3a,0x4596,{0xa3,0xbc,0xda,0xd0,0x0e,0x67,0x65,0x6f}};
static EFI_GUID LOADED_IMAGE_GUID={0x5b1b31a1,0x9562,0x11d2,{0x8e,0x3f,0x00,0xa0,0xc9,0x69,0x72,0x3b}};
static EFI_GUID DEVICE_PATH_GUID={0x09576e91,0x6d3f,0x11d2,{0x8e,0x39,0x00,0xa0,0xc9,0x69,0x72,0x3b}};
static EFI_GUID FILE_SYSTEM_GUID={0x964e5b22,0x6459,0x11d2,{0x8e,0x39,0x00,0xa0,0xc9,0x69,0x72,0x3b}};
static void copy_bytes(void *dst, const void *src, UINTN n) { U8 *d=dst; const U8 *s=src; while(n--) *d++=*s++; }
static int equal_bytes(const void *a,const void *b,UINTN n) { const U8*x=a,*y=b; U8 diff=0; while(n--) diff|=*x++ ^ *y++; return diff==0; }
static void print_ascii(EFI_SYSTEM_TABLE *st,const char *s) {
    CHAR16 buf[128]; UINTN n=0;
    while(*s) { buf[n++]=(U8)*s++; if(n==127) { buf[n]=0; st->ConOut->OutputString(st->ConOut,buf); n=0; } }
    buf[n]=0; st->ConOut->OutputString(st->ConOut,buf);
}
#endif
