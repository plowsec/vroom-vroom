/* vroom_d3d: a wrapper around the Direct3D driver of viogpu3d (UTM's
 * Neptune UMD), registered in its place by the vroom agent.
 *
 * vroom turns the 3D display device off before a suspend or snapshot
 * (Direct3D state lives on the host and can't be saved). The kernel
 * driver then unmaps the shared memory the UMD talks to the host
 * through (its rings) from every process, while their UMD threads still
 * use it: each process with a Direct3D device crashed at its next draw
 * (Explorer, the Start menu, the text input host, DWM, apps).
 *
 * This DLL loads the real UMD (its path is in vroom_d3d.txt next to this
 * file, written by the agent), hands Direct3D its entry points, and adds
 * an exception handler: an access by the UMD to memory that is no longer
 * mapped gets fresh zeroed memory there instead. A ring's header starts a
 * page; in each page the handler sets where a ring's status would be to
 * "fatal", the host's own way of saying it is gone, so the UMD stops
 * waiting on it and fails the call. Windows reports the device removed to
 * the app at its next present, as for any lost GPU. Only accesses from the
 * UMD's code to unmapped memory are handled; anything else crashes as it
 * would have.
 *
 * Built for ARM64 and x64 (scripts/build-windows-d3d.sh); vroom_d3d.dll,
 * an ARM64X forwarder, sends each kind of process to its build. 32-bit
 * apps keep the stock driver. */
#include <windows.h>

#define RING_STATUS_OFFSET  128u     /* npt_ring_layout: head 0, tail 64, status 128 */
#define RING_STATUS_FATAL   (1u << 1) /* NPT_RING_STATUS_FATAL_BIT */
#define WINDOW              0x10000u  /* what VirtualAlloc places memory on */
#define PAGE                0x1000u

typedef HRESULT(APIENTRY *open_adapter_fn)(void *args);

static INIT_ONCE once = INIT_ONCE_STATIC_INIT;
static HMODULE self, umd;
static void *handler;
static open_adapter_fn open10, open12;
/* The UMD's code: faults elsewhere aren't ours to touch. */
static ULONG_PTR umd_lo, umd_hi;

