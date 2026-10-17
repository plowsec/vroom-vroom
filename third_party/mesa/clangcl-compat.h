/* MSVC intrinsics Mesa's u_atomic.h names in lowercase, which clang-cl's
 * intrin.h doesn't declare. Forced in with /FI. */
#pragma once
#if defined(__clang__) && defined(_MSC_VER) && !defined(RC_INVOKED) && !defined(__ASSEMBLER__)
#include <intrin.h>
static __inline long _interlockedadd(long volatile *p, long v)
{
   return _InterlockedExchangeAdd(p, v) + v;
}
static __inline __int64 _interlockedadd64(__int64 volatile *p, __int64 v)
{
   return _InterlockedExchangeAdd64(p, v) + v;
}
#define _interlockedincrement64 _InterlockedIncrement64
#define _interlockeddecrement64 _InterlockedDecrement64
#define _interlockedexchange64 _InterlockedExchange64
#define _interlockedexchangeadd64 _InterlockedExchangeAdd64
/* flex output (made by a non-Windows flex) calls isatty. */
#include <io.h>
#endif
