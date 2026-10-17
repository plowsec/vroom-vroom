// Small, non-exploit client used to prepare an HEVD call-site checkpoint.
// Build inside the Windows guest with: cl /W4 /DUNICODE /D_UNICODE client.c

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#define HEVD_IOCTL(function) CTL_CODE(FILE_DEVICE_UNKNOWN, (function), METHOD_NEITHER, FILE_ANY_ACCESS)

int wmain(int argc, wchar_t **argv) {
    DWORD ioctl = HEVD_IOCTL(0x800);
    DWORD length = 0x1000;
    if (argc > 1) {
        ioctl = (DWORD)wcstoul(argv[1], NULL, 0);
    }
    if (argc > 2) {
        length = (DWORD)wcstoul(argv[2], NULL, 0);
    }
    if (length == 0 || length > 0x100000) {
        fputs("length must be in 1..0x100000\n", stderr);
        return 2;
    }

    HANDLE device = CreateFileW(
        L"\\\\.\\HackSysExtremeVulnerableDriver",
        GENERIC_READ | GENERIC_WRITE,
        0, NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
    if (device == INVALID_HANDLE_VALUE) {
        fprintf(stderr, "CreateFileW failed: %lu\n", GetLastError());
        return 1;
    }
    BYTE *buffer = (BYTE *)HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, length);
    if (buffer == NULL) {
        CloseHandle(device);
        return 1;
    }
    DWORD returned = 0;
    BOOL ok = DeviceIoControl(device, ioctl, buffer, length, buffer, length, &returned, NULL);
    printf("ioctl=0x%08lx ok=%d status=%lu returned=%lu\n",
           (unsigned long)ioctl, ok != FALSE, (unsigned long)GetLastError(),
           (unsigned long)returned);
    SecureZeroMemory(buffer, length);
    HeapFree(GetProcessHeap(), 0, buffer);
    CloseHandle(device);
    return ok ? 0 : 1;
}