static LONG CALLBACK
on_fault(EXCEPTION_POINTERS *e)
{
   const EXCEPTION_RECORD *r = e->ExceptionRecord;
   if (r->ExceptionCode != EXCEPTION_ACCESS_VIOLATION || r->NumberParameters < 2)
      return EXCEPTION_CONTINUE_SEARCH;
   const ULONG_PTR pc = (ULONG_PTR)r->ExceptionAddress;
   if (pc < umd_lo || pc >= umd_hi)
      return EXCEPTION_CONTINUE_SEARCH;
   const ULONG_PTR a = r->ExceptionInformation[1];
   const ULONG_PTR base = a & ~(ULONG_PTR)(WINDOW - 1);
   MEMORY_BASIC_INFORMATION mi;
   if (!VirtualQuery((void *)a, &mi, sizeof mi))
      return EXCEPTION_CONTINUE_SEARCH;
   if (mi.State == MEM_COMMIT && mi.Type == MEM_PRIVATE && mi.Protect == PAGE_READWRITE)
      return EXCEPTION_CONTINUE_EXECUTION; /* another thread got here first */
   /* Seen once after a boot: the ring still listed as a mapped view, its
    * pages gone. A view that faults is dead: take it out. */
   if (mi.State == MEM_COMMIT && mi.Type == MEM_MAPPED && UnmapViewOfFile(mi.AllocationBase))
      return EXCEPTION_CONTINUE_EXECUTION; /* faults again, on free memory */
   if (mi.State != MEM_FREE)
      return EXCEPTION_CONTINUE_SEARCH;
   /* Memory starts on 64 KiB, but the next mapping may follow closer
    * than that: take what is free from the window's start. */
   if (!VirtualQuery((void *)base, &mi, sizeof mi) || mi.State != MEM_FREE)
      return EXCEPTION_CONTINUE_SEARCH;
   const SIZE_T size = mi.RegionSize < WINDOW ? mi.RegionSize : WINDOW;
   if (a - base >= size)
      return EXCEPTION_CONTINUE_SEARCH;
   void *p = VirtualAlloc((void *)base, size, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
   if (!p) {
      /* Lost a race with another thread mapping the same window. */
      if (VirtualQuery((void *)a, &mi, sizeof mi) && mi.State == MEM_COMMIT && mi.Type == MEM_PRIVATE)
         return EXCEPTION_CONTINUE_EXECUTION;
      return EXCEPTION_CONTINUE_SEARCH;
   }
   /* A ring's header starts a page: mark each page's would-be status. */
   for (SIZE_T off = 0; off < size; off += PAGE)
      ((volatile UINT32 *)((char *)p + off))[RING_STATUS_OFFSET / 4] = RING_STATUS_FATAL;
   return EXCEPTION_CONTINUE_EXECUTION;
}

static BOOL CALLBACK
load(INIT_ONCE *o, void *param, void **ctx)
{
   (void)o, (void)param, (void)ctx;
   /* The real driver's path, from vroom_d3d.txt next to us (UTF-8 or
    * ASCII, one line). */
   wchar_t path[MAX_PATH];
   DWORD n = GetModuleFileNameW(self, path, MAX_PATH);
   if (!n || n >= MAX_PATH)
      return TRUE;
   wchar_t *slash = path + n;
   while (slash > path && slash[-1] != L'\\')
      slash--;
   if ((size_t)(slash - path) + 16 >= MAX_PATH)
      return TRUE;
   lstrcpyW(slash, L"vroom_d3d.txt");
   HANDLE f = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_DELETE, NULL, OPEN_EXISTING, 0, NULL);
   if (f == INVALID_HANDLE_VALUE)
      return TRUE;
   char buf[MAX_PATH * 3];
   DWORD got = 0;
   BOOL ok = ReadFile(f, buf, sizeof buf - 1, &got, NULL);
   CloseHandle(f);
   if (!ok)
      return TRUE;
   while (got && (buf[got - 1] == '\n' || buf[got - 1] == '\r' || buf[got - 1] == ' '))
      got--;
   buf[got] = 0;
   wchar_t path_of_umd[MAX_PATH];
   if (!got || !MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, buf, -1, path_of_umd, MAX_PATH))
      return TRUE;
   umd = LoadLibraryExW(path_of_umd, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
   if (!umd)
      return TRUE;
   open10 = (open_adapter_fn)(void *)GetProcAddress(umd, "OpenAdapter10_2");
   open12 = (open_adapter_fn)(void *)GetProcAddress(umd, "OpenAdapter12");
   /* The stock DLL is an ARM64X forwarder: the code is in the module the
    * export resolved to. */
   HMODULE code = NULL;
   if (open10 && GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                    (LPCWSTR)(void *)open10, &code)) {
      const IMAGE_DOS_HEADER *dos = (const IMAGE_DOS_HEADER *)code;
      const IMAGE_NT_HEADERS *nt = (const IMAGE_NT_HEADERS *)((const char *)code + dos->e_lfanew);
      umd_lo = (ULONG_PTR)code;
      umd_hi = umd_lo + nt->OptionalHeader.SizeOfImage;
      handler = AddVectoredExceptionHandler(1, on_fault);
   }
   return TRUE;
}

__declspec(dllexport) HRESULT APIENTRY
OpenAdapter10_2(void *args)
{
   InitOnceExecuteOnce(&once, load, NULL, NULL);
   return open10 ? open10(args) : E_FAIL;
}

__declspec(dllexport) HRESULT APIENTRY
OpenAdapter12(void *args)
{
   InitOnceExecuteOnce(&once, load, NULL, NULL);
   return open12 ? open12(args) : E_FAIL;
}

BOOL WINAPI
DllMain(HINSTANCE h, DWORD reason, void *reserved)
{
   if (reason == DLL_PROCESS_ATTACH) {
      self = h;
      DisableThreadLibraryCalls(h);
   } else if (reason == DLL_PROCESS_DETACH && !reserved) {
      /* Direct3D let go of the adapter (it does when the device goes
       * away): the handler must not outlive this code, and the real
       * driver goes with us, as it would have without us. FreeLibrary
       * can't run under the loader lock: a worker thread calls it. */
      if (handler)
         RemoveVectoredExceptionHandler(handler);
      if (umd)
         QueueUserWorkItem((LPTHREAD_START_ROUTINE)(void *)FreeLibrary, umd, WT_EXECUTEDEFAULT);
   }
   return TRUE;
}
