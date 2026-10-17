/* The forwarder's entry point, one per view: nothing to set up. */
#include <windows.h>
BOOL WINAPI vroom_gl_entry(HINSTANCE h, DWORD reason, LPVOID reserved)
{
   (void)h; (void)reason; (void)reserved;
   return TRUE;
}
